//! Динамический диск против эталонной модели: случайные записи, чтения,
//! TRIM, обнуления, проходы очистки и обслуживание (упаковка видеопамяти,
//! переезд блоков, возврат кусков), в том числе при отказах хранилищ.
//!
//! Эталон — массив байт и признак «блок занимает память» для каждого блока,
//! поэтому проверяется не только содержимое, но и сколько памяти держит диск.
//!
//! Настоящая видеопамять проверяется на программном Vulkan (lavapipe), если он
//! есть: `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json cargo test`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vramdisk::disk::{free_blocks, DiskError, FreeHint, HybridDisk, Reclaim, IDLE};
use vramdisk::pool::{self_test, Kind, NoRoom, SimStore, Slot, Store, BLOCK, REAP_DELAY};
use vramdisk::ram::RamStore;
use vramdisk::stats::Stats;

const MIB: u64 = 1 << 20;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Хранилище, которым тест управляет и после передачи диску.
#[derive(Clone)]
struct Shared(Arc<Mutex<SimStore>>);

impl Shared {
    fn new(s: SimStore) -> Self {
        Self(Arc::new(Mutex::new(s)))
    }
    fn set_refuse(&self, r: Option<NoRoom>) {
        self.0.lock().unwrap().refuse = r;
    }
    fn live_chunks(&self) -> usize {
        self.0.lock().unwrap().live_chunks()
    }
}

impl Store for Shared {
    fn kind(&self) -> Kind {
        self.0.lock().unwrap().kind()
    }
    fn limit(&self) -> u64 {
        self.0.lock().unwrap().limit()
    }
    fn alloc(&mut self, over: bool) -> Result<Slot, NoRoom> {
        self.0.lock().unwrap().alloc(over)
    }
    fn free(&mut self, slot: u64) {
        self.0.lock().unwrap().free(slot)
    }
    fn used(&self) -> u64 {
        self.0.lock().unwrap().used()
    }
    fn held(&self) -> u64 {
        self.0.lock().unwrap().held()
    }
    fn read(&mut self, runs: &[(u64, u64, usize)], out: &mut [u8]) -> Result<(), DiskError> {
        self.0.lock().unwrap().read(runs, out)
    }
    fn write(&mut self, fills: &[(u64, u64)], runs: &[(u64, u64, usize)], data: &[u8]) -> Result<(), DiskError> {
        self.0.lock().unwrap().write(fills, runs, data)
    }
    fn flush(&mut self) -> Result<(), DiskError> {
        self.0.lock().unwrap().flush()
    }
    fn max_io(&self) -> usize {
        self.0.lock().unwrap().max_io()
    }
    fn copy(&mut self, moves: &[(u64, u64)]) -> Result<(), DiskError> {
        self.0.lock().unwrap().copy(moves)
    }
    fn reap(&mut self, now: Instant, force: bool) -> u64 {
        self.0.lock().unwrap().reap(now, force)
    }
    fn plan_compaction(&mut self, max: usize) -> Vec<(u64, u64)> {
        self.0.lock().unwrap().plan_compaction(max)
    }
}

/// Эталон диска.
struct Model {
    data: Vec<u8>,
    /// Блок занимает память.
    mapped: Vec<bool>,
    /// Номер прохода очистки при последней записи в блок.
    gen: Vec<u32>,
    pass: u32,
}

