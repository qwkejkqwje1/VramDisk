//! Хранилища блоков для динамического диска.
//!
//! Диск размечен на логические блоки по 64 КиБ. Блок, в который ничего не
//! писали, памяти не занимает и читается нулями. При первой записи блоку
//! выделяется «слот» — 64 КиБ в одном из хранилищ (`Store`): оперативной
//! памяти или видеопамяти. Видеопамять берётся у драйвера кусками
//! (`ChunkAlloc`): кусок выделяется, когда в уже выделенных нет свободных
//! слотов, и возвращается, когда опустел.

use crate::disk::DiskError;
use std::time::{Duration, Instant};

/// Логический блок диска.
pub const BLOCK_SHIFT: u32 = 16;
pub const BLOCK: u64 = 1 << BLOCK_SHIFT;
/// Кусок видеопамяти.
pub const CHUNK: u64 = 64 << 20;
/// Пустой кусок отдаётся не сразу: файлы часто удаляют и тут же пишут новые.
pub const REAP_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Ram,
    Vram,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Ram => "оперативная память",
            Kind::Vram => "видеопамять",
        }
    }
}

/// Выделенный слот: номер и известно ли, что он заполнен нулями.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub index: u64,
    pub zeroed: bool,
}

/// Почему хранилище не дало слот.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoRoom {
    /// Достигнут предел, заданный пользователем (обычная ситуация).
    Limit,
    /// Видеопамять нужна другим программам: свободно меньше запаса.
    Budget(String),
    /// Система или драйвер отказали в памяти.
    Memory(String),
}

impl std::fmt::Display for NoRoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoRoom::Limit => write!(f, "достигнут заданный предел"),
            NoRoom::Budget(s) | NoRoom::Memory(s) => write!(f, "{s}"),
        }
    }
}

/// Хранилище слотов. Адрес внутри хранилища — `слот × BLOCK + смещение`.
pub trait Store: Send {
    fn kind(&self) -> Kind;
    /// Предел, заданный пользователем, байт.
    fn limit(&self) -> u64;
    /// Выделить слот. `over` — можно сверх предела (аварийно, когда другое
    /// хранилище отказало).
    fn alloc(&mut self, over: bool) -> Result<Slot, NoRoom>;
    /// Вернуть слот.
    fn free(&mut self, slot: u64);
    /// Занятые слоты.
    fn used(&self) -> u64;
    /// Сколько памяти хранилище держит сейчас, байт.
    fn held(&self) -> u64;
    /// Прочитать куски `(адрес, длина, смещение в out)`.
    fn read(&mut self, runs: &[(u64, u64, usize)], out: &mut [u8]) -> Result<(), DiskError>;
    /// Обнулить отрезки `fills`, затем записать куски `(адрес, длина, смещение в data)`.
    fn write(&mut self, fills: &[(u64, u64)], runs: &[(u64, u64, usize)], data: &[u8]) -> Result<(), DiskError>;
    /// Дождаться завершения отложенных записей.
    fn flush(&mut self) -> Result<(), DiskError> {
        Ok(())
    }
    /// Наибольший объём одного обращения.
    fn max_io(&self) -> usize {
        usize::MAX
    }
    /// Скопировать слоты `(откуда, куда)` внутри хранилища.
    fn copy(&mut self, moves: &[(u64, u64)]) -> Result<(), DiskError> {
        let mut buf = vec![0u8; BLOCK as usize];
        for &(s, d) in moves {
            self.read(&[(s * BLOCK, BLOCK, 0)], &mut buf)?;
            self.write(&[], &[(d * BLOCK, BLOCK, 0)], &buf)?;
        }
        Ok(())
    }
    /// Вернуть системе опустевшие куски (`force` — не дожидаясь паузы).
    /// Возвращает освобождённые байты.
    fn reap(&mut self, _now: Instant, _force: bool) -> u64 {
        0
    }
    /// План упаковки: слоты верхнего куска → свободные слоты нижних. Слоты
    /// назначения уже выделены; после копирования исходные освобождает
    /// вызывающий.
    fn plan_compaction(&mut self, _max: usize) -> Vec<(u64, u64)> {
        Vec::new()
    }
    /// Сколько ещё можно занять в пределах лимита, байт.
    fn room(&self) -> u64 {
        self.limit().saturating_sub(self.used() * BLOCK)
    }
    /// На сколько хранилище вышло за предел, байт.
    fn over(&self) -> u64 {
        self.held().saturating_sub(self.limit())
    }
}

