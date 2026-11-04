//! Именованный канал между окном и фоновой частью: одно сообщение-запрос —
//! одно сообщение-ответ. Открыть канал могут только администраторы и
//! система: окно и фоновая часть работают с правами администратора, а
//! обычные программы не должны управлять диском.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vramdisk::win::{last_error, sys, wide};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SEM_TIMEOUT, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    CallNamedPipeW, ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

/// Полный доступ — администраторам (BA) и системе (SY), остальным — ничего.
const SDDL: &str = "D:P(A;;GA;;;BA)(A;;GA;;;SY)";
const BUF: u32 = 64 << 10;
/// Больше ответы не бывают (состояние диска — несколько КБ).
const REPLY_MAX: usize = 1 << 20;

/// Имя канала: свой на каждый сеанс Windows (если вошли несколько человек).
pub fn default_name() -> String {
    format!(r"\\.\pipe\VramDisk.{}", sys::session_id())
}

pub struct PipeServer {
    name: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Handle(HANDLE);
unsafe impl Send for Handle {}

impl PipeServer {
    /// Создаёт канал (ошибка — сразу, если его не создать) и обслуживает
    /// запросы в отдельном потоке по одному.
    pub fn start(name: &str, handler: impl FnMut(&[u8]) -> Vec<u8> + Send + 'static) -> Result<Self, String> {
        let h = Handle(create(name)?);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pipe-server".into())
            .spawn(move || {
                let h = h;
                serve(h.0, handler, &s2)
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { name: name.to_string(), stop, thread: Some(thread) })
    }

    /// Закрывает канал. Поток ждёт подключения — будим его своим запросом.
    pub fn stop(mut self) {
        self.stop.store(true, SeqCst);
        let _ = call(&self.name, b"null", 500);
        if let Some(t) = self.thread.take() {
            let until = Instant::now() + Duration::from_secs(1);
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

fn create(name: &str) -> Result<HANDLE, String> {
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(SDDL).as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut())
            == 0
        {
            return Err(format!("права доступа к каналу: {}", last_error()));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let h = CreateNamedPipeW(
            wide(name).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            BUF,
            BUF,
            0,
            &sa,
        );
        let err = (h == INVALID_HANDLE_VALUE).then(last_error);
        LocalFree(sd as _);
        match err {
            Some(e) => Err(format!("канал {name}: {e}")),
            None => Ok(h),
        }
    }
}

fn serve(h: HANDLE, mut handler: impl FnMut(&[u8]) -> Vec<u8>, stop: &AtomicBool) {
    let mut buf = vec![0u8; BUF as usize];
    loop {
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED };
        if stop.load(SeqCst) {
            break;
        }
        if connected {
            if let Some(msg) = read_message(h, &mut buf) {
                // Сбой в обработчике не должен оставить окно без связи.
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&msg))).unwrap_or_default();
                let mut n = 0u32;
                unsafe {
                    WriteFile(h, out.as_ptr(), out.len() as u32, &mut n, std::ptr::null_mut());
                    FlushFileBuffers(h);
                }
            }
        } else {
            // Не крутиться впустую, если канал сломался.
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { DisconnectNamedPipe(h) };
    }
    unsafe {
        DisconnectNamedPipe(h);
        CloseHandle(h);
    }
}

fn read_message(h: HANDLE, buf: &mut [u8]) -> Option<Vec<u8>> {
    let mut msg = Vec::new();
    loop {
        let mut n = 0u32;
        let ok = unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) != 0 };
        msg.extend_from_slice(&buf[..n as usize]);
        if ok {
            return Some(msg);
        }
        if unsafe { GetLastError() } != ERROR_MORE_DATA || msg.len() > REPLY_MAX {
            return None;
        }
    }
}

/// Запрос — ответ. `timeout_ms` — сколько ждать, пока канал занят.
/// Ошибка — код Windows (2 — канала нет: фоновая часть не запущена).
pub fn call(name: &str, req: &[u8], timeout_ms: u32) -> Result<Vec<u8>, u32> {
    let name = wide(name);
    let mut out = vec![0u8; REPLY_MAX];
    let until = Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        let left = until.saturating_duration_since(Instant::now()).as_millis() as u32;
        let mut n = 0u32;
        let ok = unsafe {
            CallNamedPipeW(
                name.as_ptr(),
                req.as_ptr() as _,
                req.len() as u32,
                out.as_mut_ptr() as _,
                out.len() as u32,
                &mut n,
                left.max(1),
            )
        };
        if ok != 0 {
            out.truncate(n as usize);
            return Ok(out);
        }
        let e = unsafe { GetLastError() };
        // Канал занят другим запросом: CallNamedPipe ждёт освобождения лишь
        // однажды, а освободившийся канал может перехватить другой клиент.
        if !matches!(e, ERROR_PIPE_BUSY | ERROR_SEM_TIMEOUT) || Instant::now() >= until {
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_reply() {
        let name = format!(r"\\.\pipe\VramDisk.test.{}", std::process::id());
        let mut count = 0;
        let srv = PipeServer::start(&name, move |m| {
            count += 1;
            let mut v = m.to_vec();
            v.reverse();
            v.extend_from_slice(format!("#{count}").as_bytes());
            v
        })
        .unwrap();
        // Второй сервер с тем же именем не создаётся.
        assert!(PipeServer::start(&name, |m| m.to_vec()).is_err());
        assert_eq!(call(&name, b"abc", 1000).unwrap(), b"cba#1");
        // Большой ответ целиком.
        let big = vec![7u8; 300 << 10];
        assert_eq!(call(&name, &big, 1000).unwrap().len(), big.len() + 2);
        // Несколько клиентов сразу: каждый получает свой ответ.
        let ths: Vec<_> = (0..4)
            .map(|i| {
                let n = name.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        let r = call(&n, format!("{i}").as_bytes(), 5000).unwrap();
                        assert_eq!(r[0], b'0' + i as u8);
                    }
                })
            })
            .collect();
        for t in ths {
            t.join().unwrap();
        }
        srv.stop();
        assert_eq!(call(&name, b"x", 100), Err(2));
        // После остановки имя свободно.
        let srv = PipeServer::start(&name, |m| m.to_vec()).unwrap();
        assert_eq!(call(&name, b"again", 1000).unwrap(), b"again");
        srv.stop();
    }
}
