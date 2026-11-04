//! Вызовы imdisk.exe (ImDisk Toolkit / ImDisk Virtual Disk Driver).

use super::{run_hidden, squash, RunOutput};
use std::path::PathBuf;
use std::time::Duration;

pub const TOOLKIT_URL: &str = "https://sourceforge.net/projects/imdisk-toolkit/";

/// Путь к imdisk.exe: System32, папка ImDisk в Program Files или PATH.
pub fn exe() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut places = vec![root.join("System32").join("imdisk.exe"), root.join("Sysnative").join("imdisk.exe")];
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            places.push(PathBuf::from(pf).join("ImDisk").join("imdisk.exe"));
        }
    }
    if let Some(p) = places.into_iter().find(|f| f.is_file()) {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("imdisk.exe")).find(|f| f.is_file()))
}

fn run(args: &[&str], timeout: Duration) -> Result<RunOutput, String> {
    let exe = exe().ok_or("ImDisk не установлен: не найден imdisk.exe")?;
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    run_hidden(&exe, &args, timeout)
}

/// Короткий текст ошибки из вывода imdisk (без служебных строк).
fn reason(o: &RunOutput) -> String {
    let lines: Vec<&str> = o
        .text
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("Creating device")
                && !l.starts_with("Notifying")
                && !l.starts_with("Opening")
                && !l.ends_with("...")
        })
        .collect();
    if lines.is_empty() {
        format!("код {}", o.code)
    } else {
        lines.join(" ")
    }
}

pub const NOT_INSTALLED: &str = "ImDisk не установлен: не найден imdisk.exe. Установите ImDisk Toolkit.";

/// ImDisk установлен, а его драйвер запущен (или запускается).
///
/// Состояние драйвера берётся у диспетчера служб Windows, а не из `imdisk -l`:
/// она завершается с кодом 1 и при успехе (в том числе «No virtual disks.»),
/// и когда драйвер не загружен, а 0 — лишь при редких внутренних ошибках,
/// поэтому по коду её проверять нельзя (в версии 1.0 из-за этого «ImDisk не
/// найден» показывалось при установленном ImDisk).
pub fn check() -> Result<(), String> {
    if exe().is_none() {
        return Err(NOT_INSTALLED.into());
    }
    let r = super::service::ensure_running("ImDisk");
    let r = match r {
        Ok(()) => Ok(()),
        Err(super::service::ServiceError::Missing) => Err(
            "Драйвер ImDisk не установлен: в системе нет службы «ImDisk», хотя imdisk.exe есть. Переустановите ImDisk Toolkit \
             (при необходимости перезагрузите компьютер)."
                .into(),
        ),
        Err(super::service::ServiceError::Disabled) => Err(
            "Драйвер ImDisk отключён (у службы «ImDisk» тип запуска «Отключена»). Включите его: в командной строке от имени \
             администратора выполните «sc config ImDisk start= demand» или переустановите ImDisk Toolkit."
                .into(),
        ),
        Err(super::service::ServiceError::Start(code)) => Err(start_error(code)),
        Err(super::service::ServiceError::Timeout) => Err("Драйвер ImDisk не запустился за 10 секунд.".into()),
        // Нет прав или диспетчер служб недоступен: не мешаем — imdisk -a сам
        // попробует запустить драйвер и объяснит, если не выйдет.
        Err(super::service::ServiceError::Unknown(e)) => {
            crate::logger::log(format!("состояние драйвера ImDisk неизвестно: {e}"));
            Ok(())
        }
    };
    // Для журнала: что видит сам imdisk. Код 1 здесь — норма.
    if r.is_ok() {
        let _ = run(&["-l"], Duration::from_secs(15));
    }
    r
}

/// Почему драйвер ImDisk не запустился — понятными словами.
fn start_error(code: u32) -> String {
    match code {
        2 | 3 => "Файл драйвера imdisk.sys не найден (System32\\drivers). Переустановите ImDisk Toolkit.".into(),
        577 => "Windows не приняла цифровую подпись драйвера imdisk.sys. Обычно так бывает, когда включена \
                «Целостность памяти»: Безопасность Windows → Безопасность устройства → Изоляция ядра. \
                Выключите её и перезагрузите компьютер — или используйте другую программу."
            .into(),
        1275 => "Windows заблокировала драйвер imdisk.sys (список заблокированных драйверов или «Целостность памяти»: \
                 Безопасность Windows → Безопасность устройства → Изоляция ядра)."
            .into(),
        1058 => "Драйвер ImDisk отключён. В командной строке от имени администратора выполните «sc config ImDisk start= demand».".into(),
        c => format!("Драйвер ImDisk не запускается: {}", super::error_text(c)),
    }
}

