//! Оперативная память для диска.
//!
//! Под весь диск сразу резервируется только адресное пространство — это
//! ничего не стоит. Память под блок 64 КиБ берётся у системы при первой
//! записи в него (Windows — `MEM_COMMIT`) и возвращается, когда блок
//! освобождается (`MEM_DECOMMIT`). Поэтому диск занимает ровно столько
//! оперативной памяти, сколько на нём данных.

use crate::disk::DiskError;
use crate::pool::{Kind, NoRoom, Slot, Store, BLOCK};

pub struct RamStore {
    ptr: *mut u8,
    /// Слотов в зарезервированном адресном пространстве.
    cap: u64,
    /// Предел пользователя, слотов.
    limit: u64,
    used: u64,
    /// Слоты от `next` и выше ни разу не выдавались.
    next: u64,
    free: Vec<u32>,
}

// Память принадлежит только этому объекту, доступ — из одного потока сервера.
unsafe impl Send for RamStore {}

impl RamStore {
    /// `capacity` — сколько адресного пространства зарезервировать (байт;
    /// обычно весь диск — на случай, если видеопамять откажет), `limit` —
    /// предел пользователя.
    pub fn new(capacity: u64, limit: u64) -> Result<Self, String> {
        let cap = capacity.div_ceil(BLOCK).max(1);
        if cap > u32::MAX as u64 {
            return Err("слишком большой диск".into());
        }
        let ptr = sys::reserve(cap * BLOCK).ok_or_else(|| {
            format!("не удалось зарезервировать {} адресного пространства", crate::util::size(cap * BLOCK))
        })?;
        Ok(Self { ptr, cap, limit: limit / BLOCK, used: 0, next: 0, free: Vec::new() })
    }

    fn at(&self, addr: u64, len: u64) -> *mut u8 {
        assert!(addr + len <= self.cap * BLOCK, "адрес за пределами хранилища");
        unsafe { self.ptr.add(addr as usize) }
    }
}

impl Drop for RamStore {
    fn drop(&mut self) {
        sys::release(self.ptr, self.cap * BLOCK);
    }
}

impl Store for RamStore {
    fn kind(&self) -> Kind {
        Kind::Ram
    }

    fn limit(&self) -> u64 {
        self.limit * BLOCK
    }

    fn alloc(&mut self, over: bool) -> Result<Slot, NoRoom> {
        if !over && self.used >= self.limit {
            return Err(NoRoom::Limit);
        }
        let slot = match self.free.pop() {
            Some(s) => s as u64,
            None if self.next < self.cap => {
                self.next += 1;
                self.next - 1
            }
            None => return Err(NoRoom::Limit),
        };
        if let Err(e) = unsafe { sys::commit(self.at(slot * BLOCK, BLOCK), BLOCK) } {
            self.free.push(slot as u32);
            return Err(NoRoom::Memory(format!("система не дала оперативной памяти ({e})")));
        }
        self.used += 1;
        // Только что выданная системой память всегда заполнена нулями.
        Ok(Slot { index: slot, zeroed: true })
    }

    fn free(&mut self, slot: u64) {
        unsafe { sys::decommit(self.at(slot * BLOCK, BLOCK), BLOCK) };
        self.free.push(slot as u32);
        self.used -= 1;
    }

    fn used(&self) -> u64 {
        self.used
    }

    fn held(&self) -> u64 {
        self.used * BLOCK
    }

    fn read(&mut self, runs: &[(u64, u64, usize)], out: &mut [u8]) -> Result<(), DiskError> {
        for &(a, l, o) in runs {
            let src = unsafe { std::slice::from_raw_parts(self.at(a, l), l as usize) };
            out[o..o + l as usize].copy_from_slice(src);
        }
        Ok(())
    }

    fn write(&mut self, fills: &[(u64, u64)], runs: &[(u64, u64, usize)], data: &[u8]) -> Result<(), DiskError> {
        for &(a, l) in fills {
            unsafe { std::ptr::write_bytes(self.at(a, l), 0, l as usize) };
        }
        for &(a, l, o) in runs {
            let dst = unsafe { std::slice::from_raw_parts_mut(self.at(a, l), l as usize) };
            dst.copy_from_slice(&data[o..o + l as usize]);
        }
        Ok(())
    }

    fn copy(&mut self, moves: &[(u64, u64)]) -> Result<(), DiskError> {
        for &(s, d) in moves {
            unsafe { std::ptr::copy_nonoverlapping(self.at(s * BLOCK, BLOCK), self.at(d * BLOCK, BLOCK), BLOCK as usize) };
        }
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
    };

    pub fn reserve(len: u64) -> Option<*mut u8> {
        let p = unsafe { VirtualAlloc(std::ptr::null(), len as usize, MEM_RESERVE, PAGE_NOACCESS) };
        (!p.is_null()).then_some(p as *mut u8)
    }

    pub fn release(p: *mut u8, _len: u64) {
        unsafe { VirtualFree(p as _, 0, MEM_RELEASE) };
    }

    pub unsafe fn commit(p: *mut u8, len: u64) -> Result<(), String> {
        if VirtualAlloc(p as _, len as usize, MEM_COMMIT, PAGE_READWRITE).is_null() {
            Err(crate::win::last_error())
        } else {
            Ok(())
        }
    }

    pub unsafe fn decommit(p: *mut u8, len: u64) {
        VirtualFree(p as _, len as usize, MEM_DECOMMIT);
    }
}

#[cfg(unix)]
mod sys {
    pub fn reserve(len: u64) -> Option<*mut u8> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        (p != libc::MAP_FAILED).then_some(p as *mut u8)
    }

    pub fn release(p: *mut u8, len: u64) {
        unsafe { libc::munmap(p as _, len as usize) };
    }

    /// Linux выдаёт страницы при первом обращении сам.
    pub unsafe fn commit(_p: *mut u8, _len: u64) -> Result<(), String> {
        Ok(())
    }

    /// После `MADV_DONTNEED` страницы читаются нулями.
    pub unsafe fn decommit(p: *mut u8, len: u64) {
        libc::madvise(p as _, len as usize, libc::MADV_DONTNEED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_on_demand_and_limit() {
        let mut r = RamStore::new(16 * BLOCK, 4 * BLOCK).unwrap();
        let slots: Vec<u64> = (0..4).map(|_| r.alloc(false).unwrap().index).collect();
        assert_eq!(slots, vec![0, 1, 2, 3]);
        assert_eq!(r.alloc(false), Err(NoRoom::Limit));
        let s = r.alloc(true).unwrap();
        assert!(s.zeroed);
        assert_eq!(r.held(), 5 * BLOCK);
        assert_eq!(r.over(), BLOCK);
        let data = vec![7u8; BLOCK as usize];
        r.write(&[], &[(2 * BLOCK, BLOCK, 0)], &data).unwrap();
        r.free(2);
        // освобождённый слот выдаётся снова и читается нулями
        let s = r.alloc(true).unwrap();
        assert_eq!(s.index, 2);
        let mut out = vec![1u8; BLOCK as usize];
        r.read(&[(2 * BLOCK, BLOCK, 0)], &mut out).unwrap();
        assert!(out.iter().all(|&b| b == 0));
    }
}
