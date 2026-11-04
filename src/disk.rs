//! Динамический диск из оперативной памяти и видеопамяти.
//!
//! Диск размечен на логические блоки по 64 КиБ. Пустой блок памяти не
//! занимает и читается нулями. При первой записи блоку выделяется слот в
//! одном из хранилищ: сначала в том, что выбрал пользователь, когда оно
//! заполнено до предела — в другом. Если хранилище отказало (видеопамять
//! заняла игра, системе не хватило памяти), блок аварийно ложится в другое
//! сверх его предела, а потом, когда появится место, переезжает обратно.
//!
//! Память возвращается, когда блок больше не нужен: по TRIM от файловой
//! системы, по карте свободного места (проход очистки, `FreeHint`), при
//! записи в блок сплошных нулей.

pub use crate::pool::Kind;
use crate::pool::{is_zero, NoRoom, Store, BLOCK};
use crate::stats::Stats;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskError {
    /// Запрос за пределами диска.
    OutOfRange,
    /// Ошибка устройства или нехватка памяти.
    Device(String),
    /// Видеокарта сброшена драйвером: содержимое видеопамяти потеряно.
    Lost(String),
}

impl std::fmt::Display for DiskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiskError::OutOfRange => write!(f, "запрос за пределами диска"),
            DiskError::Device(s) => write!(f, "{s}"),
            DiskError::Lost(s) => write!(f, "{s}"),
        }
    }
}

/// Диск простаивает: можно заняться упаковкой и переездом блоков.
pub const IDLE: Duration = Duration::from_secs(2);
/// Сколько блоков освобождать за один шаг обслуживания по подсказке очистки.
const FREE_STEP: usize = 16384;
/// Сколько блоков переносить за один шаг упаковки / переезда.
const MOVE_STEP: usize = 1024;

const POOL_BIT: u32 = 1 << 31;
const MAX_SLOT: u64 = (POOL_BIT - 2) as u64;

fn enc(pool: usize, slot: u64) -> u32 {
    debug_assert!(pool < 2 && slot <= MAX_SLOT);
    (if pool == 1 { POOL_BIT } else { 0 }) | (slot as u32 + 1)
}

fn dec(v: u32) -> Option<(usize, u64)> {
    (v != 0).then(|| ((v >> 31) as usize, ((v & !POOL_BIT) - 1) as u64))
}

/// Подсказка прохода очистки: биты блоков, целиком свободных в файловой
/// системе. Блок освобождается, только если в него не писали с начала
/// прохода `pass` — иначе файловая система могла занять его заново.
pub struct FreeHint {
    pub pass: u32,
    pub bits: Vec<u64>,
}

/// Блоки диска, целиком лежащие в свободных кластерах файловой системы.
///
/// `base` — где начинаются кластеры (байт от начала тома), `cluster` — размер
/// кластера, `bitmap` — карта кластеров тома (бит 1 — занят), `clusters` —
/// сколько их. Блоки, задевающие служебную область до кластеров или хвост
/// тома после них, свободными не считаются.
pub fn free_blocks(size: u64, base: u64, cluster: u64, clusters: u64, bitmap: &[u8]) -> (Vec<u64>, u64) {
    let blocks = size.div_ceil(BLOCK);
    let mut out = vec![0u64; blocks.div_ceil(64) as usize];
    let mut n = 0;
    if cluster == 0 {
        return (out, 0);
    }
    let clusters = clusters.min(bitmap.len() as u64 * 8);
    let used = |c: u64| bitmap[(c / 8) as usize] >> (c % 8) & 1 == 1;
    for b in 0..blocks {
        let start = b * BLOCK;
        let end = ((b + 1) * BLOCK).min(size);
        if start < base {
            continue;
        }
        let c0 = (start - base) / cluster;
        let c1 = (end - 1 - base) / cluster;
        if c1 >= clusters {
            break;
        }
        if !(c0..=c1).any(used) {
            out[(b / 64) as usize] |= 1 << (b % 64);
            n += 1;
        }
    }
    (out, n)
}

/// Связь прохода очистки (отдельный поток) с диском.
#[derive(Clone)]
pub struct Reclaim {
    pub tx: mpsc::Sender<FreeHint>,
    pub pass: Arc<AtomicU32>,
    pub size: u64,
}

