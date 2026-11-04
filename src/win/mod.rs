//! Всё, что зависит от Windows: сервер общей памяти для ImDisk, вызовы
//! imdisk.exe, системные сведения, автозапуск и реакция на выключение.

pub mod autostart;
pub mod d3d11;
pub mod imdisk;
pub mod reclaim;
pub mod service;
pub mod shm;
pub mod shutdown;
pub mod sys;

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
};

/// Строка с нулём в конце для функций Windows.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Строка из буфера UTF-16 (до первого нуля).
pub fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Текст системной ошибки на языке системы: «Отказано в доступе. (код 5)».
pub fn error_text(code: u32) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    };
    let msg = String::from_utf16_lossy(&buf[..n as usize]);
    let msg = msg.trim();
    if msg.is_empty() {
        format!("код {code}")
    } else {
        format!("{msg} (код {code})")
    }
}

pub fn last_error() -> String {
    error_text(unsafe { GetLastError() })
}

/// Текст консольной программы в кодировке OEM (cp866 на русской Windows).
pub fn oem_to_string(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(b).into_owned();
        }
        let mut w = vec![0u16; n as usize];
        MultiByteToWideChar(CP_OEMCP, 0, b.as_ptr(), b.len() as i32, w.as_mut_ptr(), n);
        String::from_utf16_lossy(&w)
    }
}

pub struct RunOutput {
    pub code: i32,
    pub text: String,
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Запускает консольную программу без окна и ждёт не дольше `timeout`.
pub fn run_hidden(exe: &Path, args: &[String], timeout: Duration) -> Result<RunOutput, String> {
    run_hidden_ex(exe, args, timeout, true)
}

/// То же; `log_output: false` — в журнал только код возврата (для длинного вывода).
pub fn run_hidden_ex(exe: &Path, args: &[String], timeout: Duration, log_output: bool) -> Result<RunOutput, String> {
    let name = exe.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("не удалось запустить {name}: {e}"))?;
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(s) = so.as_mut() {
            let _ = s.read_to_end(&mut v);
        }
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(s) = se.as_mut() {
            let _ = s.read_to_end(&mut v);
        }
        v
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("{name}: {e}")),
        }
    };
    let Some(status) = status else {
        // Потоки чтения не ждём: дочерний format.com мог унаследовать канал.
        crate::logger::log(format!("{name} {}: не завершился за {} с", args.join(" "), timeout.as_secs()));
        return Err(format!("{name} не ответил за {} с", timeout.as_secs()));
    };
    let mut text = oem_to_string(&t_out.join().unwrap_or_default());
    let err = oem_to_string(&t_err.join().unwrap_or_default());
    if !err.trim().is_empty() {
        text.push('\n');
        text.push_str(&err);
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let code = status.code().unwrap_or(-1);
    crate::logger::log(format!(
        "{name} {} → код {code} за {} мс: {}",
        args.join(" "),
        start.elapsed().as_millis(),
        if log_output { squash(&text) } else { format!("{} символов вывода", text.chars().count()) }
    ));
    Ok(RunOutput { code, text })
}

/// Сжимает многострочный вывод в одну строку для журнала.
pub fn squash(s: &str) -> String {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ")
}