impl Model {
    fn new(size: u64) -> Self {
        let n = size.div_ceil(BLOCK) as usize;
        Self { data: vec![0; size as usize], mapped: vec![false; n], gen: vec![0; n], pass: 0 }
    }
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn block(&self, b: usize) -> (u64, u64) {
        let s = b as u64 * BLOCK;
        (s, (s + BLOCK).min(self.size()))
    }
    /// Пересечения отрезка с блоками: (блок, начало, конец).
    fn pieces(&self, off: u64, len: u64) -> Vec<(usize, u64, u64)> {
        let mut out = Vec::new();
        let mut p = off;
        while p < off + len {
            let b = (p / BLOCK) as usize;
            let e = self.block(b).1.min(off + len);
            out.push((b, p, e));
            p = e;
        }
        out
    }
    fn clear(&mut self, b: usize) {
        let (s, e) = self.block(b);
        self.data[s as usize..e as usize].fill(0);
        self.mapped[b] = false;
    }
    fn write(&mut self, off: u64, d: &[u8]) {
        for (b, s, e) in self.pieces(off, d.len() as u64) {
            let zero = d[(s - off) as usize..(e - off) as usize].iter().all(|&x| x == 0);
            let whole = (s, e) == self.block(b);
            self.gen[b] = self.pass;
            if !self.mapped[b] {
                self.mapped[b] = !zero;
            } else if zero && whole {
                self.mapped[b] = false;
            }
        }
        self.data[off as usize..off as usize + d.len()].copy_from_slice(d);
    }
    fn zero(&mut self, off: u64, len: u64) {
        for (b, s, e) in self.pieces(off, len) {
            self.gen[b] = self.pass;
            if (s, e) == self.block(b) {
                self.mapped[b] = false;
            }
        }
        self.data[off as usize..(off + len) as usize].fill(0);
    }
    /// TRIM освобождает только целые блоки (и хвостовой неполный блок диска).
    fn trim(&mut self, off: u64, len: u64) {
        let len = len.min(self.size().saturating_sub(off));
        if len == 0 {
            return;
        }
        let end = off + len;
        let first = off.div_ceil(BLOCK) as usize;
        let last = if end == self.size() { self.mapped.len() } else { (end / BLOCK) as usize };
        for b in first..last.max(first) {
            self.clear(b);
        }
    }
    fn reclaim(&mut self, pass: u32, bits: &[u64]) -> u64 {
        let mut freed = 0;
        for b in 0..self.mapped.len() {
            if bits[b / 64] >> (b % 64) & 1 == 1 && self.mapped[b] && self.gen[b] < pass {
                let (s, e) = self.block(b);
                freed += e - s;
                self.clear(b);
            }
        }
        freed
    }
    fn mapped_count(&self) -> u64 {
        self.mapped.iter().filter(|&&m| m).count() as u64
    }
}

/// Диск, эталон и рычаги управления хранилищами.
struct Rig {
    disk: HybridDisk,
    m: Model,
    rc: Reclaim,
    /// Хранилища, которым можно устраивать отказы.
    levers: Vec<Shared>,
    refused: Option<usize>,
    rng: Rng,
    /// Сколько раз за прогон диск выходил за предел хранилища.
    overs: usize,
}

impl Rig {
    fn new(mut disk: HybridDisk, levers: Vec<Shared>, seed: u64) -> Self {
        let rc = disk.reclaim();
        let m = Model::new(disk.size());
        Self { disk, m, rc, levers, refused: None, rng: Rng(seed), overs: 0 }
    }

    fn check(&self, step: usize) {
        let s = self.disk.stats().snapshot();
        assert_eq!(self.disk.mapped(), self.m.mapped_count(), "шаг {step}: блоков с памятью");
        assert_eq!(s.used_ram + s.used_vram, self.m.mapped_count() * BLOCK, "шаг {step}: учёт занятого");
    }

    fn compare(&mut self, off: u64, len: u64, what: &str) {
        let mut out = vec![0x77u8; len as usize];
        self.disk.read(off, &mut out).unwrap();
        let want = &self.m.data[off as usize..(off + len) as usize];
        if out[..] != want[..] {
            let bad = out.iter().zip(want).position(|(a, b)| a != b).unwrap();
            panic!("{what}: чтение {off}+{len} расходится с эталоном в байте {}", off + bad as u64);
        }
    }

    fn compare_all(&mut self, what: &str) {
        let size = self.disk.size();
        let mut pos = 0;
        while pos < size {
            let l = (size - pos).min(MIB);
            self.compare(pos, l, what);
            pos += l;
        }
    }