/// Учёт слотов в кусках одинакового размера: какие куски выделены, какие
/// слоты в них заняты. Новые слоты берутся из самого нижнего выделенного
/// куска со свободным местом, поэтому верхние куски пустеют первыми.
#[derive(Debug, Clone)]
pub struct ChunkAlloc {
    per: u64,
    soft: usize,
    live: Vec<bool>,
    used: Vec<u32>,
    bits: Vec<u64>,
    empty_since: Vec<Option<Instant>>,
    live_n: usize,
    used_n: u64,
    /// Ниже этого куска свободных слотов в выделенных кусках нет.
    hint: usize,
}

impl ChunkAlloc {
    /// `capacity` и `soft` — в слотах; `per` — слотов в куске (кратно 64).
    pub fn new(capacity: u64, per: u64, soft: u64) -> Self {
        assert!(per > 0 && per.is_multiple_of(64), "размер куска должен быть кратен 64 слотам");
        let n = capacity.div_ceil(per) as usize;
        Self {
            per,
            soft: soft.div_ceil(per) as usize,
            live: vec![false; n],
            used: vec![0; n],
            bits: vec![0; n * (per / 64) as usize],
            empty_since: vec![None; n],
            live_n: 0,
            used_n: 0,
            hint: 0,
        }
    }

    pub fn per(&self) -> u64 {
        self.per
    }
    pub fn chunks(&self) -> usize {
        self.live.len()
    }
    pub fn soft(&self) -> usize {
        self.soft
    }
    pub fn live_count(&self) -> usize {
        self.live_n
    }
    pub fn used(&self) -> u64 {
        self.used_n
    }
    pub fn is_live(&self, c: usize) -> bool {
        self.live[c]
    }
    pub fn used_in(&self, c: usize) -> u64 {
        self.used[c] as u64
    }

    fn word(&self, slot: u64) -> (usize, u64) {
        let c = (slot / self.per) as usize;
        let i = slot % self.per;
        (c * (self.per / 64) as usize + (i / 64) as usize, 1u64 << (i % 64))
    }

    pub fn is_used(&self, slot: u64) -> bool {
        let (w, m) = self.word(slot);
        self.bits[w] & m != 0
    }

    /// Свободный слот в самом нижнем выделенном куске ниже `below`.
    fn take_below(&mut self, below: usize) -> Option<u64> {
        let mut c = self.hint;
        while c < below {
            if self.live[c] && (self.used[c] as u64) < self.per {
                if below == self.live.len() {
                    self.hint = c;
                }
                return Some(self.take_in(c));
            }
            c += 1;
        }
        if below == self.live.len() {
            self.hint = below;
        }
        None
    }

    /// Свободный слот в уже выделенном куске (самом нижнем).
    pub fn take_live(&mut self) -> Option<u64> {
        self.take_below(self.live.len())
    }

    /// Занять первый свободный слот выделенного куска `c`.
    pub fn take_in(&mut self, c: usize) -> u64 {
        assert!(self.live[c], "кусок {c} не выделен");
        let wpc = (self.per / 64) as usize;
        for wi in 0..wpc {
            let w = &mut self.bits[c * wpc + wi];
            if *w != u64::MAX {
                let b = (!*w).trailing_zeros() as u64;
                *w |= 1 << b;
                self.used[c] += 1;
                self.used_n += 1;
                self.empty_since[c] = None;
                return c as u64 * self.per + wi as u64 * 64 + b;
            }
        }
        panic!("кусок {c} заполнен");
    }

