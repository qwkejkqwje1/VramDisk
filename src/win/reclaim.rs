//! Проход очистки: возвращает память блоков, которые файловая система
//! считает свободными (файлы удалены).
//!
//! Windows сообщает об удалении только командой TRIM, а до диска ImDisk она
//! доходит не всегда. Поэтому раз в несколько секунд — если на диск что-то
//! писали — поток читает карту занятых кластеров тома
//! (FSCTL_GET_VOLUME_BITMAP) и передаёт диску блоки, целиком лежащие в
//! свободных кластерах. Диск освобождает их, только если в них ничего не
//! писали с начала прохода (см. `disk::FreeHint`).

use super::{error_text, wide};
use crate::disk::{free_blocks, FreeHint, Reclaim};
use crate::logger::log;
use crate::stats::Stats;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_MORE_DATA, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_RETRIEVAL_POINTER_BASE, FSCTL_GET_VOLUME_BITMAP, RETRIEVAL_POINTER_BASE, STARTING_LCN_INPUT_BUFFER,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// Первый проход — вскоре после подключения (убрать следы форматирования).
const FIRST: Duration = Duration::from_secs(5);
const INTERVAL: Duration = Duration::from_secs(10);
const STOP_WAIT: Duration = Duration::from_secs(3);

pub struct Reclaimer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Reclaimer {
    pub fn start(letter: char, fs: String, rc: Reclaim, stats: Arc<Stats>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("reclaim".into())
            .spawn(move || run(letter, &fs, rc, stats, s2))
            .ok();
        Self { stop, thread }
    }

    /// Остановить поток и дождаться: перед отключением диска у нас не должно
    /// быть открытых описателей тома. Ждём не дольше нескольких секунд: если
    /// вызов к тому завис, поток отпускаем — он закончится сам, когда вызов
    /// вернётся, а окно не должно замирать.
    pub fn stop(&mut self) {
        self.stop.store(true, SeqCst);
        if let Some(t) = self.thread.take() {
            let until = Instant::now() + STOP_WAIT;
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                log(format!(
                    "проход очистки не закончился за {} с — диск отключается без него",
                    STOP_WAIT.as_secs()
                ));
            }
        }
    }
}

impl Drop for Reclaimer {
    fn drop(&mut self) {
        self.stop();
    }
}

enum PassError {
    /// Файловая система или драйвер этого не умеют — больше не пытаться.
    Unsupported(String),
    /// Помеха, которая может пройти (том занят, временно недоступен).
    Again(String),
    /// Диска в программе уже нет.
    Gone,
}