    fn random_range(&mut self) -> (u64, u64) {
        let size = self.disk.size();
        let off = match self.rng.below(4) {
            0 => self.rng.below(size.div_ceil(BLOCK)) * BLOCK,
            1 => (self.rng.below(size.div_ceil(BLOCK)) * BLOCK).saturating_sub(1 + self.rng.below(300)),
            _ => self.rng.below(size),
        }
        .min(size - 1);
        // Иногда — большие куски: диск заполняется, хранилища упираются в пределы.
        let max = match self.rng.below(16) {
            0 => size / 2,
            1 | 2 => 3 * MIB,
            _ => 700 * 1024,
        };
        (off, (1 + self.rng.below(max)).min(size - off))
    }

    fn write_random(&mut self) {
        let (off, len) = self.random_range();
        let data: Vec<u8> = if self.rng.below(8) == 0 {
            vec![0; len as usize]
        } else {
            let fill = (self.rng.next() & 0xFF) as u8 | 1;
            (0..len).map(|i| fill.wrapping_add((i / 7) as u8)).collect()
        };
        self.disk.write(off, &data).unwrap();
        self.m.write(off, &data);
    }

    /// Проход очистки: пока он идёт, файловая система ещё пишет.
    fn reclaim_pass(&mut self) {
        let pass = self.rc.begin();
        self.m.pass = pass;
        for _ in 0..self.rng.below(4) {
            self.write_random();
        }
        let n = self.m.mapped.len();
        let mut bits = vec![0u64; n.div_ceil(64)];
        // Свободным файловая система считает от 0 до 3/16 блоков.
        let density = self.rng.below(4);
        for b in 0..n {
            if self.rng.below(16) < density {
                bits[b / 64] |= 1 << (b % 64);
            }
        }
        let before = self.disk.stats().snapshot().reclaimed;
        self.rc.tx.send(FreeHint { pass, bits: bits.clone() }).unwrap();
        self.disk.maintain(Instant::now());
        let freed = self.m.reclaim(pass, &bits);
        assert_eq!(self.disk.stats().snapshot().reclaimed - before, freed, "возвращено проходом очистки");
    }

    fn toggle_refusal(&mut self) {
        if self.levers.is_empty() {
            return;
        }
        match self.refused.take() {
            Some(i) => self.levers[i].set_refuse(None),
            None => {
                // Отказывает только одно хранилище: тогда запись не должна сбоить.
                let i = self.rng.below(self.levers.len() as u64) as usize;
                let why = if self.rng.below(2) == 0 {
                    NoRoom::Budget("видеопамять занята другими программами".into())
                } else {
                    NoRoom::Memory("система не дала памяти".into())
                };
                self.levers[i].set_refuse(Some(why));
                self.refused = Some(i);
            }
        }
    }

    fn run(&mut self, ops: usize) {
        for step in 0..ops {
            match self.rng.below(20) {
                0..=7 => self.write_random(),
                8..=11 => {
                    let (off, len) = self.random_range();
                    self.compare(off, len, &format!("шаг {step}"));
                }
                12 => {
                    let (off, len) = self.random_range();
                    self.disk.trim(off, len).unwrap();
                    self.m.trim(off, len);
                }
                13 => {
                    let (off, len) = self.random_range();
                    self.disk.zero(off, len).unwrap();
                    self.m.zero(off, len);
                }
                14 => self.disk.maintain(Instant::now()),
                // Простой: упаковка и переезд блоков.
                15 => self.disk.maintain(Instant::now() + IDLE + Duration::from_secs(1)),
                // Долгий простой: ещё и возврат опустевших кусков.
                16 => self.disk.maintain(Instant::now() + REAP_DELAY + Duration::from_secs(1)),
                17 => self.reclaim_pass(),
                18 => self.toggle_refusal(),
                _ => self.disk.flush().unwrap(),
            }
            self.check(step);
            let s = self.disk.stats().snapshot();
            self.overs += (s.over_ram + s.over_vram > 0) as usize;
            if step % 50 == 0 {
                self.disk.verify().unwrap_or_else(|e| panic!("шаг {step}: {e}"));
            }
        }
        if let Some(i) = self.refused.take() {
            self.levers[i].set_refuse(None);
        }
        self.disk.flush().unwrap();
        self.disk.verify().unwrap();
        self.compare_all("итог");
        let s = self.disk.stats().snapshot();
        eprintln!(
            "шагов {ops}: сверх предела {} раз, переехало {} КиБ, освобождено TRIM {} КиБ, очисткой {} КиБ",
            self.overs,
            s.moved / 1024,
            s.trimmed / 1024,
            s.reclaimed / 1024
        );
    }