    /// Нижний невыделенный кусок, если предел позволяет.
    pub fn next_dead(&self, over: bool) -> Option<usize> {
        if !over && self.live_n >= self.soft {
            return None;
        }
        self.live.iter().position(|l| !l)
    }

    /// Кусок `c` выделен у драйвера.
    pub fn set_live(&mut self, c: usize) {
        assert!(!self.live[c], "кусок {c} уже выделен");
        self.live[c] = true;
        self.live_n += 1;
        self.empty_since[c] = Some(Instant::now());
        self.hint = self.hint.min(c);
    }

    pub fn release(&mut self, slot: u64) {
        let (w, m) = self.word(slot);
        assert!(self.bits[w] & m != 0, "слот {slot} не занят");
        self.bits[w] &= !m;
        let c = (slot / self.per) as usize;
        self.used[c] -= 1;
        self.used_n -= 1;
        if self.used[c] == 0 {
            self.empty_since[c] = Some(Instant::now());
        }
        self.hint = self.hint.min(c);
    }

    /// Пустые куски, которые пора вернуть (`force` — все пустые сразу).
    /// Помечает их невыделенными.
    pub fn reap(&mut self, now: Instant, force: bool) -> Vec<usize> {
        let mut out = Vec::new();
        for c in 0..self.live.len() {
            if self.live[c]
                && self.used[c] == 0
                && (force || self.empty_since[c].is_some_and(|t| now.saturating_duration_since(t) >= REAP_DELAY))
            {
                self.live[c] = false;
                self.live_n -= 1;
                self.empty_since[c] = None;
                out.push(c);
            }
        }
        out
    }

    /// План упаковки: если свободных слотов в нижних кусках хватает, чтобы
    /// вместить весь верхний занятый кусок, — до `max` переносов из него.
    /// Слоты назначения сразу помечаются занятыми.
    pub fn plan_compaction(&mut self, max: usize) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        if self.live_n < 2 || self.live_n as u64 * self.per - self.used_n < self.per {
            return out;
        }
        let Some(top) = (0..self.live.len()).rev().find(|&c| self.live[c] && self.used[c] > 0) else {
            return out;
        };
        let holes_below: u64 = (0..top).filter(|&c| self.live[c]).map(|c| self.per - self.used[c] as u64).sum();
        if holes_below < self.used[top] as u64 {
            return out;
        }
        let base = top as u64 * self.per;
        let mut i = 0;
        while i < self.per && out.len() < max {
            let s = base + i;
            if self.is_used(s) {
                match self.take_below(top) {
                    Some(d) => out.push((s, d)),
                    None => break,
                }
            }
            i += 1;
        }
        out
    }
}

/// Быстрая проверка «всё нули».
pub fn is_zero(b: &[u8]) -> bool {
    let (head, body, tail) = unsafe { b.align_to::<u128>() };
    head.iter().all(|&x| x == 0)
        && tail.iter().all(|&x| x == 0)
        && body.chunks(16).all(|c| c.iter().fold(0u128, |a, &x| a | x) == 0)
}

/// Псевдослучайный узор (xorshift), разный для разных `seed`.
pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut v = Vec::with_capacity(len + 8);
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SelfTest {
    pub write_bps: f64,
    pub read_bps: f64,
    pub checked_bytes: u64,
    pub ms: u128,
}

