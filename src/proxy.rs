//! Протокол ImDisk proxy поверх общей памяти.
//!
//! Драйвер кладёт заголовок запроса в начало общей памяти (первые 4096 байт),
//! данные — сразу за ним, взводит событие `_Request` и ждёт `_Response`.
//! Здесь — разбор одного запроса и ответ; ожидание событий живёт в
//! `win::shm`, поэтому протокол проверяется тестами и на Linux.

use crate::disk::{DiskError, HybridDisk};
use std::sync::atomic::Ordering::Relaxed;

/// Размер области заголовка (IMDPROXY_HEADER_SIZE).
pub const HEADER_SIZE: usize = 4096;

pub mod req {
    pub const INFO: u64 = 1;
    pub const READ: u64 = 2;
    pub const WRITE: u64 = 3;
    pub const CONNECT: u64 = 4;
    pub const CLOSE: u64 = 5;
    pub const UNMAP: u64 = 6;
    pub const ZERO: u64 = 7;
}

pub const FLAG_SUPPORTS_UNMAP: u64 = 0x02;
pub const FLAG_SUPPORTS_ZERO: u64 = 0x04;

pub const EIO: u64 = 5;
pub const ENODEV: u64 = 19;
pub const EINVAL: u64 = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Ответ записан — взвести `_Response`.
    Reply,
    /// Драйвер отключился (диск снят), отвечать не нужно.
    Close,
}