    /// После отказов и простоя всё возвращается в пределы, а после очистки
    /// диска память отдаётся целиком.
    fn settle(&mut self) {
        let mut rounds = 0;
        loop {
            self.disk.maintain(Instant::now() + REAP_DELAY + Duration::from_secs(1));
            let s = self.disk.stats().snapshot();
            if s.over_ram == 0 && s.over_vram == 0 {
                break;
            }
            rounds += 1;
            assert!(rounds < 100, "диск не вернулся в пределы: сверх ОЗУ {}, сверх видеопамяти {}", s.over_ram, s.over_vram);
        }
        self.disk.verify().unwrap();
        self.compare_all("после переезда");
        let size = self.disk.size();
        self.disk.trim(0, size).unwrap();
        self.m.trim(0, size);
        self.disk.maintain(Instant::now() + REAP_DELAY + Duration::from_secs(1));
        let s = self.disk.stats().snapshot();
        assert_eq!((s.used_ram, s.used_vram, s.held_ram, s.held_vram), (0, 0, 0, 0), "после очистки память возвращена");
        self.compare_all("после очистки");
    }
}

fn stats() -> Arc<Stats> {
    Arc::new(Stats::default())
}

fn sim(kind: Kind, size: u64, limit: u64, chunk: u64, max_io: usize) -> Shared {
    let mut s = SimStore::new(kind, size, limit, chunk);
    s.max_io = max_io;
    Shared::new(s)
}

#[test]
fn model_ram_and_vram() {
    // Размеры не кратны блоку и куску: проверяется и неполный хвост диска.
    for (seed, first, ram, vram) in [(0x1234_5678u64, Kind::Ram, 5 * MIB + 12345, 9 * MIB), (77, Kind::Vram, 3 * MIB, 9 * MIB + 4321)] {
        let size = ram + vram;
        let mut d = HybridDisk::new(size, stats());
        d.add(Box::new(RamStore::new(size, ram).unwrap()));
        let v = sim(Kind::Vram, size, vram, 4 * MIB, 256 * 1024);
        d.add(Box::new(v.clone()));
        d.set_first(first);
        let mut rig = Rig::new(d, vec![v], seed);
        rig.run(3000);
        assert!(rig.overs > 0 && rig.disk.stats().snapshot().moved > 0, "прогон должен задеть отказы и переезд");
        rig.settle();
    }
}

#[test]
fn model_both_can_refuse() {
    let (ram, vram) = (8 * MIB, 8 * MIB);
    let size = ram + vram;
    let mut d = HybridDisk::new(size, stats());
    let r = sim(Kind::Ram, size, ram, 4 * MIB, usize::MAX);
    let v = sim(Kind::Vram, size, vram, 4 * MIB, 64 * 1024);
    d.add(Box::new(r.clone()));
    d.add(Box::new(v.clone()));
    let mut rig = Rig::new(d, vec![r, v], 0xDEAD_BEEF);
    rig.run(3000);
    assert!(rig.overs > 0 && rig.disk.stats().snapshot().moved > 0, "прогон должен задеть отказы и переезд");
    rig.settle();
}

#[test]
fn model_ram_only() {
    let size = 6 * MIB;
    let mut d = HybridDisk::new(size, stats());
    d.add(Box::new(RamStore::new(size, size).unwrap()));
    let mut rig = Rig::new(d, Vec::new(), 99);
    rig.run(2000);
    rig.settle();
}

