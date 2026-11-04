//! Журнал работы: файл `vramdisk.log` и последние строки для окна «Журнал».
//!
//! Пишется из любого потока и из двух процессов сразу: окна и фоновой части
//! (её строки помечены «[фон]»). Каждая строка уходит в файл одной записью в
//! конец, а файл время от времени открывается заново — так строки не
//! перемешиваются, и после переименования журнала другим процессом запись
//! идёт в новый файл. Файл больше 1 МБ при запуске переименовывается в
//! `vramdisk.old.log`, так что журнал не растёт бесконечно.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

const KEEP: usize = 300;
const MAX_FILE: u64 = 1 << 20;
/// Как часто открывать файл заново.
const REOPEN: Duration = Duration::from_secs(1);

struct Log {
    file: Option<std::fs::File>,
    /// Когда файл открывался в последний раз.
    opened: Option<Instant>,
    path: Option<PathBuf>,
    recent: VecDeque<String>,
    /// Почему журнал не пишется в файл.
    problem: Option<String>,
    /// Пометка процесса в каждой строке («фон»).
    tag: Option<&'static str>,
}

static LOG: Mutex<Log> = Mutex::new(Log { file: None, opened: None, path: None, recent: VecDeque::new(), problem: None, tag: None });

fn lock() -> MutexGuard<'static, Log> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// Пометка для строк этого процесса (вызывать до `init`).
pub fn set_tag(tag: &'static str) {
    lock().tag = Some(tag);
}

/// Открывает файл журнала `vramdisk.log` в папке `dir`.
pub fn init(dir: &Path) -> Result<(), String> {
    let path = dir.join("vramdisk.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("vramdisk.old.log"));
    }
    let r = open(&path);
    let mut l = lock();
    l.path = Some(path.clone());
    match r {
        Ok(f) => {
            l.file = Some(f);
            l.opened = Some(Instant::now());
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
    let mut l = lock();
    let line = match l.tag {
        Some(t) => format!("{} [{t}] {}", timestamp(), msg.as_ref()),
        None => format!("{} {}", timestamp(), msg.as_ref()),
    };
    if let Some(path) = l.path.clone() {
        // Открыть заново раз в секунду (и не чаще, если не открывается).
        if l.opened.is_none_or(|at| at.elapsed() >= REOPEN) {
            l.opened = Some(Instant::now());
            match open(&path) {
                Ok(f) => l.file = Some(f),
                Err(e) => {
                    l.problem = Some(format!("журнал не записывается в {}: {e}", path.display()));
                    l.file = None;
                }
            }
        }
        let nl = if cfg!(windows) { "\r\n" } else { "\n" };
        if let Some(f) = l.file.as_mut() {
            match f.write_all(format!("{line}{nl}").as_bytes()) {
                Ok(()) => l.problem = None,
                Err(e) => {
                    l.problem = Some(format!("журнал не записывается в {}: {e}", path.display()));
                    l.file = None;
                }
            }
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

/// Последние строки журнала этого процесса.
pub fn recent() -> Vec<String> {
    lock().recent.iter().cloned().collect()
}

/// Последние строки файла журнала — вместе со строками фоновой части.
/// Если файл не читается, — строки этого процесса.
pub fn tail() -> Vec<String> {
    let Some(path) = path() else { return recent() };
    match read_tail(&path, 96 << 10) {
        Some(t) => {
            let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
            lines[lines.len().saturating_sub(KEEP)..].iter().map(|l| l.to_string()).collect()
        }
        None => recent(),
    }
}

/// Конец файла (не больше `max` байт) без оборванной первой строки.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(i) = text.find('\n') {
            text.drain(..=i);
        }
    }
    Some(text)
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
