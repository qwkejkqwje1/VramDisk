//! VramDisk — ядро: блочное устройство из двух частей (оперативная память +
//! видеопамять) и сервер протокола ImDisk через общую память.
//!
//! Часть, не зависящая от Windows (карта блоков, хранилища, протокол),
//! собирается и тестируется и на Linux: видеопамять там проверяется через
//! программный Vulkan (lavapipe).

pub mod disk;
pub mod gpu;
pub mod logger;
pub mod pool;
pub mod proxy;
pub mod ram;
pub mod settings;
pub mod stats;
pub mod util;
pub mod vram;

#[cfg(windows)]
pub mod win;

/// Заголовок главного окна (по нему находится уже запущенная копия).
pub const APP_TITLE: &str = "VramDisk";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