/// Проверка хранилища перед подключением диска: занимает до `bytes`
/// (обычно один кусок), пишет узор, читает обратно и сравнивает, заодно
/// замеряя скорость. Потом всё возвращает.
pub fn self_test(store: &mut dyn Store, bytes: u64) -> Result<SelfTest, String> {
    let t0 = Instant::now();
    let want = (bytes / BLOCK).max(1);
    let mut slots = Vec::new();
    let mut fail = None;
    while (slots.len() as u64) < want {
        match store.alloc(false) {
            Ok(s) => slots.push(s.index),
            Err(NoRoom::Limit) if !slots.is_empty() => break,
            Err(e) => {
                fail = Some(format!("не удалось выделить память для проверки ({}): {e}", store.kind().label()));
                break;
            }
        }
    }
    let r = match fail {
        Some(e) => Err(e),
        None => test_slots(store, &slots),
    };
    for &s in &slots {
        store.free(s);
    }
    store.reap(Instant::now(), true);
    r.map(|mut t| {
        t.ms = t0.elapsed().as_millis();
        t
    })
}

fn test_slots(store: &mut dyn Store, slots: &[u64]) -> Result<SelfTest, String> {
    let per_batch = ((store.max_io().min(8 << 20) as u64 / BLOCK).max(1)) as usize;
    let total = slots.len() * BLOCK as usize;
    let data = pattern(total, 0x5EED);
    let mut back = vec![0u8; total];
    let e = |e: DiskError| e.to_string();
    let runs = |batch: &[u64]| -> Vec<(u64, u64, usize)> {
        batch.iter().enumerate().map(|(i, &s)| (s * BLOCK, BLOCK, i * BLOCK as usize)).collect()
    };
    let t = Instant::now();
    for (bi, batch) in slots.chunks(per_batch).enumerate() {
        let at = bi * per_batch * BLOCK as usize;
        store.write(&[], &runs(batch), &data[at..at + batch.len() * BLOCK as usize]).map_err(e)?;
    }
    store.flush().map_err(e)?;
    let write_bps = total as f64 / t.elapsed().as_secs_f64().max(1e-6);
    let t = Instant::now();
    for (bi, batch) in slots.chunks(per_batch).enumerate() {
        let at = bi * per_batch * BLOCK as usize;
        store.read(&runs(batch), &mut back[at..at + batch.len() * BLOCK as usize]).map_err(e)?;
    }
    let read_bps = total as f64 / t.elapsed().as_secs_f64().max(1e-6);
    if let Some(i) = data.iter().zip(&back).position(|(a, b)| a != b) {
        return Err(format!(
            "проверка не прошла ({}): данные в области {} прочитались не так, как записались",
            store.kind().label(),
            crate::util::size(i as u64)
        ));
    }
    Ok(SelfTest { write_bps, read_bps, checked_bytes: total as u64, ms: 0 })
}

/// Хранилище кусками в куче процесса: для тестов и демо-режима. Умеет
/// изображать отказы драйвера и мусор в новой памяти.
pub struct SimStore {
    kind: Kind,
    ca: ChunkAlloc,
    chunks: Vec<Option<Vec<u8>>>,
    limit: u64,
    /// В новой и освобождённой памяти — мусор (как в видеопамяти).
    pub garbage: bool,
    /// Отказывать в новых кусках («видеопамять занята»).
    pub refuse: Option<NoRoom>,
    /// Не больше стольких кусков (имитация бюджета).
    pub budget_chunks: Option<usize>,
    pub max_io: usize,
    /// Пауза при выделении куска (демо).
    pub alloc_delay: Duration,
    /// Не хранить данные — только учёт (демо: не занимать гигабайты).
    pub phantom: bool,
    /// Считать занятой памятью только слоты с данными, как у оперативной
    /// памяти Windows (`RamStore`), а не целые куски (демо).
    pub per_block: bool,
}

impl SimStore {
    /// `capacity`, `limit`, `chunk` — в байтах.
    pub fn new(kind: Kind, capacity: u64, limit: u64, chunk: u64) -> Self {
        let per = (chunk / BLOCK).max(64) / 64 * 64;
        let ca = ChunkAlloc::new(capacity.div_ceil(BLOCK), per, limit.div_ceil(BLOCK));
        let n = ca.chunks();
        Self {
            kind,
            limit: ca.soft() as u64 * per * BLOCK,
            ca,
            chunks: (0..n).map(|_| None).collect(),
            garbage: true,
            refuse: None,
            budget_chunks: None,
            max_io: usize::MAX,
            alloc_delay: Duration::ZERO,
            phantom: false,
            per_block: false,
        }
    }