impl Reclaim {
    /// Начать проход: возвращает его номер. Вызывать до чтения карты
    /// свободного места.
    pub fn begin(&self) -> u32 {
        self.pass.fetch_add(1, SeqCst) + 1
    }
}

struct Pool {
    store: Box<dyn Store>,
    /// Слот → логический блок + 1 (0 — свободен).
    rev: Vec<u32>,
    /// Когда в журнал писали об отказе этого хранилища.
    noted: Option<Instant>,
}

pub struct HybridDisk {
    size: u64,
    map: Vec<u32>,
    /// Номер прохода очистки на момент последней записи в блок.
    gen: Vec<u32>,
    pools: Vec<Pool>,
    order: Vec<usize>,
    stats: Arc<Stats>,
    pass: Arc<AtomicU32>,
    rx: Option<mpsc::Receiver<FreeHint>>,
    pending: Option<(FreeHint, usize)>,
    last_io: Instant,
}

/// Куски запроса по блокам: (блок, смещение в блоке, длина, смещение в буфере).
struct Blocks {
    pos: u64,
    end: u64,
    start: u64,
}

impl Iterator for Blocks {
    type Item = (usize, u64, usize, usize);
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.end {
            return None;
        }
        let b = self.pos / BLOCK;
        let in_off = self.pos % BLOCK;
        let len = (BLOCK - in_off).min(self.end - self.pos);
        let item = (b as usize, in_off, len as usize, (self.pos - self.start) as usize);
        self.pos += len;
        Some(item)
    }
}

fn blocks(off: u64, len: u64) -> Blocks {
    Blocks { pos: off, end: off + len, start: off }
}

/// Добавить кусок, склеив с предыдущим, если он продолжает его.
fn push_run(v: &mut Vec<(u64, u64, usize)>, addr: u64, len: u64, off: usize) {
    if let Some(last) = v.last_mut() {
        if last.0 + last.1 == addr && last.2 + last.1 as usize == off {
            last.1 += len;
            return;
        }
    }
    v.push((addr, len, off));
}

impl HybridDisk {
    pub fn new(size: u64, stats: Arc<Stats>) -> Self {
        let blocks = size.div_ceil(BLOCK) as usize;
        assert!((blocks as u64) < u32::MAX as u64, "слишком большой диск");
        Self {
            size,
            map: vec![0; blocks],
            gen: vec![0; blocks],
            pools: Vec::new(),
            order: Vec::new(),
            stats,
            pass: Arc::new(AtomicU32::new(0)),
            rx: None,
            pending: None,
            last_io: Instant::now(),
        }
    }

    /// Добавить хранилище (не больше двух). Первое добавленное заполняется
    /// первым, пока не вызван `set_first`.
    pub fn add(&mut self, store: Box<dyn Store>) {
        assert!(self.pools.len() < 2, "не больше двух хранилищ");
        let limit = store.limit();
        match store.kind() {
            Kind::Ram => self.stats.max_ram.fetch_add(limit, Relaxed),
            Kind::Vram => self.stats.max_vram.fetch_add(limit, Relaxed),
        };
        self.order.push(self.pools.len());
        self.pools.push(Pool { store, rev: Vec::new(), noted: None });
        self.refresh();
    }

