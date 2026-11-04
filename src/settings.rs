//! Настройки и место для файлов программы.
//!
//! Всё своё VramDisk хранит рядом с vramdisk.exe: настройки `vramdisk.json`,
//! журнал `vramdisk.log` (и предыдущий — `vramdisk.old.log`), запись о
//! подключённом диске `vramdisk.disk.json` (её ведёт фоновая часть, пока
//! держит диск). Переменная окружения `VRAMDISK_HOME` задаёт другую папку
//! (для тестов).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsKind {
    Ntfs,
    ExFat,
}

impl FsKind {
    pub fn label(self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::ExFat => "exFAT",
        }
    }
    /// Значение для `format.com /fs:`.
    pub fn format_name(self) -> &'static str {
        match self {
            FsKind::Ntfs => "ntfs",
            FsKind::ExFat => "exfat",
        }
    }
    /// Наибольшая длина метки тома.
    pub fn label_limit(self) -> usize {
        match self {
            FsKind::Ntfs => 32,
            FsKind::ExFat => 11,
        }
    }
}

/// Тема оформления.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeChoice {
    /// Как в Windows.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "как в системе",
            ThemeChoice::Light => "светлая",
            ThemeChoice::Dark => "тёмная",
        }
    }
    pub fn next(self) -> Self {
        match self {
            ThemeChoice::System => ThemeChoice::Light,
            ThemeChoice::Light => ThemeChoice::Dark,
            ThemeChoice::Dark => ThemeChoice::System,
        }
    }
}

/// Какую память диск занимает первой.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FillFirst {
    /// Оперативную: она быстрее; видеопамять — когда ОЗУ занята до предела.
    #[default]
    Ram,
    /// Видеопамять: оперативная остаётся программам, пока видеопамять не
    /// заполнена до предела.
    Vram,
}

impl FillFirst {
    pub fn label(self) -> &'static str {
        match self {
            FillFirst::Ram => "оперативную память",
            FillFirst::Vram => "видеопамять",
        }
    }
}

/// Подключённый диск — чтобы после аварийного завершения убрать «зависший».
/// `pid` — процесс, который обслуживает диск (фоновая часть).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    pub letter: char,
    pub device: Option<u32>,
    pub pid: u32,
}

impl MountRecord {
    pub fn path() -> PathBuf {
        app_dir().join("vramdisk.disk.json")
    }

    pub fn load() -> Option<Self> {
        let t = std::fs::read_to_string(Self::path()).ok()?;
        serde_json::from_str(&t).ok()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(&path, json.as_bytes()).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
    }

    pub fn clear() {
        let _ = std::fs::remove_file(Self::path());
    }
}

/// Запись через временный файл: при сбое не остаётся половины файла.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Предел оперативной памяти для диска.
    pub ram_mb: u64,
    /// Предел видеопамяти для диска.
    pub vram_mb: u64,
    /// Что заполнять первым.
    pub fill_first: FillFirst,
    pub letter: char,
    pub label: String,
    pub fs: FsKind,
    pub compress: bool,
    /// Создавать диск при входе в Windows.
    pub autostart: bool,
    pub open_explorer: bool,
    pub gpu_uuid: Option<String>,
    /// Сколько видеопамяти оставлять свободной для игр и рабочего стола.
    pub vram_reserve_mb: u64,
    /// Сколько оперативной памяти оставлять системе.
    pub ram_reserve_mb: u64,
    /// Запись о диске версии 2.0 (теперь — в `vramdisk.disk.json`):
    /// читается один раз при обновлении.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounted: Option<MountRecord>,
    pub theme: ThemeChoice,
    /// Настройки только что созданы (файла не было).
    #[serde(skip)]
    pub fresh: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ram_mb: 2048,
            vram_mb: 4096,
            fill_first: FillFirst::Ram,
            letter: 'V',
            label: "VramDisk".into(),
            fs: FsKind::Ntfs,
            compress: false,
            autostart: false,
            open_explorer: true,
            gpu_uuid: None,
            vram_reserve_mb: 1024,
            ram_reserve_mb: 2048,
            mounted: None,
            theme: ThemeChoice::System,
            fresh: true,
        }
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Папка программы: рядом с vramdisk.exe.
pub fn app_dir() -> PathBuf {
    env_dir("VRAMDISK_HOME")
        .or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Где хранила файлы версия 1.0 (настройки, журнал).
fn old_dirs() -> (Option<PathBuf>, Option<PathBuf>) {
    if cfg!(windows) {
        (env_dir("APPDATA").map(|d| d.join("VramDisk")), env_dir("LOCALAPPDATA").map(|d| d.join("VramDisk")))
    } else {
        let home = env_dir("HOME");
        (
            env_dir("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config"))).map(|d| d.join("vramdisk")),
            env_dir("XDG_STATE_HOME").or_else(|| home.as_ref().map(|h| h.join(".local/state"))).map(|d| d.join("vramdisk")),
        )
    }
}

