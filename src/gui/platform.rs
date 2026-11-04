//! Системная часть для интерфейса. На Windows — настоящие ImDisk и Vulkan;
//! в демо-режиме (Linux, `--demo`) — имитация для проверки интерфейса.

use super::engine::CreateRequest;
use vramdisk::disk::HybridDisk;

/// Результат подключения.
pub struct MountOk {
    pub server: Server,
    pub device: Option<u32>,
    pub notes: Vec<String>,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub enum Unmount {
    /// Диск занят — можно отключить принудительно.
    Busy(String),
    Failed(String),
}

pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::{CreateRequest, HybridDisk, MountOk, Unmount};
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use std::time::Duration;
    use vramdisk::logger::log;
    use vramdisk::settings::FsKind;
    use vramdisk::disk::Reclaim;
    use vramdisk::stats::Stats;
    use vramdisk::win::imdisk::{self, RemoveError};
    use vramdisk::win::reclaim::Reclaimer;
    use vramdisk::win::shm::ShmServer;
    use vramdisk::win::{autostart, shutdown, sys};
    use std::sync::Arc;

    /// Буфер общей памяти: драйвер режет запросы по нему.
    pub const SHM_BUFFER: usize = 8 << 20;

    pub struct Server {
        shm: Option<ShmServer>,
        /// Проход очистки и всё, чтобы запустить его заново.
        reclaimer: Option<Reclaimer>,
        reclaim: (char, String, Reclaim, Arc<Stats>),
    }

    impl Server {
        pub fn running(&self) -> bool {
            self.shm.as_ref().is_some_and(|s| s.is_running())
        }

        fn start_reclaim(&mut self) {
            let (l, fs, rc, st) = self.reclaim.clone();
            self.reclaimer = Some(Reclaimer::start(l, fs, rc, st));
        }
    }

    pub fn memory() -> (u64, u64) {
        sys::memory()
    }
    pub fn free_letters() -> Vec<char> {
        sys::free_letters()
    }
    pub fn letter_used(c: char) -> bool {
        sys::letter_used(c)
    }
    pub fn disk_space(c: char) -> Option<(u64, u64)> {
        sys::disk_space(c)
    }
    pub fn open_drive(c: char) {
        sys::shell_open(&format!("{c}:\\"));
    }
    pub fn open_url(u: &str) {
        sys::shell_open(u);
    }
    pub fn open_path(p: &std::path::Path) {
        sys::shell_open(&p.to_string_lossy());
    }

    /// `None` — ImDisk установлен и его драйвер запущен.
    pub fn imdisk_problem() -> Option<String> {
        imdisk::check().err()
    }

    pub fn autostart_enabled() -> bool {
        autostart::check_and_repair()
    }
    pub fn set_autostart(on: bool) -> Result<(), String> {
        if on {
            autostart::enable()
        } else {
            autostart::disable()
        }
    }

    /// Диск от прошлого запуска (процесс `pid`), сервер которого уже не
    /// работает. Проверяем, что устройство именно наше: номер и буква могли
    /// с тех пор достаться другому диску ImDisk.
    pub fn stale_exists(letter: char, device: Option<u32>, pid: u32) -> bool {
        let info = match device {
            Some(d) => imdisk::device_info(d),
            None if sys::letter_used(letter) => imdisk::describe(letter),
            None => None,
        };
        info.is_some_and(|i| imdisk::is_ours(&i, pid))
    }

    pub fn remove_stale(letter: char, device: Option<u32>) -> Result<(), String> {
        match device {
            Some(d) => {
                // `-R` снимает зависшее устройство, но не букву — её
                // убираем сами, если она всё ещё указывает на него.
                imdisk::emergency_remove(d)?;
                if sys::dos_target(letter).is_some_and(|t| t.eq_ignore_ascii_case(&format!("\\Device\\ImDisk{d}"))) {
                    sys::remove_drive_letter(letter);
                    log(format!("буква {letter}: от зависшего диска снята"));
                }
                Ok(())
            }
            None => imdisk::remove(letter, true).map_err(|e| match e {
                RemoveError::Busy(t) | RemoveError::Other(t) => t,
            }),
        }
    }

