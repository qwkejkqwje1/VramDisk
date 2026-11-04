//! Журнал работы: файл `vramdisk.log` и последние строки для окна «Журнал».
//!
//! Пишется из любого потока. Файл больше 1 МБ при запуске переименовывается
//! в `vramdisk.old.log`, так что журнал не растёт бесконечно.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const KEEP: usize = 300;
const MAX_FILE: u64 = 1 << 20;

struct Log {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    recent: VecDeque<String>,
    /// Почему журнал не пишется в файл.
    problem: Option<String>,
}

static LOG: Mutex<Log> = Mutex::new(Log { file: None, path: None, recent: VecDeque::new(), problem: None });

fn lock() -> MutexGuard<'static, Log> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// Открывает файл журнала `vramdisk.log` в папке `dir`.
pub fn init(dir: &Path) -> Result<(), String> {
    let path = dir.join("vramdisk.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("vramdisk.old.log"));
    }
    let r = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    let mut l = lock();
    l.path = Some(path.clone());
    match r {
        Ok(f) => {
            l.file = Some(f);
            l.problem = None;
            Ok(())
        }
        Err(e) => {
            let msg = format!("журнал не записывается в {}: {e}", path.display());
            l.problem = Some(msg.clone());
            Err(msg)
        }
    }
}

/// Почему журнал не пишется в файл (если не пишется).
pub fn problem() -> Option<String> {
    lock().problem.clone()
}

pub fn log(msg: impl AsRef<str>) {
    let line = format!("{} {}", timestamp(), msg.as_ref());
    let mut l = lock();
    if let Some(f) = l.file.as_mut() {
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        if let Err(e) = write!(f, "{line}{nl}").and_then(|_| f.flush()) {
            let path = l.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
            l.problem = Some(format!("журнал не записывается в {path}: {e}"));
            l.file = None;
        }
    }
    if cfg!(debug_assertions) {
        eprintln!("{line}");
    }
    l.recent.push_back(line);
    while l.recent.len() > KEEP {
        l.recent.pop_front();
    }
}

/// Последние строки журнала (для интерфейса).
pub fn recent() -> Vec<String> {
    lock().recent.iter().cloned().collect()
}

pub fn path() -> Option<PathBuf> {
    lock().path.clone()
}

/// Местное время «2025-01-31 12:34:56.789».
pub fn timestamp() -> String {
    let (y, mo, d, h, mi, s, ms) = local_time();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{ms:03}")
}

#[cfg(windows)]
fn local_time() -> (u32, u32, u32, u32, u32, u32, u32) {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    (
        t.wYear as u32,
        t.wMonth as u32,
        t.wDay as u32,
        t.wHour as u32,
        t.wMinute as u32,
        t.wSecond as u32,
        t.wMilliseconds as u32,
    )
}

#[cfg(unix)]
fn local_time() -> (u32, u32, u32, u32, u32, u32, u32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    (
        (tm.tm_year + 1900) as u32,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
        now.subsec_millis(),
    )
}