/// Номер устройства из «Created device 3: V: -> ...».
pub fn parse_device(text: &str) -> Option<u32> {
    let i = text.find("Created device ")? + "Created device ".len();
    let digits: String = text[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Создаёт диск, подключённый к серверу общей памяти `name`, и форматирует.
pub fn create(name: &str, letter: char, format_args: &str) -> Result<Option<u32>, String> {
    let mount = format!("{letter}:");
    let o = run(
        &["-a", "-t", "proxy", "-o", "shm", "-f", name, "-m", &mount, "-p", format_args],
        Duration::from_secs(300),
    )?;
    let dev = parse_device(&o.text);
    if o.code != 0 {
        return Err(format!("ImDisk не создал диск: {}", reason(&o)));
    }
    Ok(dev)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveError {
    /// Диск занят (открыты файлы) — можно отключить принудительно.
    Busy(String),
    Other(String),
}

/// Снимает диск. `force` — даже если на нём открыты файлы.
pub fn remove(letter: char, force: bool) -> Result<(), RemoveError> {
    let mount = format!("{letter}:");
    let o = run(&[if force { "-D" } else { "-d" }, "-m", &mount], Duration::from_secs(60)).map_err(RemoveError::Other)?;
    if o.code == 0 {
        return Ok(());
    }
    let r = reason(&o);
    let low = o.text.to_lowercase();
    if !force && (low.contains("in use") || low.contains("access") || low.contains("lock") || low.contains("-d")) {
        Err(RemoveError::Busy(r))
    } else {
        Err(RemoveError::Other(r))
    }
}

/// Аварийное снятие зависшего диска (сервер которого уже не работает).
pub fn emergency_remove(device: u32) -> Result<(), String> {
    let n = device.to_string();
    let o = run(&["-R", "-u", &n], Duration::from_secs(30))?;
    if o.code == 0 {
        Ok(())
    } else {
        Err(reason(&o))
    }
}

/// Сведения об устройстве ImDisk с этим номером (`None` — такого нет).
pub fn device_info(device: u32) -> Option<String> {
    let n = device.to_string();
    run(&["-l", "-u", &n], Duration::from_secs(15)).ok().filter(|o| o.code == 0).map(|o| o.text)
}

/// Имя общей памяти, которое VramDisk даёт серверу процесса `pid`. По нему
/// в `imdisk -l` («Image file: ...») узнаём свой диск, даже если номер
/// устройства или буква с тех пор достались другому диску.
pub fn shm_prefix(pid: u32) -> String {
    format!("VramDisk{pid}_")
}

/// Это диск нашего процесса `pid`?
pub fn is_ours(info: &str, pid: u32) -> bool {
    info.to_lowercase().contains(&shm_prefix(pid).to_lowercase())
}

/// Сведения о диске на букве (если это диск ImDisk) — для журнала.
pub fn describe(letter: char) -> Option<String> {
    let mount = format!("{letter}:");
    run(&["-l", "-m", &mount], Duration::from_secs(15)).ok().filter(|o| o.code == 0).map(|o| squash(&o.text))
}

#[cfg(test)]
mod tests {
    #[test]
    fn device_number() {
        assert_eq!(super::parse_device("Creating device...\nCreated device 3: V: -> Image in memory"), Some(3));
        assert_eq!(super::parse_device("Error"), None);
    }

    #[test]
    fn ownership() {
        let info = "Drive letter: V\nImage file: \\BaseNamedObjects\\Global\\VramDisk1234_0\nSize: 1 bytes, Proxy, HDD.";
        assert!(super::is_ours(info, 1234));
        assert!(!super::is_ours(info, 123));
        assert!(!super::is_ours("Image file: \\BaseNamedObjects\\Global\\VramDisk12345_0", 1234));
    }
}