    /// Снять диск при выключении Windows или сбое программы.
    pub fn arm_cleanup(letter: Option<char>) {
        match letter {
            Some(c) => shutdown::set_cleanup(Some(Box::new(move || {
                if let Err(e) = sys::force_remove_volume(c) {
                    log(format!("аварийное снятие {c}: {e}"));
                }
            }))),
            None => shutdown::set_cleanup(None),
        }
    }

    pub fn hook_main_window() -> bool {
        shutdown::hook_main_window()
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    pub fn mount(mut disk: HybridDisk, req: &CreateRequest) -> Result<MountOk, String> {
        let letter = req.letter;
        if sys::letter_used(letter) {
            return Err(format!("Буква {letter}: уже занята другим диском. Выберите другую."));
        }
        let rc = disk.reclaim();
        let stats = disk.stats().clone();
        let name = format!("{}{}", imdisk::shm_prefix(std::process::id()), COUNTER.fetch_add(1, SeqCst));
        let server = ShmServer::start(&name, disk, SHM_BUFFER)?;
        let args = format!("/fs:{} /q /y /v:VramDisk", req.fs.format_name());
        let device = match imdisk::create(&name, letter, &args) {
            Ok(d) => d,
            Err(e) => {
                if sys::letter_used(letter) {
                    let _ = imdisk::remove(letter, true);
                }
                server.finish(Duration::from_secs(3));
                return Err(e);
            }
        };
        match device {
            Some(d) => log(format!("диск {letter}: создан, устройство ImDisk {d}")),
            None => log(format!("диск {letter}: создан (номер устройства ImDisk узнать не удалось)")),
        }
        // format.com запускается из imdisk, его код возврата не передаётся —
        // проверяем файловую систему сами.
        let mut info = None;
        for _ in 0..60 {
            info = sys::volume_info(letter);
            if info.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let fs_ok = info.as_ref().is_some_and(|(f, _)| f.eq_ignore_ascii_case(req.fs.label()));
        if !fs_ok {
            let now = info.map(|i| i.0).filter(|f| !f.is_empty()).unwrap_or_else(|| "нет файловой системы".into());
            let _ = imdisk::remove(letter, true);
            server.finish(Duration::from_secs(5));
            return Err(format!(
                "Диск создан, но format.com не отформатировал его в {} (сейчас: {now}). Подробности — в журнале.",
                req.fs.label()
            ));
        }
        let mut notes = Vec::new();
        if req.label != "VramDisk" {
            if let Err(e) = sys::set_label(letter, &req.label) {
                notes.push(format!("Метку тома установить не удалось: {e}"));
            }
        }
        if req.compress && req.fs == FsKind::Ntfs {
            if let Err(e) = sys::set_compression(letter) {
                notes.push(format!("Сжатие NTFS включить не удалось: {e}"));
            }
        }
        let mut server = Server {
            shm: Some(server),
            reclaimer: None,
            reclaim: (letter, req.fs.label().to_string(), rc, stats),
        };
        server.start_reclaim();
        Ok(MountOk { server, device, notes })
    }

    pub fn unmount(letter: char, server: &mut Server, force: bool) -> Result<(), Unmount> {
        // Проход очистки держит описатель тома — сначала остановить его.
        if let Some(mut r) = server.reclaimer.take() {
            r.stop();
        }
        let r = match imdisk::remove(letter, force) {
            Ok(()) => Ok(()),
            Err(RemoveError::Busy(t)) => Err(Unmount::Busy(t)),
            Err(RemoveError::Other(t)) if !force => Err(Unmount::Busy(t)),
            Err(RemoveError::Other(t)) => {
                log(format!("imdisk -D не справился ({t}), снимаю том напрямую"));
                sys::force_remove_volume(letter).map_err(|e| Unmount::Failed(format!("{t}\n{e}")))
            }
        };
        if r.is_err() {
            server.start_reclaim();
            return r;
        }
        if let Some(s) = server.shm.take() {
            s.finish(Duration::from_secs(10));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    //! Демо: диск живёт в памяти процесса, «ImDisk» и Проводник имитируются,
    //! поток создаёт нагрузку, чтобы было видно графики.
    use super::{CreateRequest, HybridDisk, MountOk, Unmount};
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use vramdisk::util::GIB;

    pub struct Server {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        pub fn running(&self) -> bool {
            self.thread.as_ref().is_some_and(|t| !t.is_finished())
        }
    }

    static SPACE: Mutex<Option<(char, u64, Arc<vramdisk::stats::Stats>)>> = Mutex::new(None);

    pub fn memory() -> (u64, u64) {
        (32 * GIB, 21 * GIB + 400 * vramdisk::util::MIB)
    }
    pub fn free_letters() -> Vec<char> {
        ('D'..='Z').filter(|c| !"EF".contains(*c)).collect()
    }
    pub fn letter_used(_c: char) -> bool {
        false
    }
    pub fn disk_space(c: char) -> Option<(u64, u64)> {
        let g = SPACE.lock().unwrap();
        let (l, size, st) = g.as_ref()?;
        if *l != c {
            return None;
        }
        let s = st.snapshot();
        let total = size - size / 100;
        Some((total, total.saturating_sub(s.used_ram + s.used_vram + 80 * vramdisk::util::MIB)))
    }
    pub fn open_drive(_c: char) {}
    pub fn open_url(_u: &str) {}
    pub fn open_path(_p: &std::path::Path) {}
    pub fn imdisk_problem() -> Option<String> {
        None
    }
    pub fn autostart_enabled() -> bool {
        false
    }
    pub fn set_autostart(_on: bool) -> Result<(), String> {
        Ok(())
    }
    pub fn stale_exists(_l: char, _d: Option<u32>, _pid: u32) -> bool {
        false
    }
    pub fn remove_stale(_l: char, _d: Option<u32>) -> Result<(), String> {
        Ok(())
    }
    pub fn arm_cleanup(_l: Option<char>) {}
    pub fn hook_main_window() -> bool {
        true
    }

    pub fn mount(mut disk: HybridDisk, req: &CreateRequest) -> Result<MountOk, String> {
        std::thread::sleep(Duration::from_millis(900));
        let size = disk.size();
        *SPACE.lock().unwrap() = Some((req.letter, size, disk.stats().clone()));
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::spawn(move || {
            // Имитация работы с диском: копирование «файлов», чтение,
            // удаление (TRIM) и паузы, во время которых диск обслуживает себя.
            const PIECE: usize = 4 << 20;
            let buf = vec![0x3Cu8; PIECE];
            let mut out = vec![0u8; PIECE];
            let fill = (size / 10 * 6).max(PIECE as u64) / PIECE as u64 * PIECE as u64;
            let mut pos = 1u64 << 20;
            let mut last_maint = Instant::now();
            let started = Instant::now();
            let mut tick = 0u64;
            while !s2.load(SeqCst) {
                tick += 1;
                let t = started.elapsed().as_secs() % 40;
                if t < 12 {
                    if pos + PIECE as u64 <= fill {
                        let _ = disk.write(pos, &buf);
                        pos += PIECE as u64;
                    }
                } else if t < 20 {
                    let off = (tick * 7919 % (pos / PIECE as u64).max(1)) * PIECE as u64;
                    let _ = disk.read(off, &mut out);
                } else if t == 20 && pos > fill / 2 {
                    // Удалили большой файл в середине записанного.
                    let (a, b) = (fill / 3, fill / 3 + fill / 4);
                    let _ = disk.trim(a / PIECE as u64 * PIECE as u64, b - a);
                    pos = a / PIECE as u64 * PIECE as u64;
                }
                if last_maint.elapsed() >= Duration::from_millis(250) {
                    disk.maintain(Instant::now());
                    last_maint = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(if (12..20).contains(&t) { 8 } else { 4 }));
            }
        });
        Ok(MountOk { server: Server { stop, thread: Some(thread) }, device: Some(0), notes: Vec::new() })
    }

    pub fn unmount(_letter: char, server: &mut Server, _force: bool) -> Result<(), Unmount> {
        std::thread::sleep(Duration::from_millis(600));
        server.stop.store(true, SeqCst);
        if let Some(t) = server.thread.take() {
            let _ = t.join();
        }
        *SPACE.lock().unwrap() = None;
        Ok(())
    }
}