/// Перенос с версии 1.0: настройки переезжают к программе, старые файлы
/// (только свои) и опустевшие папки удаляются. Возвращает, что сделано.
pub fn migrate_old() -> Vec<String> {
    let mut done = Vec::new();
    if env_dir("VRAMDISK_HOME").is_some() && std::env::var_os("VRAMDISK_MIGRATE").is_none() {
        return done;
    }
    let (conf, logs) = old_dirs();
    if let Some(dir) = conf {
        let old = dir.join("settings.json");
        if old.is_file() {
            let new = Settings::path();
            if !new.exists() {
                match std::fs::read_to_string(&old).ok().and_then(|t| serde_json::from_str::<Settings>(&t).ok()) {
                    Some(mut s) => {
                        s.sanitize();
                        match s.save() {
                            Ok(()) => done.push(format!("настройки перенесены из {}", old.display())),
                            Err(e) => {
                                done.push(format!("перенести настройки из {} не удалось: {e}", old.display()));
                                return done;
                            }
                        }
                    }
                    None => done.push(format!("старые настройки {} не читаются — не переношу", old.display())),
                }
            }
            if std::fs::remove_file(&old).is_ok() {
                done.push(format!("удалён {}", old.display()));
            }
            let _ = std::fs::remove_file(dir.join("settings.json.tmp"));
        }
        if std::fs::remove_dir(&dir).is_ok() {
            done.push(format!("удалена пустая папка {}", dir.display()));
        }
    }
    if let Some(dir) = logs {
        for f in ["vramdisk.log", "vramdisk.old.log"] {
            let p = dir.join(f);
            if p.is_file() && std::fs::remove_file(&p).is_ok() {
                done.push(format!("удалён старый журнал {}", p.display()));
            }
        }
        if std::fs::remove_dir(&dir).is_ok() {
            done.push(format!("удалена пустая папка {}", dir.display()));
        }
    }
    done
}

impl Settings {
    pub fn path() -> PathBuf {
        app_dir().join("vramdisk.json")
    }

    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => match serde_json::from_str::<Settings>(&s) {
                Ok(mut v) => {
                    v.fresh = false;
                    v.sanitize();
                    v
                }
                Err(e) => {
                    crate::logger::log(format!("настройки повреждены ({e}), беру значения по умолчанию"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(&path, json.as_bytes()).map_err(|e| format!("не удалось сохранить настройки в {}: {e}", path.display()))
    }

    /// Приводит значения к допустимым.
    pub fn sanitize(&mut self) {
        self.letter = self.letter.to_ascii_uppercase();
        if !('D'..='Z').contains(&self.letter) {
            self.letter = 'V';
        }
        self.label = clean_label(&self.label, self.fs);
        self.ram_mb = self.ram_mb.min(1 << 24);
        self.vram_mb = self.vram_mb.min(1 << 20);
        if self.fs != FsKind::Ntfs {
            self.compress = false;
        }
    }
}

/// Метка тома без запрещённых символов и не длиннее предела файловой системы.
pub fn clean_label(s: &str, fs: FsKind) -> String {
    const BAD: &[char] = &['*', '?', '/', '\\', '|', '.', ',', ';', ':', '+', '=', '[', ']', '<', '>', '"', '\t'];
    let mut out: String = s.chars().filter(|c| !BAD.contains(c) && !c.is_control()).take(fs.label_limit()).collect();
    out = out.trim().to_string();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_sanitize() {
        let mut s = Settings { letter: 'v', label: "Мой: диск*".into(), ..Default::default() };
        s.sanitize();
        assert_eq!(s.letter, 'V');
        assert_eq!(s.label, "Мой диск");
        let json = serde_json::to_string(&s).unwrap();
        let mut back: Settings = serde_json::from_str(&json).unwrap();
        back.fresh = s.fresh;
        assert_eq!(back, s);
        // старый файл без новых полей читается
        let old: Settings = serde_json::from_str(r#"{"ram_mb": 1024}"#).unwrap();
        assert_eq!(old.ram_mb, 1024);
        assert_eq!(old.letter, 'V');
        assert_eq!(clean_label("ОченьДлинноеИмя", FsKind::ExFat).chars().count(), 11);
    }

    #[test]
    fn write_atomic_replaces() {
        let dir = std::env::temp_dir().join(format!("vramdisk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.json");
        write_atomic(&p, b"1").unwrap();
        write_atomic(&p, b"22").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"22");
        assert!(!dir.join("x.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
