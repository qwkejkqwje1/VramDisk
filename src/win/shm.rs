//! Сервер ImDisk proxy через общую память (`imdisk -t proxy -o shm -f ИМЯ`).
//!
//! Объекты (как у devio из ImDisk): секция `Global\ИМЯ` (заголовок 4096 байт
//! и буфер данных), мьютекс `Global\ИМЯ_Server` (держит поток сервера),
//! события с автосбросом `Global\ИМЯ_Request` и `Global\ИМЯ_Response`.
//! Драйвер пишет запрос, взводит `_Request` и ждёт `_Response` без тайм-аута,
//! поэтому сервер должен жить, пока существует диск.

use super::{last_error, wide};
use crate::disk::HybridDisk;
use crate::logger::log;
use crate::proxy::{self, Outcome, HEADER_SIZE};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    SEC_COMMIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentThread, ReleaseMutex, SetEvent, SetThreadPriority, WaitForSingleObject,
    THREAD_PRIORITY_HIGHEST,
};

/// Сколько ждать запроса, прежде чем заняться обслуживанием диска.
const IDLE_WAIT_MS: u32 = 250;
/// Во время работы обслуживание — не чаще раза в секунду.
const BUSY_MAINTAIN: Duration = Duration::from_secs(1);

struct Objects {
    map: HANDLE,
    view: *mut u8,
    len: usize,
    mutex: HANDLE,
    req: HANDLE,
    resp: HANDLE,
}

// Описатели действуют во всём процессе; общая память используется строго
// по очереди с драйвером (запрос → ответ).
unsafe impl Send for Objects {}
unsafe impl Sync for Objects {}

impl Objects {
    fn create(name: &str, buffer: usize) -> Result<Self, String> {
        let len = HEADER_SIZE + buffer;
        let mut o = Objects {
            map: std::ptr::null_mut(),
            view: std::ptr::null_mut(),
            len,
            mutex: std::ptr::null_mut(),
            req: std::ptr::null_mut(),
            resp: std::ptr::null_mut(),
        };
        unsafe {
            let n = wide(&format!("Global\\{name}"));
            o.map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_COMMIT,
                (len as u64 >> 32) as u32,
                len as u32,
                n.as_ptr(),
            );
            if o.map.is_null() {
                return Err(format!("не удалось создать общую память для ImDisk: {}", last_error()));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return Err("общая память с таким именем уже существует — работает другой экземпляр".into());
            }
            let v = MapViewOfFile(o.map, FILE_MAP_WRITE, 0, 0, 0);
            if v.Value.is_null() {
                return Err(format!("не удалось отобразить общую память: {}", last_error()));
            }
            o.view = v.Value as *mut u8;
            std::ptr::write_bytes(o.view, 0, HEADER_SIZE);
            o.mutex = CreateMutexW(std::ptr::null(), 0, wide(&format!("Global\\{name}_Server")).as_ptr());
            o.req = CreateEventW(std::ptr::null(), 0, 0, wide(&format!("Global\\{name}_Request")).as_ptr());
            o.resp = CreateEventW(std::ptr::null(), 0, 0, wide(&format!("Global\\{name}_Response")).as_ptr());
            if o.mutex.is_null() || o.req.is_null() || o.resp.is_null() {
                return Err(format!("не удалось создать события для ImDisk: {}", last_error()));
            }
        }
        Ok(o)
    }
}

impl Drop for Objects {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as _ });
            }
            for h in [self.resp, self.req, self.mutex, self.map] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Драйвер прислал CLOSE — диск снят.
    Closed,
    /// Остановлен нами (диск не был подключён).
    Stopped,
    /// Ошибка ожидания событий.
    Failed,
}

pub struct ShmServer {
    name: String,
    objs: Arc<Objects>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    last_request: Arc<AtomicU64>,
    started: Instant,
    thread: Option<JoinHandle<ExitReason>>,
}

impl ShmServer {
    /// Создаёт объекты и запускает поток сервера. `buffer` — размер буфера
    /// данных: драйвер сам режет запросы на куски не больше него.
    pub fn start(name: &str, disk: HybridDisk, buffer: usize) -> Result<Self, String> {
        let objs = Arc::new(Objects::create(name, buffer)?);
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let last_request = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        let thread = {
            let (o, s, c, lr) = (objs.clone(), stop.clone(), connected.clone(), last_request.clone());
            std::thread::Builder::new()
                .name("imdisk-proxy".into())
                .spawn(move || serve(o, disk, s, c, lr, started, tx))
                .map_err(|e| format!("не удалось запустить поток сервера: {e}"))?
        };
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err("поток сервера не запустился".into()),
        }
        log(format!("сервер ImDisk запущен: Global\\{name}, буфер {}", crate::util::size(buffer as u64)));
        Ok(Self { name: name.to_string(), objs, stop, connected, last_request, started, thread: Some(thread) })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Драйвер уже присылал запросы.
    pub fn connected(&self) -> bool {
        self.connected.load(SeqCst)
    }

