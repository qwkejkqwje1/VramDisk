//! Счётчики работы диска. Пишет поток сервера, читает интерфейс.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

#[derive(Default)]
pub struct Stats {
    pub read_ram: AtomicU64,
    pub read_vram: AtomicU64,
    pub write_ram: AtomicU64,
    pub write_vram: AtomicU64,
    /// Освобождено по TRIM, ZERO и записи сплошных нулей.
    pub trimmed: AtomicU64,
    /// Освобождено проходом очистки по карте свободного места.
    pub reclaimed: AtomicU64,
    /// Перенесено между хранилищами (после аварийного выхода за предел).
    pub moved: AtomicU64,
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    /// Отказов записи из-за нехватки памяти.
    pub nomem: AtomicU64,
    /// Данные на диске (блоки с данными), байт.
    pub used_ram: AtomicU64,
    pub used_vram: AtomicU64,
    /// Сколько памяти диск держит сейчас (видеопамять — кусками).
    pub held_ram: AtomicU64,
    pub held_vram: AtomicU64,
    /// Сверх предела пользователя (аварийно, когда другое хранилище отказало).
    pub over_ram: AtomicU64,
    pub over_vram: AtomicU64,
    /// Пределы пользователя.
    pub max_ram: AtomicU64,
    pub max_vram: AtomicU64,
    /// Видеокарта сброшена драйвером: данные в видеопамяти потеряны.
    pub device_lost: AtomicBool,
    /// Сервер завершился (ImDisk закрыл диск или ошибка).
    pub server_stopped: AtomicBool,
    pub last_error: Mutex<Option<String>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub read_ram: u64,
    pub read_vram: u64,
    pub write_ram: u64,
    pub write_vram: u64,
    pub trimmed: u64,
    pub reclaimed: u64,
    pub moved: u64,
    pub requests: u64,
    pub errors: u64,
    pub nomem: u64,
    pub used_ram: u64,
    pub used_vram: u64,
    pub held_ram: u64,
    pub held_vram: u64,
    pub over_ram: u64,
    pub over_vram: u64,
    pub max_ram: u64,
    pub max_vram: u64,
    pub device_lost: bool,
    pub server_stopped: bool,
}

impl Snapshot {
    /// Всего освобождено после удаления файлов.
    pub fn freed(&self) -> u64 {
        self.trimmed + self.reclaimed
    }
}

impl Stats {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            read_ram: self.read_ram.load(Relaxed),
            read_vram: self.read_vram.load(Relaxed),
            write_ram: self.write_ram.load(Relaxed),
            write_vram: self.write_vram.load(Relaxed),
            trimmed: self.trimmed.load(Relaxed),
            reclaimed: self.reclaimed.load(Relaxed),
            moved: self.moved.load(Relaxed),
            requests: self.requests.load(Relaxed),
            errors: self.errors.load(Relaxed),
            nomem: self.nomem.load(Relaxed),
            used_ram: self.used_ram.load(Relaxed),
            used_vram: self.used_vram.load(Relaxed),
            held_ram: self.held_ram.load(Relaxed),
            held_vram: self.held_vram.load(Relaxed),
            over_ram: self.over_ram.load(Relaxed),
            over_vram: self.over_vram.load(Relaxed),
            max_ram: self.max_ram.load(Relaxed),
            max_vram: self.max_vram.load(Relaxed),
            device_lost: self.device_lost.load(Relaxed),
            server_stopped: self.server_stopped.load(Relaxed),
        }
    }

    pub fn set_error(&self, e: impl Into<String>) {
        let e = e.into();
        crate::logger::log(format!("ошибка: {e}"));
        *self.last_error.lock().unwrap() = Some(e);
        self.errors.fetch_add(1, Relaxed);
    }

    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().take()
    }
}