    pub fn chunk_bytes(&self) -> u64 {
        self.ca.per() * BLOCK
    }

    pub fn live_chunks(&self) -> usize {
        self.ca.live_count()
    }

    fn split(&self, addr: u64, len: u64) -> Vec<(usize, usize, usize)> {
        let cb = self.chunk_bytes();
        let mut out = Vec::new();
        let (mut p, end) = (addr, addr + len);
        while p < end {
            let c = (p / cb) as usize;
            let local = p % cb;
            let l = (cb - local).min(end - p);
            out.push((c, local as usize, l as usize));
            p += l;
        }
        out
    }

    fn chunk(&mut self, c: usize) -> Result<&mut Vec<u8>, DiskError> {
        self.chunks[c].as_mut().ok_or_else(|| DiskError::Device(format!("обращение к невыделенному куску {c}")))
    }
}

impl Store for SimStore {
    fn kind(&self) -> Kind {
        self.kind
    }
    fn limit(&self) -> u64 {
        self.limit
    }
    fn alloc(&mut self, over: bool) -> Result<Slot, NoRoom> {
        if let Some(s) = self.ca.take_live() {
            return Ok(Slot { index: s, zeroed: false });
        }
        let c = self.ca.next_dead(over).ok_or(NoRoom::Limit)?;
        if let Some(r) = &self.refuse {
            return Err(r.clone());
        }
        if self.budget_chunks.is_some_and(|b| self.ca.live_count() >= b) {
            return Err(NoRoom::Budget("видеопамять занята другими программами".into()));
        }
        if !self.alloc_delay.is_zero() {
            std::thread::sleep(self.alloc_delay);
        }
        let fill = if self.garbage { 0xA5 } else { 0 };
        self.chunks[c] = Some(if self.phantom { Vec::new() } else { vec![fill; self.chunk_bytes() as usize] });
        self.ca.set_live(c);
        Ok(Slot { index: self.ca.take_in(c), zeroed: false })
    }
    fn free(&mut self, slot: u64) {
        self.ca.release(slot);
        if self.garbage && !self.phantom {
            let cb = self.chunk_bytes();
            let (c, local) = ((slot * BLOCK / cb) as usize, (slot * BLOCK % cb) as usize);
            if let Some(ch) = self.chunks[c].as_mut() {
                ch[local..local + BLOCK as usize].fill(0x5A);
            }
        }
    }
    fn used(&self) -> u64 {
        self.ca.used()
    }
    fn held(&self) -> u64 {
        if self.per_block {
            self.ca.used() * BLOCK
        } else {
            self.ca.live_count() as u64 * self.chunk_bytes()
        }
    }
    fn read(&mut self, runs: &[(u64, u64, usize)], out: &mut [u8]) -> Result<(), DiskError> {
        let phantom = self.phantom;
        for &(a, l, o) in runs {
            let mut o = o;
            for (c, local, len) in self.split(a, l) {
                let ch = self.chunk(c)?;
                if phantom {
                    out[o..o + len].fill(0x3C);
                    o += len;
                    continue;
                }
                out[o..o + len].copy_from_slice(&ch[local..local + len]);
                o += len;
            }
        }
        Ok(())
    }
    fn write(&mut self, fills: &[(u64, u64)], runs: &[(u64, u64, usize)], data: &[u8]) -> Result<(), DiskError> {
        if self.phantom {
            for &(a, l, _) in runs {
                for (c, _, _) in self.split(a, l) {
                    self.chunk(c)?;
                }
            }
            return Ok(());
        }
        for &(a, l) in fills {
            for (c, local, len) in self.split(a, l) {
                self.chunk(c)?[local..local + len].fill(0);
            }
        }
        for &(a, l, o) in runs {
            let mut o = o;
            for (c, local, len) in self.split(a, l) {
                self.chunk(c)?[local..local + len].copy_from_slice(&data[o..o + len]);
                o += len;
            }
        }
        Ok(())
    }
    fn max_io(&self) -> usize {
        self.max_io
    }
    fn reap(&mut self, now: Instant, force: bool) -> u64 {
        let dead = self.ca.reap(now, force);
        for &c in &dead {
            self.chunks[c] = None;
        }
        dead.len() as u64 * self.chunk_bytes()
    }
    fn plan_compaction(&mut self, max: usize) -> Vec<(u64, u64)> {
        self.ca.plan_compaction(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_alloc_prefers_low_chunks_and_reaps() {
        let mut ca = ChunkAlloc::new(64 * 4, 64, 64 * 2);
        assert_eq!(ca.take_live(), None);
        assert_eq!(ca.next_dead(false), Some(0));
        ca.set_live(0);
        let a: Vec<u64> = (0..64).map(|_| ca.take_live().unwrap()).collect();
        assert_eq!(a, (0..64).collect::<Vec<_>>());
        assert_eq!(ca.take_live(), None);
        ca.set_live(1);
        assert_eq!(ca.take_live(), Some(64));
        // предел — два куска; сверх него только аварийно
        assert_eq!(ca.next_dead(false), None);
        assert_eq!(ca.next_dead(true), Some(2));
        // освободили слот внизу — он и выдаётся
        ca.release(5);
        assert_eq!(ca.take_live(), Some(5));
        // кусок 1 пустеет и возвращается только после паузы
        ca.release(64);
        assert!(ca.reap(Instant::now(), false).is_empty());
        assert_eq!(ca.reap(Instant::now() + REAP_DELAY, false), vec![1]);
        assert_eq!(ca.live_count(), 1);
    }

    #[test]
    fn compaction_plan_moves_top_chunk_into_holes() {
        let mut ca = ChunkAlloc::new(64 * 3, 64, 64 * 3);
        for c in 0..3 {
            ca.set_live(c);
            for _ in 0..64 {
                ca.take_in(c);
            }
        }
        // в кусках 0 и 1 освободили по 40 слотов, в куске 2 осталось 10
        for s in 0..40 {
            ca.release(s);
            ca.release(64 + s);
        }
        for s in 128..182 {
            ca.release(s);
        }
        assert_eq!(ca.used_in(2), 10);
        let moves = ca.plan_compaction(100);
        assert_eq!(moves.len(), 10);
        assert!(moves.iter().all(|&(s, d)| s >= 128 && d < 40));
        for &(s, _) in &moves {
            ca.release(s);
        }
        assert_eq!(ca.used_in(2), 0);
        // мало дыр — упаковывать нечего
        let mut ca = ChunkAlloc::new(128, 64, 128);
        ca.set_live(0);
        ca.set_live(1);
        for _ in 0..100 {
            ca.take_live().unwrap();
        }
        assert!(ca.plan_compaction(100).is_empty());
    }

    #[test]
    fn zero_check() {
        let mut v = vec![0u8; 70000];
        assert!(is_zero(&v));
        assert!(is_zero(&v[3..65539]));
        v[69999] = 1;
        assert!(!is_zero(&v));
        assert!(is_zero(&v[..69999]));
        v[1] = 2;
        assert!(!is_zero(&v[1..2]));
        assert!(is_zero(&[]));
    }

    #[test]
    fn sim_store_self_test() {
        let mut s = SimStore::new(Kind::Vram, 64 << 20, 32 << 20, 4 << 20);
        s.max_io = 1 << 20;
        let t = self_test(&mut s, 8 << 20).unwrap();
        assert_eq!(t.checked_bytes, 8 << 20);
        assert_eq!(s.used(), 0);
        assert_eq!(s.held(), 0, "после проверки куски возвращены");
    }
}