    /// Сколько прошло с последнего запроса драйвера.
    pub fn idle(&self) -> Duration {
        let ms = self.last_request.load(SeqCst);
        self.started.elapsed().saturating_sub(Duration::from_millis(ms))
    }

    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Ждёт завершения сервера (после `imdisk -d` драйвер присылает CLOSE).
    /// Если за `wait` не завершился — останавливает сам. Вызывать, только
    /// когда диска уже нет: иначе запросы драйвера повиснут.
    pub fn finish(mut self, wait: Duration) -> Option<ExitReason> {
        self.finish_inner(wait)
    }

    fn finish_inner(&mut self, wait: Duration) -> Option<ExitReason> {
        let t = self.thread.take()?;
        let deadline = Instant::now() + wait;
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
        }
        if !t.is_finished() {
            self.stop.store(true, SeqCst);
            unsafe { SetEvent(self.objs.req) };
            let d2 = Instant::now() + Duration::from_secs(5);
            while !t.is_finished() && Instant::now() < d2 {
                std::thread::sleep(Duration::from_millis(15));
            }
            if !t.is_finished() {
                log("сервер ImDisk не остановился за 5 с — оставляю поток");
                return None;
            }
        }
        t.join().ok()
    }
}

impl Drop for ShmServer {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.finish_inner(Duration::from_millis(200));
        }
    }
}

fn serve(
    o: Arc<Objects>,
    mut disk: HybridDisk,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    last_request: Arc<AtomicU64>,
    started: Instant,
    tx: mpsc::Sender<Result<(), String>>,
) -> ExitReason {
    let w = unsafe { WaitForSingleObject(o.mutex, 0) };
    if w != WAIT_OBJECT_0 && w != WAIT_ABANDONED {
        let _ = tx.send(Err("сервер с таким именем уже работает".into()));
        return ExitReason::Failed;
    }
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
    let _ = tx.send(Ok(()));
    let stats = disk.stats().clone();
    let mut panics = 0u32;
    let mut last_maintain = Instant::now();
    // Обслуживание (возврат памяти, упаковка) — между запросами драйвера.
    let mut maintain_ok = true;
    let mut maintain = |disk: &mut HybridDisk, now: Instant| {
        if !maintain_ok {
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| disk.maintain(now))).is_err() {
            maintain_ok = false;
            stats.set_error("сбой при обслуживании диска — возврат памяти отключён до отключения диска");
            log("сбой в HybridDisk::maintain — обслуживание отключено");
        }
    };
    let reason = loop {
        let w = unsafe { WaitForSingleObject(o.req, IDLE_WAIT_MS) };
        if w == WAIT_TIMEOUT {
            if stop.load(SeqCst) {
                break ExitReason::Stopped;
            }
            let now = Instant::now();
            maintain(&mut disk, now);
            last_maintain = now;
            continue;
        }
        if w != WAIT_OBJECT_0 {
            stats.set_error(format!("ожидание запроса ImDisk: {}", last_error()));
            break ExitReason::Failed;
        }
        if stop.load(SeqCst) {
            break ExitReason::Stopped;
        }
        connected.store(true, SeqCst);
        last_request.store(started.elapsed().as_millis() as u64, SeqCst);
        std::sync::atomic::fence(SeqCst);
        let outcome = {
            // Драйвер не трогает память, пока ждёт ответа.
            let shm = unsafe { std::slice::from_raw_parts_mut(o.view, o.len) };
            let code = proxy::get(shm, 0);
            match catch_unwind(AssertUnwindSafe(|| proxy::handle(shm, &mut disk))) {
                Ok(out) => out,
                Err(p) => {
                    panics += 1;
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    stats.set_error(format!("сбой при обработке запроса {code}: {msg}"));
                    proxy::put(shm, 0, proxy::EIO);
                    proxy::put(shm, 8, 0);
                    Outcome::Reply
                }
            }
        };
        std::sync::atomic::fence(SeqCst);
        match outcome {
            Outcome::Reply => {
                if unsafe { SetEvent(o.resp) } == 0 {
                    stats.set_error(format!("ответ ImDisk: {}", last_error()));
                    break ExitReason::Failed;
                }
            }
            Outcome::Close => break ExitReason::Closed,
        }
        let now = Instant::now();
        if now.saturating_duration_since(last_maintain) >= BUSY_MAINTAIN {
            maintain(&mut disk, now);
            last_maintain = now;
        }
    };
    let _ = disk.flush();
    drop(disk);
    unsafe { ReleaseMutex(o.mutex) };
    stats.server_stopped.store(true, SeqCst);
    log(format!("сервер ImDisk завершён: {reason:?}, сбоев обработки: {panics}"));
    reason
}