    /// Какое хранилище заполнять первым.
    pub fn set_first(&mut self, kind: Kind) {
        self.order.sort_by_key(|&p| self.pools[p].store.kind() != kind);
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// Наибольший запрос, который хранилища принимают за раз.
    pub fn max_io(&self) -> usize {
        self.pools.iter().map(|p| p.store.max_io()).min().unwrap_or(usize::MAX).max(BLOCK as usize)
    }

    /// Связь для прохода очистки (создаётся один раз).
    pub fn reclaim(&mut self) -> Reclaim {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        Reclaim { tx, pass: self.pass.clone(), size: self.size }
    }

    /// Блоков с данными.
    pub fn mapped(&self) -> u64 {
        self.pools.iter().map(|p| p.store.used()).sum()
    }

    fn block_len(&self, b: usize) -> u64 {
        (self.size - b as u64 * BLOCK).min(BLOCK)
    }

    fn check(&self, off: u64, len: u64) -> Result<(), DiskError> {
        if off.checked_add(len).is_none_or(|e| e > self.size) {
            Err(DiskError::OutOfRange)
        } else {
            Ok(())
        }
    }

    fn account(&self, kind: Kind, read: bool, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let c = match (kind, read) {
            (Kind::Ram, true) => &self.stats.read_ram,
            (Kind::Ram, false) => &self.stats.write_ram,
            (Kind::Vram, true) => &self.stats.read_vram,
            (Kind::Vram, false) => &self.stats.write_vram,
        };
        c.fetch_add(bytes, Relaxed);
    }

    /// Обновить счётчики занятой памяти.
    fn refresh(&self) {
        let s = &self.stats;
        for p in &self.pools {
            let st = &p.store;
            let (used, held, over) = (st.used() * BLOCK, st.held(), st.over());
            match st.kind() {
                Kind::Ram => {
                    s.used_ram.store(used, Relaxed);
                    s.held_ram.store(held, Relaxed);
                    s.over_ram.store(over, Relaxed);
                }
                Kind::Vram => {
                    s.used_vram.store(used, Relaxed);
                    s.held_vram.store(held, Relaxed);
                    s.over_vram.store(over, Relaxed);
                }
            }
        }
    }

    fn fail(&self, e: DiskError) -> DiskError {
        if let DiskError::Lost(_) = e {
            self.stats.device_lost.store(true, Relaxed);
        }
        self.stats.set_error(e.to_string());
        e
    }

    /// Записать в журнал, что хранилище отказало (не чаще раза в минуту).
    fn note(&mut self, p: usize, text: String) {
        let now = Instant::now();
        if self.pools[p].noted.is_none_or(|t| now - t > Duration::from_secs(60)) {
            self.pools[p].noted = Some(now);
            crate::logger::log(text);
        }
    }

    /// Выделить блоку `b` слот: по порядку хранилищ, сначала в пределах,
    /// затем аварийно сверх предела.
    fn alloc_block(&mut self, b: usize) -> Result<(usize, crate::pool::Slot), DiskError> {
        let mut why: Vec<(usize, NoRoom)> = Vec::new();
        for over in [false, true] {
            for i in 0..self.order.len() {
                let p = self.order[i];
                match self.pools[p].store.alloc(over) {
                    Ok(slot) => {
                        if slot.index > MAX_SLOT {
                            self.pools[p].store.free(slot.index);
                            return Err(DiskError::Device("слишком много блоков".into()));
                        }
                        self.map[b] = enc(p, slot.index);
                        let rev = &mut self.pools[p].rev;
                        let si = slot.index as usize;
                        if rev.len() <= si {
                            rev.resize((si + 1).next_power_of_two().max(1024), 0);
                        }
                        rev[si] = b as u32 + 1;
                        let refused: Vec<String> = why
                            .iter()
                            .filter(|(_, r)| *r != NoRoom::Limit)
                            .map(|(q, r)| format!("{}: {r}", self.pools[*q].store.kind().label()))
                            .collect();
                        if !refused.is_empty() {
                            let kind = self.pools[p].store.kind().label();
                            let text = if over {
                                format!("{} — данные временно пишутся сверх предела: {kind}", refused.join("; "))
                            } else {
                                format!("{} — данные пишутся в другое хранилище: {kind}", refused.join("; "))
                            };
                            self.note(p, text);
                        }
                        return Ok((p, slot));
                    }
                    Err(e) => {
                        if !why.iter().any(|(q, r)| *q == p && *r == e) {
                            why.push((p, e));
                        }
                    }
                }
            }
        }
        let text: Vec<String> = why
            .iter()
            .filter(|(_, r)| *r != NoRoom::Limit)
            .map(|(q, r)| format!("{}: {r}", self.pools[*q].store.kind().label()))
            .collect();
        self.stats.nomem.fetch_add(1, Relaxed);
        Err(DiskError::Device(if text.is_empty() {
            "на диске закончилась память".into()
        } else {
            format!("не хватило памяти для записи — {}", text.join("; "))
        }))
    }

    fn release_block(&mut self, b: usize) -> u64 {
        let Some((p, s)) = dec(self.map[b]) else { return 0 };
        self.map[b] = 0;
        self.pools[p].rev[s as usize] = 0;
        self.pools[p].store.free(s);
        self.block_len(b)
    }

    /// Конец очередного куска запроса от `pos`: не длиннее того, что
    /// хранилища принимают за раз, и по границе блока — чтобы блок не делился
    /// между кусками.
    fn piece_end(&self, pos: u64, end: u64) -> u64 {
        // max_io() не меньше блока, поэтому кусок никогда не пустой.
        (pos.saturating_add(self.max_io() as u64) / BLOCK * BLOCK).min(end)
    }

    pub fn read(&mut self, off: u64, out: &mut [u8]) -> Result<(), DiskError> {
        self.check(off, out.len() as u64)?;
        self.last_io = Instant::now();
        let end = off + out.len() as u64;
        let mut pos = off;
        while pos < end {
            let e = self.piece_end(pos, end);
            if let Err(err) = self.read_piece(pos, &mut out[(pos - off) as usize..(e - off) as usize]) {
                return Err(self.fail(err));
            }
            pos = e;
        }
        Ok(())
    }

    fn read_piece(&mut self, off: u64, out: &mut [u8]) -> Result<(), DiskError> {
        let mut runs: [Vec<(u64, u64, usize)>; 2] = Default::default();
        let mut bytes = [0u64; 2];
        for (b, in_off, len, o) in blocks(off, out.len() as u64) {
            match dec(self.map[b]) {
                None => out[o..o + len].fill(0),
                Some((p, s)) => {
                    push_run(&mut runs[p], s * BLOCK + in_off, len as u64, o);
                    bytes[p] += len as u64;
                }
            }
        }
        for p in 0..self.pools.len() {
            if !runs[p].is_empty() {
                self.pools[p].store.read(&runs[p], out)?;
                self.account(self.pools[p].store.kind(), true, bytes[p]);
            }
        }
        Ok(())
    }

    pub fn write(&mut self, off: u64, data: &[u8]) -> Result<(), DiskError> {
        self.check(off, data.len() as u64)?;
        self.last_io = Instant::now();
        let end = off + data.len() as u64;
        let mut pos = off;
        while pos < end {
            let e = self.piece_end(pos, end);
            let r = self.write_piece(pos, &data[(pos - off) as usize..(e - off) as usize]);
            self.refresh();
            if let Err(err) = r {
                return Err(self.fail(err));
            }
            pos = e;
        }
        Ok(())
    }

    fn write_piece(&mut self, off: u64, data: &[u8]) -> Result<(), DiskError> {
        let pass = self.pass.load(SeqCst);
        let mut fills: [Vec<(u64, u64)>; 2] = Default::default();
        let mut runs: [Vec<(u64, u64, usize)>; 2] = Default::default();
        let mut bytes = [0u64; 2];
        let mut freed = 0;
        let mut err = None;
        for (b, in_off, len, o) in blocks(off, data.len() as u64) {
            self.gen[b] = pass;
            let piece = &data[o..o + len];
            let whole = len as u64 == self.block_len(b);
            let zero = is_zero(piece);
            match dec(self.map[b]) {
                // Нули в пустой блок: он и так читается нулями.
                None if zero => {}
                None => match self.alloc_block(b) {
                    Ok((p, slot)) => {
                        let base = slot.index * BLOCK;
                        if !whole && !slot.zeroed {
                            fills[p].push((base, BLOCK));
                        }
                        push_run(&mut runs[p], base + in_off, len as u64, o);
                        bytes[p] += len as u64;
                    }
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                },
                // Блок целиком перезаписан нулями — память больше не нужна.
                Some(_) if zero && whole => freed += self.release_block(b),
                Some((p, s)) => {
                    push_run(&mut runs[p], s * BLOCK + in_off, len as u64, o);
                    bytes[p] += len as u64;
                }
            }
        }
        // Даже при нехватке памяти дописываем то, под что слоты уже выданы:
        // у каждого выделенного блока должно быть определённое содержимое.
        for p in 0..self.pools.len() {
            if !runs[p].is_empty() || !fills[p].is_empty() {
                self.pools[p].store.write(&fills[p], &runs[p], data)?;
                self.account(self.pools[p].store.kind(), false, bytes[p]);
            }
        }
        if freed > 0 {
            self.stats.trimmed.fetch_add(freed, Relaxed);
        }
        err.map_or(Ok(()), Err)
    }

    /// Обнулить отрезок (запрос ZERO от ImDisk): целые блоки освобождаются,
    /// края записываются нулями.
    pub fn zero(&mut self, off: u64, len: u64) -> Result<(), DiskError> {
        self.check(off, len)?;
        self.last_io = Instant::now();
        let pass = self.pass.load(SeqCst);
        let mut freed = 0;
        let mut r = Ok(());
        for (b, in_off, l, _) in blocks(off, len) {
            // Обнуление части блока — тоже запись: проход очистки, начатый
            // раньше, этот блок освобождать не должен.
            self.gen[b] = pass;
            match dec(self.map[b]) {
                None => {}
                Some(_) if l as u64 == self.block_len(b) => freed += self.release_block(b),
                Some((p, s)) => {
                    let zeros = vec![0u8; l];
                    if let Err(e) = self.pools[p].store.write(&[], &[(s * BLOCK + in_off, l as u64, 0)], &zeros) {
                        r = Err(e);
                        break;
                    }
                }
            }
        }
        self.stats.trimmed.fetch_add(freed, Relaxed);
        self.refresh();
        r.map_err(|e| self.fail(e))
    }

    /// Освободить отрезок (TRIM от файловой системы). Неполные блоки не
    /// трогаются: в них могут быть чужие данные.
    pub fn trim(&mut self, off: u64, len: u64) -> Result<(), DiskError> {
        let len = len.min(self.size.saturating_sub(off));
        if len == 0 {
            return Ok(());
        }
        self.last_io = Instant::now();
        let end = off + len;
        let first = off.div_ceil(BLOCK) as usize;
        let last = if end == self.size { self.map.len() } else { (end / BLOCK) as usize };
        let mut freed = 0;
        for b in first..last.max(first) {
            if self.map[b] != 0 {
                freed += self.release_block(b);
            }
        }
        self.stats.trimmed.fetch_add(freed, Relaxed);
        self.refresh();
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), DiskError> {
        for p in &mut self.pools {
            p.store.flush()?;
        }
        Ok(())
    }

    /// Обслуживание между запросами: подсказки очистки, упаковка
    /// видеопамяти, переезд блоков из-за предела, возврат пустых кусков.
    pub fn maintain(&mut self, now: Instant) {
        if let Some(rx) = &self.rx {
            while let Ok(h) = rx.try_recv() {
                if h.bits.len() as u64 * 64 >= self.map.len() as u64 {
                    self.pending = Some((h, 0));
                }
            }
        }
        self.apply_hint(FREE_STEP);
        if now.saturating_duration_since(self.last_io) >= IDLE {
            for p in 0..self.pools.len() {
                self.compact(p);
            }
            self.rebalance();
        }
        for p in &mut self.pools {
            p.store.reap(now, false);
        }
        self.refresh();
    }

    /// Освободить блоки, которые файловая система считает свободными.
    fn apply_hint(&mut self, budget: usize) {
        let Some((hint, mut i)) = self.pending.take() else { return };
        let n = self.map.len();
        let mut done = 0;
        let mut freed = 0;
        while i < n && done < budget {
            let w = hint.bits[i / 64];
            if w == 0 {
                i = (i / 64 + 1) * 64;
                continue;
            }
            if w >> (i % 64) & 1 == 1 && self.map[i] != 0 && self.gen[i] < hint.pass {
                freed += self.release_block(i);
                done += 1;
            }
            i += 1;
        }
        if freed > 0 {
            self.stats.reclaimed.fetch_add(freed, Relaxed);
        }
        if i < n {
            self.pending = Some((hint, i));
        }
    }

    /// Упаковка: переносит блоки из верхнего куска в дыры нижних, чтобы
    /// верхний опустел и вернулся драйверу.
    fn compact(&mut self, p: usize) {
        let moves = self.pools[p].store.plan_compaction(MOVE_STEP);
        if moves.is_empty() {
            return;
        }
        let pool = &mut self.pools[p];
        match pool.store.copy(&moves) {
            Ok(()) => {
                for &(s, d) in &moves {
                    let owner = pool.rev[s as usize];
                    debug_assert!(owner != 0, "перенос свободного слота");
                    if owner != 0 {
                        self.map[owner as usize - 1] = enc(p, d);
                    }
                    if pool.rev.len() <= d as usize {
                        pool.rev.resize(d as usize + 1, 0);
                    }
                    pool.rev[d as usize] = owner;
                    pool.rev[s as usize] = 0;
                    pool.store.free(s);
                }
            }
            Err(e) => {
                for &(_, d) in &moves {
                    pool.store.free(d);
                }
                let e = self.fail(e);
                crate::logger::log(format!("упаковка видеопамяти не удалась: {e}"));
            }
        }
    }

    /// Блоки, легшие сверх предела хранилища, переезжают в другое, когда в
    /// нём появилось место.
    fn rebalance(&mut self) {
        if self.pools.len() < 2 {
            return;
        }
        for p in 0..2 {
            let q = 1 - p;
            let over = self.pools[p].store.over();
            let room = self.pools[q].store.room();
            if over == 0 || room < BLOCK {
                continue;
            }
            let want = (over.min(room) / BLOCK).max(1) as usize;
            let step = ((self.max_io() as u64 / BLOCK) as usize).clamp(1, 128);
            let mut moved = 0;
            while moved < want.min(MOVE_STEP) {
                // Самые верхние слоты: у видеопамяти так пустеют верхние куски.
                let mut batch: Vec<(usize, u64)> = Vec::new();
                let rev = &self.pools[p].rev;
                let mut s = rev.len();
                while s > 0 && batch.len() < step.min(want - moved) {
                    s -= 1;
                    if rev[s] != 0 {
                        batch.push((rev[s] as usize - 1, s as u64));
                    }
                }
                if batch.is_empty() || self.move_blocks(p, q, &batch).is_err() {
                    return;
                }
                moved += batch.len();
            }
        }
    }

    /// Перенести блоки `(блок, слот в p)` в хранилище `q`.
    fn move_blocks(&mut self, p: usize, q: usize, batch: &[(usize, u64)]) -> Result<(), ()> {
        let mut dst = Vec::new();
        for _ in batch {
            match self.pools[q].store.alloc(false) {
                Ok(s) => dst.push(s.index),
                Err(_) => break,
            }
        }
        if dst.is_empty() {
            return Err(());
        }
        let n = dst.len();
        let mut buf = vec![0u8; n * BLOCK as usize];
        let rd: Vec<(u64, u64, usize)> =
            batch[..n].iter().enumerate().map(|(i, &(_, s))| (s * BLOCK, BLOCK, i * BLOCK as usize)).collect();
        let wr: Vec<(u64, u64, usize)> =
            dst.iter().enumerate().map(|(i, &d)| (d * BLOCK, BLOCK, i * BLOCK as usize)).collect();
        let r = self.pools[p].store.read(&rd, &mut buf).and_then(|_| self.pools[q].store.write(&[], &wr, &buf));
        if let Err(e) = r {
            for &d in &dst {
                self.pools[q].store.free(d);
            }
            self.fail(e);
            return Err(());
        }
        for (i, &(b, s)) in batch[..n].iter().enumerate() {
            let d = dst[i];
            self.map[b] = enc(q, d);
            let rev = &mut self.pools[q].rev;
            if rev.len() <= d as usize {
                rev.resize((d as usize + 1).next_power_of_two().max(1024), 0);
            }
            rev[d as usize] = b as u32 + 1;
            self.pools[p].rev[s as usize] = 0;
            self.pools[p].store.free(s);
        }
        self.stats.moved.fetch_add(n as u64 * BLOCK, Relaxed);
        if n < batch.len() {
            return Err(());
        }
        Ok(())
    }

    /// Проверка внутренней согласованности (для тестов).
    pub fn verify(&self) -> Result<(), String> {
        let mut per = [0u64; 2];
        for (b, &v) in self.map.iter().enumerate() {
            if let Some((p, s)) = dec(v) {
                per[p] += 1;
                let owner = self.pools[p].rev.get(s as usize).copied().unwrap_or(0);
                if owner != b as u32 + 1 {
                    return Err(format!("блок {b}: слот {s} хранилища {p} принадлежит {owner}"));
                }
            }
        }
        for (p, pool) in self.pools.iter().enumerate() {
            if pool.store.used() != per[p] {
                return Err(format!("хранилище {p}: занято {} слотов, а блоков {}", pool.store.used(), per[p]));
            }
        }
        Ok(())
    }
}
