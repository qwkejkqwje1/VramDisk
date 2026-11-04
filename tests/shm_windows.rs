//! Сервер ImDisk через общую память, проверенный «драйвером» из
//! пользовательского режима: те же объекты и тот же порядок, что у
//! sys_proxy.cpp (заголовок → данные → `_Request` → ждать `_Response`).
//! Запускается под Windows (или Wine).
#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;
use vramdisk::disk::HybridDisk;
use vramdisk::pool::{Kind, SimStore};
use vramdisk::proxy::{get, put, req, EINVAL, FLAG_SUPPORTS_UNMAP, FLAG_SUPPORTS_ZERO, HEADER_SIZE};
use vramdisk::ram::RamStore;
use vramdisk::stats::Stats;
use vramdisk::win::shm::{ExitReason, ShmServer};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, WaitForSingleObject, EVENT_ALL_ACCESS};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Client {
    map: HANDLE,
    view: *mut u8,
    len: usize,
    req: HANDLE,
    resp: HANDLE,
}

impl Client {
    fn open(name: &str, len: usize) -> Self {
        unsafe {
            let map = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide(&format!("Global\\{name}")).as_ptr());
            assert!(!map.is_null(), "секция не открылась");
            let v = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            assert!(!v.Value.is_null(), "секция не отобразилась");
            let req = OpenEventW(EVENT_ALL_ACCESS, 0, wide(&format!("Global\\{name}_Request")).as_ptr());
            let resp = OpenEventW(EVENT_ALL_ACCESS, 0, wide(&format!("Global\\{name}_Response")).as_ptr());
            assert!(!req.is_null() && !resp.is_null(), "события не открылись");
            Self { map, view: v.Value as *mut u8, len, req, resp }
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn shm(&self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.view, self.len) }
    }

    fn call(&self, hdr: &[u64], payload: &[u8]) -> (u64, u64) {
        let shm = self.shm();
        for (i, v) in hdr.iter().enumerate() {
            put(shm, i * 8, *v);
        }
        shm[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(payload);
        unsafe {
            SetEvent(self.req);
            assert_eq!(WaitForSingleObject(self.resp, 10_000), WAIT_OBJECT_0, "сервер не ответил");
        }
        (get(shm, 0), get(shm, 8))
    }

    fn data(&self, len: usize) -> &[u8] {
        &self.shm()[HEADER_SIZE..HEADER_SIZE + len]
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ });
            CloseHandle(self.req);
            CloseHandle(self.resp);
            CloseHandle(self.map);
        }
    }
}

const MIB: u64 = 1 << 20;

/// Диск `ram + vram`: оперативная память (настоящая, `VirtualAlloc`) и
/// «видеопамять» кусками по 4 МиБ с мусором в новой памяти.
fn disk(stats: &Arc<Stats>, ram: u64, vram: u64) -> HybridDisk {
    let size = ram + vram;
    let mut d = HybridDisk::new(size, stats.clone());
    d.add(Box::new(RamStore::new(size, ram).unwrap()));
    if vram > 0 {
        let mut v = SimStore::new(Kind::Vram, size, vram, 4 * MIB);
        v.max_io = 256 * 1024;
        d.add(Box::new(v));
    }
    d
}