#[test]
fn fill_order_and_limits() {
    let size = 16 * MIB;
    let mut d = HybridDisk::new(size, stats());
    d.add(Box::new(RamStore::new(size, 4 * MIB).unwrap()));
    let v = sim(Kind::Vram, size, 12 * MIB, 4 * MIB, 256 * 1024);
    d.add(Box::new(v.clone()));
    // Пустой диск памяти не занимает.
    let s = d.stats().snapshot();
    assert_eq!((s.held_ram, s.held_vram, s.max_ram, s.max_vram), (0, 0, 4 * MIB, 12 * MIB));
    // Сначала оперативная память до предела, затем видеопамять.
    d.write(0, &vec![1; 6 * MIB as usize]).unwrap();
    let s = d.stats().snapshot();
    assert_eq!((s.used_ram, s.used_vram, s.held_vram), (4 * MIB, 2 * MIB, 4 * MIB));
    // Видеопамять первой: новое ложится туда.
    d.set_first(Kind::Vram);
    d.write(8 * MIB, &vec![2; 4 * MIB as usize]).unwrap();
    let s = d.stats().snapshot();
    assert_eq!((s.used_ram, s.used_vram, s.held_vram), (4 * MIB, 6 * MIB, 8 * MIB));
    assert_eq!(v.live_chunks(), 2);
    assert_eq!((s.over_ram, s.over_vram, s.nomem), (0, 0, 0));
    d.verify().unwrap();
}

#[test]
fn vram_refusal_spills_over_and_moves_back() {
    let size = 16 * MIB;
    let mut d = HybridDisk::new(size, stats());
    d.add(Box::new(RamStore::new(size, 8 * MIB).unwrap()));
    let v = sim(Kind::Vram, size, 8 * MIB, 4 * MIB, 256 * 1024);
    d.add(Box::new(v.clone()));
    d.set_first(Kind::Vram);
    // Видеопамять заняла игра: всё ложится в оперативную память, сверх её предела.
    v.set_refuse(Some(NoRoom::Budget("видеопамять занята другими программами".into())));
    let data: Vec<u8> = (0..12 * MIB).map(|i| (i % 251) as u8 + 1).collect();
    d.write(0, &data).unwrap();
    let s = d.stats().snapshot();
    assert_eq!((s.used_ram, s.used_vram, s.over_ram, s.nomem), (12 * MIB, 0, 4 * MIB, 0));
    // Пока видеопамять занята, простой ничего не меняет.
    d.maintain(Instant::now() + REAP_DELAY + Duration::from_secs(1));
    assert_eq!(d.stats().snapshot().over_ram, 4 * MIB);
    // Игра закрылась: лишнее переезжает в видеопамять.
    v.set_refuse(None);
    d.maintain(Instant::now() + IDLE + Duration::from_secs(1));
    let s = d.stats().snapshot();
    assert_eq!((s.used_ram, s.used_vram, s.over_ram, s.moved), (8 * MIB, 4 * MIB, 0, 4 * MIB));
    d.verify().unwrap();
    let mut back = vec![0u8; data.len()];
    d.read(0, &mut back).unwrap();
    assert!(back == data, "после переезда данные те же");
}

#[test]
fn both_refuse_reports_error() {
    let size = 8 * MIB;
    let mut d = HybridDisk::new(size, stats());
    let r = sim(Kind::Ram, size, 4 * MIB, 4 * MIB, usize::MAX);
    let v = sim(Kind::Vram, size, 4 * MIB, 4 * MIB, usize::MAX);
    d.add(Box::new(r.clone()));
    d.add(Box::new(v.clone()));
    r.set_refuse(Some(NoRoom::Memory("система не дала оперативной памяти".into())));
    v.set_refuse(Some(NoRoom::Budget("видеопамять занята другими программами".into())));
    match d.write(0, &[7; 100]) {
        Err(DiskError::Device(m)) => {
            assert!(m.contains("не хватило памяти"), "{m}");
            assert!(m.contains("система не дала") && m.contains("видеопамять занята"), "{m}");
        }
        other => panic!("ожидалась ошибка нехватки памяти, а получено {other:?}"),
    }
    let s = d.stats().snapshot();
    assert_eq!((s.nomem, d.mapped()), (1, 0));
    assert!(d.stats().take_error().is_some_and(|e| e.contains("не хватило памяти")));
    // Ошибка не портит диск: место появилось — запись проходит.
    v.set_refuse(None);
    d.write(0, &[7; 100]).unwrap();
    let mut out = [0u8; 200];
    d.read(0, &mut out).unwrap();
    assert!(out[..100].iter().all(|&b| b == 7) && out[100..].iter().all(|&b| b == 0));
    d.verify().unwrap();
}