fn run(letter: char, fs: &str, rc: Reclaim, stats: Arc<Stats>, stop: Arc<AtomicBool>) {
    let mut next = Instant::now() + FIRST;
    let mut seen = u64::MAX;
    let mut warned = false;
    let mut first = true;
    loop {
        while Instant::now() < next {
            if stop.load(SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        next = Instant::now() + INTERVAL;
        let s = stats.snapshot();
        let writes = s.write_ram + s.write_vram;
        if writes == seen || s.used_ram + s.used_vram == 0 {
            seen = writes;
            continue;
        }
        let t = Instant::now();
        match pass(letter, fs, &rc) {
            Ok(n) => {
                seen = writes;
                if first || warned || t.elapsed() > Duration::from_millis(500) {
                    log(format!(
                        "проход очистки {letter}: свободно {n} блоков по 64 КБ, {} мс",
                        t.elapsed().as_millis()
                    ));
                }
                first = false;
                warned = false;
            }
            Err(PassError::Unsupported(e)) => {
                log(format!(
                    "очистка {letter}: карта свободного места недоступна ({e}) — память удалённых файлов вернётся \
                     только по TRIM или после отключения диска"
                ));
                return;
            }
            Err(PassError::Again(e)) => {
                // Одна запись, пока не наладится; при отключении диска
                // ошибка ожидаема — молчим.
                if !warned && !stop.load(SeqCst) {
                    log(format!(
                        "очистка {letter}: {e} — повторю через {} с; пока не получится, память удалённых \
                         файлов возвращается только по TRIM",
                        INTERVAL.as_secs()
                    ));
                    warned = true;
                }
            }
            Err(PassError::Gone) => return,
        }
    }
}

struct Vol(HANDLE);

impl Drop for Vol {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn classify(what: &str, code: u32) -> PassError {
    match code {
        // ERROR_INVALID_FUNCTION, ERROR_ACCESS_DENIED, ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER
        1 | 5 | 50 | 87 => PassError::Unsupported(format!("{what}: {}", error_text(code))),
        _ => PassError::Again(format!("{what}: {}", error_text(code))),
    }
}

fn pass(letter: char, fs: &str, rc: &Reclaim) -> Result<u64, PassError> {
    // Номер прохода — до чтения карты: всё, что запишут после, не тронем.
    let pass = rc.begin();
    let root = wide(&format!("{letter}:\\"));
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) } == 0 {
        return Err(PassError::Again(format!("размер кластера неизвестен: {}", error_text(unsafe { GetLastError() }))));
    }
    let cluster = spc as u64 * bps as u64;
    let path = wide(&format!("\\\\.\\{letter}:"));
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        // Том могут как раз форматировать или отключать — не сдаёмся.
        return Err(PassError::Again(format!("том не открывается: {}", error_text(unsafe { GetLastError() }))));
    }
    let vol = Vol(h);
    let base = {
        let mut b = RETRIEVAL_POINTER_BASE::default();
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_GET_RETRIEVAL_POINTER_BASE,
                std::ptr::null(),
                0,
                &mut b as *mut _ as *mut _,
                std::mem::size_of::<RETRIEVAL_POINTER_BASE>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            b.FileAreaOffset.max(0) as u64 * bps as u64
        } else if fs.eq_ignore_ascii_case("NTFS") {
            // У NTFS кластеры начинаются с начала тома.
            0
        } else {
            return Err(classify("начало области кластеров", unsafe { GetLastError() }));
        }
    };
    let (bitmap, clusters) = read_bitmap(&vol, total as u64)?;
    drop(vol);
    let (bits, n) = free_blocks(rc.size, base, cluster, clusters, &bitmap);
    rc.tx.send(FreeHint { pass, bits }).map_err(|_| PassError::Gone)?;
    Ok(n)
}

/// Карта кластеров тома: (биты, сколько кластеров).
fn read_bitmap(vol: &Vol, expect: u64) -> Result<(Vec<u8>, u64), PassError> {
    const HDR: usize = 16;
    let mut bits: Vec<u8> = Vec::new();
    let mut lcn: i64 = 0;
    let mut total: Option<u64> = None;
    loop {
        let input = STARTING_LCN_INPUT_BUFFER { StartingLcn: lcn };
        let want = HDR + ((expect.saturating_sub(lcn as u64) / 8 + 64) as usize).min(16 << 20);
        let mut buf = vec![0u64; want.div_ceil(8)];
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_GET_VOLUME_BITMAP,
                &input as *const _ as *const _,
                std::mem::size_of::<STARTING_LCN_INPUT_BUFFER>() as u32,
                buf.as_mut_ptr() as *mut _,
                (buf.len() * 8) as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 { unsafe { GetLastError() } } else { 0 };
        if ok == 0 && err != ERROR_MORE_DATA {
            return Err(classify("карта кластеров", err));
        }
        if (ret as usize) < HDR {
            return Err(PassError::Again("карта кластеров: пустой ответ".into()));
        }
        let start = buf[0] as i64;
        let size = buf[1];
        let all = *total.get_or_insert(start as u64 + size);
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts((buf.as_ptr() as *const u8).add(HDR), ret as usize - HDR) };
        // Начало ответа выровнено на байт карты.
        let at = (start / 8) as usize;
        if bits.len() < at {
            bits.resize(at, 0xFF);
        }
        bits.truncate(at);
        bits.extend_from_slice(bytes);
        lcn = start + (bytes.len() as i64) * 8;
        if ok != 0 || lcn as u64 >= all || bytes.is_empty() {
            break;
        }
    }
    let total = total.unwrap_or(0);
    bits.truncate(total.div_ceil(8) as usize);
    Ok((bits, total))
}