#[test]
fn shm_server_roundtrip() {
    let stats = Arc::new(Stats::default());
    let name = format!("VramDiskTest{}", std::process::id());
    let buf = MIB as usize;
    let server = ShmServer::start(&name, disk(&stats, 32 * MIB, 32 * MIB), buf).expect("сервер");
    // Второй сервер с тем же именем не должен запуститься.
    assert!(ShmServer::start(&name, disk(&Arc::new(Stats::default()), MIB, 0), buf).is_err());

    let c = Client::open(&name, HEADER_SIZE + buf);

    // INFO: размер, выравнивание, поддержка TRIM и обнуления.
    let shm = c.shm();
    put(shm, 0, req::INFO);
    unsafe {
        SetEvent(c.req);
        assert_eq!(WaitForSingleObject(c.resp, 10_000), WAIT_OBJECT_0);
    }
    assert_eq!(get(shm, 0), 64 * MIB);
    assert_eq!(get(shm, 8), 1);
    assert_eq!(get(shm, 16), FLAG_SUPPORTS_UNMAP | FLAG_SUPPORTS_ZERO);

    // Пустой диск памяти не занимает.
    assert_eq!((stats.snapshot().held_ram, stats.snapshot().held_vram), (0, 0));
    // Оперативную память (32 МиБ) заполняем почти до предела...
    let fill: Vec<u8> = vec![0x11; buf];
    for i in 0..31 {
        assert_eq!(c.call(&[req::WRITE, 32 * MIB + i * MIB, buf as u64], &fill), (0, buf as u64));
    }
    // ...и пишем 1 МиБ, который ляжет и в оперативную память, и в видеопамять.
    let off = 3 * MIB + 12345;
    let data: Vec<u8> = (0..buf).map(|i| (i * 7 + 3) as u8).collect();
    assert_eq!(c.call(&[req::WRITE, off, buf as u64], &data), (0, buf as u64));
    let s = stats.snapshot();
    assert_eq!(s.used_ram, 32 * MIB);
    assert_eq!(s.used_vram, 17 * 64 * 1024 - MIB, "в видеопамять ушло то, что не влезло");
    c.shm()[HEADER_SIZE..HEADER_SIZE + buf].fill(0xEE);
    assert_eq!(c.call(&[req::READ, off, buf as u64], &[]), (0, buf as u64));
    assert!(c.data(buf) == &data[..], "прочитано не то, что записано");

    // TRIM выровненного куска: читаются нули, память возвращается.
    let t_off = 4 * MIB - 128 * 1024;
    let mut ranges = Vec::new();
    ranges.extend_from_slice(&(t_off as i64).to_le_bytes());
    ranges.extend_from_slice(&(256u64 * 1024).to_le_bytes());
    assert_eq!(c.call(&[req::UNMAP, 16], &ranges).0, 0);
    assert_eq!(c.call(&[req::READ, t_off, 256 * 1024], &[]), (0, 256 * 1024));
    assert!(c.data(256 * 1024).iter().all(|&b| b == 0), "после TRIM не нули");
    // Из четырёх накрытых блоков память была у трёх (4-й не записывался).
    let s = stats.snapshot();
    assert_eq!(s.trimmed, 3 * 64 * 1024);
    assert_eq!(s.used_ram + s.used_vram, 32 * MIB + 64 * 1024 - 3 * 64 * 1024);
    // Соседние данные целы.
    assert_eq!(c.call(&[req::READ, off, 64 * 1024], &[]), (0, 64 * 1024));
    assert!(c.data(64 * 1024) == &data[..64 * 1024]);

    // ZERO невыровненного куска.
    let z_off = off + 1000;
    let mut ranges = Vec::new();
    ranges.extend_from_slice(&(z_off as i64).to_le_bytes());
    ranges.extend_from_slice(&5000u64.to_le_bytes());
    assert_eq!(c.call(&[req::ZERO, 16], &ranges).0, 0);
    assert_eq!(c.call(&[req::READ, off, 8192], &[]), (0, 8192));
    let got = c.data(8192);
    assert!(got[..1000] == data[..1000] && got[1000..6000].iter().all(|&b| b == 0) && got[6000..8192] == data[6000..8192]);

    // Ошибки: больше буфера, за концом диска.
    assert_eq!(c.call(&[req::READ, 0, buf as u64 + 4096], &[]).0, EINVAL);
    assert_eq!(c.call(&[req::WRITE, 64 * MIB - 4096, 8192], &data[..8192]).0, EINVAL);
    // Чтение у самого конца — короткое.
    assert_eq!(c.call(&[req::READ, 64 * MIB - 4096, 8192], &[]), (0, 4096));

    let s = stats.snapshot();
    assert!(s.requests >= 10, "запросов: {}", s.requests);

    // CLOSE: сервер завершается сам, без ответа.
    put(c.shm(), 0, req::CLOSE);
    unsafe { SetEvent(c.req) };
    assert_eq!(server.finish(Duration::from_secs(10)), Some(ExitReason::Closed));
    assert!(stats.snapshot().server_stopped);
}

#[test]
fn shm_server_stop_without_driver() {
    let stats = Arc::new(Stats::default());
    let name = format!("VramDiskTestStop{}", std::process::id());
    let server = ShmServer::start(&name, disk(&stats, 4 * MIB, 0), 256 * 1024).expect("сервер");
    assert!(server.is_running());
    assert!(!server.connected());
    // Драйвер так и не подключился (imdisk не смог создать диск).
    assert_eq!(server.finish(Duration::from_millis(100)), Some(ExitReason::Stopped));
    // Имя освободилось — можно запустить снова.
    let again = ShmServer::start(&name, disk(&stats, 4 * MIB, 0), 256 * 1024).expect("повторный запуск");
    assert_eq!(again.finish(Duration::from_millis(50)), Some(ExitReason::Stopped));
}