#[test]
fn compaction_returns_vram_chunks() {
    let size = 32 * MIB;
    let mut d = HybridDisk::new(size, stats());
    let v = sim(Kind::Vram, size, size, 4 * MIB, 256 * 1024);
    d.add(Box::new(v.clone()));
    let data: Vec<u8> = (0..16 * MIB).map(|i| (i % 253) as u8 | 1).collect();
    d.write(0, &data).unwrap();
    assert_eq!(v.live_chunks(), 4);
    // Файлы удалены почти везде: в каждом куске осталось по несколько блоков.
    let mut model = data.clone();
    for b in 0..256u64 {
        if b % 16 != 0 {
            d.trim(b * BLOCK, BLOCK).unwrap();
            model[(b * BLOCK) as usize..((b + 1) * BLOCK) as usize].fill(0);
        }
    }
    assert_eq!(v.live_chunks(), 4, "куски с данными остаются");
    // Под нагрузкой упаковки нет.
    d.maintain(Instant::now());
    assert_eq!(d.stats().snapshot().held_vram, 16 * MIB);
    // В простое блоки съезжают вниз, верхние куски возвращаются.
    for _ in 0..4 {
        d.maintain(Instant::now() + IDLE + Duration::from_secs(1));
    }
    d.maintain(Instant::now() + REAP_DELAY + Duration::from_secs(1));
    assert_eq!(v.live_chunks(), 1);
    assert_eq!(d.stats().snapshot().held_vram, 4 * MIB);
    d.verify().unwrap();
    let mut back = vec![0u8; model.len()];
    d.read(0, &mut back).unwrap();
    assert!(back == model, "после упаковки данные те же");
}

#[test]
fn reclaim_frees_only_untouched_blocks() {
    let size = 8 * MIB;
    let mut d = HybridDisk::new(size, stats());
    d.add(Box::new(RamStore::new(size, size).unwrap()));
    let rc = d.reclaim();
    d.write(0, &vec![9; 4 * MIB as usize]).unwrap();
    let pass = rc.begin();
    // Пока читалась карта свободного места, файловая система записала в блок 10.
    d.write(10 * BLOCK + 5, b"new").unwrap();
    let mut bits = vec![0u64; 2];
    bits[0] = u32::MAX as u64; // блоки 0..32 свободны
    rc.tx.send(FreeHint { pass, bits }).unwrap();
    d.maintain(Instant::now());
    let s = d.stats().snapshot();
    assert_eq!(s.reclaimed, 31 * BLOCK);
    assert_eq!(d.mapped(), 64 - 31);
    let mut b = vec![0u8; BLOCK as usize];
    d.read(10 * BLOCK, &mut b).unwrap();
    assert_eq!(&b[5..8], b"new");
    assert!(b[..5].iter().all(|&x| x == 9));
    d.read(11 * BLOCK, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0));
    d.read(40 * BLOCK, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 9));
    // Подсказка не того размера (диск пересоздан) отбрасывается.
    rc.tx.send(FreeHint { pass: rc.begin(), bits: vec![u64::MAX; 1] }).unwrap();
    d.maintain(Instant::now());
    assert_eq!(d.mapped(), 64 - 31);
    d.verify().unwrap();
}