#[inline]
pub fn get(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[inline]
pub fn put(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn errno(e: &DiskError) -> u64 {
    match e {
        DiskError::OutOfRange => EINVAL,
        _ => EIO,
    }
}

/// Обрабатывает запрос, лежащий в `shm` (заголовок + буфер данных).
pub fn handle(shm: &mut [u8], disk: &mut HybridDisk) -> Outcome {
    let (head, data) = shm.split_at_mut(HEADER_SIZE);
    let code = get(head, 0);
    disk.stats().requests.fetch_add(1, Relaxed);
    match code {
        req::INFO => {
            put(head, 0, disk.size());
            put(head, 8, 1);
            put(head, 16, FLAG_SUPPORTS_UNMAP | FLAG_SUPPORTS_ZERO);
            Outcome::Reply
        }
        req::READ => {
            let (off, len) = (get(head, 8), get(head, 16));
            let (err, done) = if len > data.len() as u64 || off > disk.size() {
                (EINVAL, 0)
            } else {
                // Чтение за концом диска — короткое, как у файла.
                let n = len.min(disk.size() - off);
                match disk.read(off, &mut data[..n as usize]) {
                    Ok(()) => (0, n),
                    Err(e) => (errno(&e), 0),
                }
            };
            put(head, 0, err);
            put(head, 8, done);
            Outcome::Reply
        }
        req::WRITE => {
            let (off, len) = (get(head, 8), get(head, 16));
            let (err, done) = if len > data.len() as u64 || off.checked_add(len).is_none_or(|e| e > disk.size()) {
                (EINVAL, 0)
            } else {
                match disk.write(off, &data[..len as usize]) {
                    Ok(()) => (0, len),
                    Err(e) => (errno(&e), 0),
                }
            };
            put(head, 0, err);
            put(head, 8, done);
            Outcome::Reply
        }
        req::UNMAP | req::ZERO => {
            let bytes = (get(head, 8) as usize).min(data.len());
            let mut err = 0;
            for i in 0..bytes / 16 {
                let off = get(data, i * 16) as i64;
                let len = get(data, i * 16 + 8);
                if off < 0 {
                    err = EINVAL;
                    break;
                }
                let r = if code == req::UNMAP { disk.trim(off as u64, len) } else { disk.zero(off as u64, len) };
                if let Err(e) = r {
                    err = errno(&e);
                    break;
                }
            }
            put(head, 0, err);
            Outcome::Reply
        }
        req::CLOSE => Outcome::Close,
        _ => {
            put(head, 0, ENODEV);
            Outcome::Reply
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{Kind, SimStore};
    use crate::ram::RamStore;
    use crate::stats::Stats;
    use std::sync::Arc;

    const BUF: usize = 256 * 1024;

    /// «Драйвер» для тестов: собирает запросы так же, как sys_proxy.cpp.
    struct Driver {
        shm: Vec<u8>,
        disk: HybridDisk,
    }

    impl Driver {
        /// Диск размером `ram + vram`: сначала заполняется оперативная
        /// память, затем «видеопамять» (кусками по 4 МиБ, с мусором в новой
        /// памяти, обращения не больше 64 КиБ — как промежуточный буфер).
        fn new(ram: u64, vram: u64) -> Self {
            let size = ram + vram;
            let mut disk = HybridDisk::new(size, Arc::new(Stats::default()));
            disk.add(Box::new(RamStore::new(size, ram).unwrap()));
            if vram > 0 {
                let mut v = SimStore::new(Kind::Vram, size, vram, 4 << 20);
                v.max_io = 64 * 1024;
                disk.add(Box::new(v));
            }
            Self { shm: vec![0; HEADER_SIZE + BUF], disk }
        }
        fn call(&mut self, hdr: &[u64], payload: &[u8]) -> Outcome {
            for (i, v) in hdr.iter().enumerate() {
                put(&mut self.shm, i * 8, *v);
            }
            self.shm[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(payload);
            handle(&mut self.shm, &mut self.disk)
        }
        fn info(&mut self) -> (u64, u64, u64) {
            assert_eq!(self.call(&[req::INFO], &[]), Outcome::Reply);
            (get(&self.shm, 0), get(&self.shm, 8), get(&self.shm, 16))
        }
        /// Как ImDiskReadProxy: режет по размеру буфера.
        fn read(&mut self, off: u64, len: usize) -> Result<Vec<u8>, u64> {
            let mut out = Vec::new();
            while out.len() < len {
                let l = (len - out.len()).min(BUF);
                self.call(&[req::READ, off + out.len() as u64, l as u64], &[]);
                let (err, got) = (get(&self.shm, 0), get(&self.shm, 8) as usize);
                if err != 0 {
                    return Err(err);
                }
                if got == 0 {
                    break;
                }
                out.extend_from_slice(&self.shm[HEADER_SIZE..HEADER_SIZE + got]);
            }
            Ok(out)
        }
        fn write(&mut self, off: u64, data: &[u8]) -> Result<(), u64> {
            for (i, c) in data.chunks(BUF).enumerate() {
                self.call(&[req::WRITE, off + (i * BUF) as u64, c.len() as u64], c);
                let err = get(&self.shm, 0);
                if err != 0 {
                    return Err(err);
                }
                assert_eq!(get(&self.shm, 8), c.len() as u64);
            }
            Ok(())
        }
        fn ranges(&mut self, code: u64, r: &[(u64, u64)]) -> u64 {
            let mut p = Vec::new();
            for &(o, l) in r {
                p.extend_from_slice(&o.to_le_bytes());
                p.extend_from_slice(&l.to_le_bytes());
            }
            self.call(&[code, p.len() as u64], &p);
            get(&self.shm, 0)
        }
    }

    fn pat(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn info_reports_size_and_flags() {
        let mut d = Driver::new(1 << 20, 3 << 20);
        let (size, align, flags) = d.info();
        assert_eq!(size, 4 << 20);
        assert_eq!(align, 1);
        assert_eq!(flags, FLAG_SUPPORTS_UNMAP | FLAG_SUPPORTS_ZERO);
    }

    #[test]
    fn write_read_across_ram_and_vram() {
        let mut d = Driver::new(1 << 20, 3 << 20);
        // 1,5 МиБ с невыровненного места: первые 16 блоков ложатся в
        // оперативную память (её предел — 1 МиБ), остальные — в видеопамять.
        let off = 3 * 64 * 1024 + 123;
        let data = pat(1536 * 1024, 7);
        d.write(off, &data).unwrap();
        assert_eq!(d.read(off, data.len()).unwrap(), data);
        // Соседние байты — нули: новый блок видеопамяти обнулён до записи.
        assert_eq!(d.read(off - 100, 100).unwrap(), vec![0; 100]);
        let after = d.read(off + data.len() as u64, 5000).unwrap();
        assert!(after.iter().all(|&b| b == 0));
        let s = d.disk.stats().snapshot();
        assert!(s.write_ram > 0 && s.write_vram > 0);
        assert_eq!(s.used_ram, 1 << 20);
        assert_eq!(s.used_vram, 25 * 64 * 1024 - (1 << 20));
        // Пустой диск памяти не занимает: память — только под записанное.
        assert_eq!(d.disk.mapped(), 25);
        d.disk.verify().unwrap();
    }

    #[test]
    fn trim_then_partial_write_gives_zeros() {
        let mut d = Driver::new(1 << 20, 1 << 20);
        let all = pat(2 << 20, 1);
        d.write(0, &all).unwrap();
        assert_eq!(d.disk.stats().snapshot().used_vram, 1 << 20);
        // TRIM всего диска: освобождённая «видеопамять» забита мусором.
        assert_eq!(d.ranges(req::UNMAP, &[(0, 2 << 20)]), 0);
        let s = d.disk.stats().snapshot();
        assert_eq!((s.used_ram, s.used_vram, s.trimmed), (0, 0, 2 << 20));
        assert!(d.read(0, 2 << 20).unwrap().iter().all(|&b| b == 0));
        // Частичная запись в блок видеопамяти с мусором: остаток блока — нули.
        d.disk.set_first(Kind::Vram);
        d.write((1 << 20) + 1000, b"hello").unwrap();
        assert_eq!(d.disk.stats().snapshot().used_vram, 64 * 1024);
        let blk = d.read(1 << 20, 64 * 1024).unwrap();
        assert_eq!(&blk[1000..1005], b"hello");
        assert!(blk[..1000].iter().chain(&blk[1005..]).all(|&b| b == 0));
    }

    #[test]
    fn zero_is_exact() {
        let mut d = Driver::new(1 << 20, 1 << 20);
        d.write(0, &vec![0xFF; 2 << 20]).unwrap();
        // Обнуление с невыровненными краями: целые блоки внутри освобождаются.
        let (zo, zl) = (5000u64, 300_000u64);
        assert_eq!(d.ranges(req::ZERO, &[(zo, zl), ((1 << 20) + 7, 10)]), 0);
        let all = d.read(0, 2 << 20).unwrap();
        for (i, &b) in all.iter().enumerate() {
            let i = i as u64;
            let zeroed = (zo..zo + zl).contains(&i) || ((1 << 20) + 7..(1 << 20) + 17).contains(&i);
            assert_eq!(b, if zeroed { 0 } else { 0xFF }, "байт {i}");
        }
        // Отрезок 5000..305000 целиком накрывает блоки 1–3.
        assert_eq!(d.disk.mapped(), 32 - 3);
        d.disk.verify().unwrap();
    }

    #[test]
    fn zeros_take_no_memory() {
        let mut d = Driver::new(1 << 20, 1 << 20);
        // Нули в пустой диск — память не нужна.
        d.write(0, &vec![0; 1 << 20]).unwrap();
        assert_eq!(d.disk.mapped(), 0);
        // Блок, целиком перезаписанный нулями, освобождается.
        d.write(0, &pat(256 * 1024, 3)).unwrap();
        assert_eq!(d.disk.mapped(), 4);
        d.write(64 * 1024, &vec![0; 128 * 1024]).unwrap();
        assert_eq!(d.disk.mapped(), 2);
        assert!(d.read(64 * 1024, 128 * 1024).unwrap().iter().all(|&b| b == 0));
        d.disk.verify().unwrap();
    }

    #[test]
    fn bad_requests() {
        let mut d = Driver::new(1 << 20, 0);
        // запись за концом
        d.call(&[req::WRITE, (1 << 20) - 10, 20], &[1; 20]);
        assert_eq!(get(&d.shm, 0), EINVAL);
        // длина больше буфера
        d.call(&[req::READ, 0, (BUF + 1) as u64], &[]);
        assert_eq!(get(&d.shm, 0), EINVAL);
        // чтение у конца — короткое
        d.call(&[req::READ, (1 << 20) - 10, 20], &[]);
        assert_eq!((get(&d.shm, 0), get(&d.shm, 8)), (0, 10));
        // неизвестный запрос
        d.call(&[req::CONNECT], &[]);
        assert_eq!(get(&d.shm, 0), ENODEV);
        assert_eq!(d.call(&[req::CLOSE], &[]), Outcome::Close);
    }
}
