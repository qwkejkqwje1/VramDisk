//! Автозапуск через Планировщик заданий: задача «VramDisk» при входе
//! пользователя, с наивысшими правами — без запроса UAC при каждом входе.

use super::run_hidden_ex;
use crate::logger::log;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const TASK: &str = "VramDisk";
pub const ARG: &str = "--autostart";

fn schtasks() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("schtasks.exe")
}

fn run(args: &[&str]) -> Result<super::RunOutput, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    run_hidden_ex(&schtasks(), &args, Duration::from_secs(20), true)
}

/// XML задания или `None`, если его нет. Смотрим и на вывод, а не только на
/// код возврата: так надёжнее (например, в Wine код 0 при пустом выводе).
fn query_xml() -> Option<String> {
    let args: Vec<String> = ["/Query", "/TN", TASK, "/XML"].iter().map(|s| s.to_string()).collect();
    let o = run_hidden_ex(&schtasks(), &args, Duration::from_secs(20), false).ok()?;
    (o.code == 0 && o.text.contains("<Task")).then_some(o.text)
}

pub fn is_enabled() -> bool {
    query_xml().is_some()
}

/// Запущена ли программа из временной папки (например, прямо из архива):
/// такой путь для автозапуска не годится — папку скоро удалят.
fn in_temp(exe: &Path) -> bool {
    let low = |p: &Path| p.to_string_lossy().to_lowercase();
    let exe = low(exe);
    let tmp = low(&std::env::temp_dir());
    !tmp.is_empty() && exe.starts_with(tmp.trim_end_matches('\\'))
}

/// Включён ли автозапуск. Если задание указывает на другой файл (программу
/// перенесли в другую папку), перенастраивает его на текущий.
pub fn check_and_repair() -> bool {
    let Some(xml) = query_xml() else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        return true;
    };
    let want = esc(&exe.to_string_lossy()).to_lowercase();
    if !xml.to_lowercase().contains(&want) && !in_temp(&exe) {
        log("автозапуск указывает на другой файл программы — перенастраиваю на текущий");
        if let Err(e) = enable() {
            log(format!("перенастроить автозапуск не удалось: {e}"));
        }
    }
    true
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    }
}

pub fn task_xml(exe: &Path, user: &str) -> String {
    let exe_s = exe.to_string_lossy();
    let dir = exe.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>VramDisk: создаёт диск из оперативной памяти и видеопамяти при входе в Windows</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT10S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
      <Arguments>{arg}</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        user = esc(user),
        exe = esc(&exe_s),
        dir = esc(&dir),
        arg = ARG
    )
}

pub fn enable() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if in_temp(&exe) {
        return Err("Программа запущена из временной папки — похоже, прямо из архива. \
                    Распакуйте её в постоянную папку и включите автозапуск оттуда."
            .into());
    }
    let xml = task_xml(&exe, &current_user());
    // Файл задания — рядом с программой, сразу удаляется.
    let path = crate::settings::app_dir().join("vramdisk-task.xml");
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).map_err(|e| format!("не удалось записать задание: {e}"))?;
    let p = path.to_string_lossy().into_owned();
    let r = run(&["/Create", "/TN", TASK, "/XML", &p, "/F"]);
    let _ = std::fs::remove_file(&path);
    let o = r?;
    if o.code == 0 {
        Ok(())
    } else {
        Err(format!("Планировщик заданий: {}", super::squash(&o.text)))
    }
}

pub fn disable() -> Result<(), String> {
    if !is_enabled() {
        return Ok(());
    }
    let o = run(&["/Delete", "/TN", TASK, "/F"])?;
    if o.code == 0 {
        Ok(())
    } else {
        Err(format!("Планировщик заданий: {}", super::squash(&o.text)))
    }
}