#[test]
fn free_blocks_from_volume_bitmap() {
    // 1 МиБ, кластер 4 КиБ, кластеры с 64 КиБ (служебная область — блок 0).
    let size = MIB;
    let mut bm = vec![0u8; 30];
    bm[20 / 8] |= 1 << (20 % 8); // кластер 20 → байт 64К + 80К → блок 2
    let (bits, n) = free_blocks(size, BLOCK, 4096, 240, &bm);
    let free: Vec<u64> = (0..16).filter(|b| bits[0] >> b & 1 == 1).collect();
    assert_eq!(free, vec![1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    assert_eq!(n, 14);
    // Том кончается раньше диска: блоки за его концом не трогаются.
    let (bits, n) = free_blocks(size, BLOCK, 4096, 200, &bm);
    let free: Vec<u64> = (0..16).filter(|b| bits[0] >> b & 1 == 1).collect();
    assert_eq!(free, vec![1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    assert_eq!(n, 11);
    // Кластер больше блока: занятый кластер 1 закрывает блоки 2 и 3.
    let (bits, n) = free_blocks(size, 0, 128 * 1024, 8, &[0b10]);
    assert_eq!((bits[0], n), (0xFFF3, 14));
    // Кластеры не с границы блока (с 4 КиБ): блок свободен, только если все
    // его кластеры свободны. Блок 0 задевает служебную область; кластер 30
    // (байты 124К..128К) — последний в блоке 1, кластер 31 — первый в блоке 2.
    for (c, want) in [(30usize, 0b100), (31, 0b010)] {
        let mut bm = vec![0u8; 32];
        bm[c / 8] |= 1 << (c % 8);
        let (bits, n) = free_blocks(size, 4096, 4096, 255, &bm);
        assert_eq!((bits[0] & 0b111, n), (want, 14), "занят кластер {c}");
    }
    // Неполный последний блок диска.
    let (bits, n) = free_blocks(100 * 1024, 0, 4096, 25, &[0; 4]);
    assert_eq!((bits[0], n), (0b11, 2));
    // Карта короче, чем кластеров: свободны только блоки, целиком описанные картой.
    let (bits, n) = free_blocks(size, 0, 4096, 256, &[0; 2]);
    assert_eq!((bits[0], n), (1, 1));
}

fn vulkan() -> Option<vramdisk::gpu::Opened> {
    let p = vramdisk::gpu::OpenParams {
        gpu_uuid: None,
        capacity: 24 * MIB,
        limit: 16 * MIB,
        chunk: 4 * MIB,
        staging: 256 * 1024,
        reserve: 0,
    };
    match vramdisk::gpu::open(&p) {
        Ok(o) => Some(o),
        Err(e) => {
            eprintln!("пропуск проверки Vulkan: {e}");
            None
        }
    }
}

#[test]
fn vulkan_self_test_and_model() {
    let Some(mut o) = vulkan() else { return };
    eprintln!("Vulkan: {} ({}, {})", o.info.name, o.info.api, o.info.kind.label());
    assert_eq!(o.store.kind(), Kind::Vram);
    assert_eq!(o.store.limit(), 16 * MIB);
    let t = self_test(o.store.as_mut(), 8 * MIB).unwrap();
    eprintln!("запись {:.0} МБ/с, чтение {:.0} МБ/с", t.write_bps / 1e6, t.read_bps / 1e6);
    assert_eq!(t.checked_bytes, 8 * MIB);
    assert_eq!((o.store.used(), o.store.held()), (0, 0), "после самопроверки видеопамять возвращена");
    let size = 24 * MIB;
    let mut d = HybridDisk::new(size, stats());
    d.add(Box::new(RamStore::new(size, 8 * MIB).unwrap()));
    d.add(o.store);
    d.set_first(Kind::Vram);
    // В видеопамяти остался узор самопроверки — диск всё равно читается нулями.
    d.write(3, &[1]).unwrap();
    let mut out = vec![0xEEu8; 4 * MIB as usize];
    d.read(0, &mut out).unwrap();
    assert!(out[3] == 1 && out.iter().filter(|&&b| b != 0).count() == 1);
    d.trim(0, size).unwrap();
    let mut rig = Rig::new(d, Vec::new(), 42);
    rig.run(1500);
    rig.settle();
}

#[test]
fn vulkan_reserve_is_respected() {
    let p = vramdisk::gpu::OpenParams {
        gpu_uuid: None,
        capacity: 8 * MIB,
        limit: 8 * MIB,
        chunk: 4 * MIB,
        staging: MIB,
        reserve: u64::MAX / 4,
    };
    let mut o = match vramdisk::gpu::open(&p) {
        Ok(o) => o,
        Err(e) => return eprintln!("пропуск проверки Vulkan: {e}"),
    };
    // Запас больше всей видеопамяти: если драйвер сообщает бюджет, кусок не выделяется.
    match o.store.alloc(false) {
        Err(NoRoom::Budget(m)) => assert!(m.contains("запас"), "{m}"),
        Ok(_) => eprintln!("драйвер не сообщает бюджет видеопамяти — запас не проверить"),
        Err(e) => panic!("неожиданный отказ: {e}"),
    }
}

/// Долгий прогон на многих случайных последовательностях:
/// `cargo test --test disk_model -- --ignored`.
#[test]
#[ignore]
fn model_many_seeds() {
    for seed in 1..=60u64 {
        let first = if seed % 2 == 0 { Kind::Ram } else { Kind::Vram };
        let ram = (1 + seed % 7) * MIB + seed * 4099 % BLOCK;
        let vram = (1 + seed % 11) * MIB + seed * 7919 % (3 * BLOCK);
        let size = ram + vram;
        let mut d = HybridDisk::new(size, stats());
        let r = sim(Kind::Ram, size, ram, 4 * MIB, [usize::MAX, 64 * 1024, 200 * 1024][seed as usize % 3]);
        let v = sim(Kind::Vram, size, vram, 4 * MIB, [256 * 1024, 64 * 1024, 100 * 1024][seed as usize % 3]);
        let levers = if seed % 3 == 0 {
            d.add(Box::new(RamStore::new(size, ram).unwrap()));
            vec![v.clone()]
        } else {
            d.add(Box::new(r.clone()));
            vec![r, v.clone()]
        };
        d.add(Box::new(v));
        d.set_first(first);
        let mut rig = Rig::new(d, levers, seed * 0x9E37_79B9);
        rig.run(1500);
        rig.settle();
    }
}

/// Запасной путь Windows — Direct3D 11 (под Wine — через wined3d).
#[cfg(windows)]
#[test]
fn d3d11_self_test_and_model() {
    use vramdisk::win::d3d11::{D3dStore, DxgiProbe};
    let list = match DxgiProbe::new() {
        Ok(p) => p.list(),
        Err(e) => return eprintln!("пропуск проверки Direct3D 11: {e}"),
    };
    let Some(g) = list.first() else { return eprintln!("пропуск проверки Direct3D 11: видеокарт нет") };
    let p = vramdisk::gpu::OpenParams {
        gpu_uuid: Some(g.uuid.clone()),
        capacity: 24 * MIB,
        limit: 16 * MIB,
        chunk: 4 * MIB,
        staging: 256 * 1024,
        reserve: 0,
    };
    let mut s = D3dStore::open(&p, g).unwrap();
    eprintln!("Direct3D 11: {} ({}, {}, {} МиБ)", s.info.name, s.info.uuid, s.info.kind.label(), s.info.vram_total >> 20);
    let t = self_test(&mut s, 8 * MIB).unwrap();
    eprintln!("запись {:.0} МБ/с, чтение {:.0} МБ/с", t.write_bps / 1e6, t.read_bps / 1e6);
    assert_eq!((s.used(), s.held()), (0, 0), "после самопроверки видеопамять возвращена");
    let size = 24 * MIB;
    let mut d = HybridDisk::new(size, stats());
    d.add(Box::new(RamStore::new(size, 8 * MIB).unwrap()));
    d.add(Box::new(s));
    d.set_first(Kind::Vram);
    let mut rig = Rig::new(d, Vec::new(), 4242);
    rig.run(1500);
    rig.settle();
}
