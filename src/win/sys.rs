//! Системные сведения и действия: память, буквы дисков, метка, сжатие,
//! Проводник, права администратора, единственный экземпляр.

use super::{from_wide, last_error, wide};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW, SetVolumeLabelW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::FSCTL_SET_COMPRESSION;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, MB_OK, SW_RESTORE, SW_SHOWNORMAL,
};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// (всего, свободно) оперативной памяти.
pub fn memory() -> (u64, u64) {
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return (0, 0);
    }
    (m.ullTotalPhys, m.ullAvailPhys)
}

/// Свободные буквы от D до Z.
pub fn free_letters() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    ('D'..='Z').filter(|&c| mask & (1 << (c as u32 - 'A' as u32)) == 0).collect()
}

pub fn letter_used(c: char) -> bool {
    let mask = unsafe { GetLogicalDrives() };
    mask & (1 << (c.to_ascii_uppercase() as u32 - 'A' as u32)) != 0
}

fn root(c: char) -> Vec<u16> {
    wide(&format!("{c}:\\"))
}

/// (всего, свободно) на томе.
pub fn disk_space(c: char) -> Option<(u64, u64)> {
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(root(c).as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some((total, total_free))
}

/// (файловая система, метка) тома.
pub fn volume_info(c: char) -> Option<(String, String)> {
    let mut name = [0u16; 64];
    let mut fs = [0u16; 32];
    let (mut serial, mut maxlen, mut flags) = (0u32, 0u32, 0u32);
    let ok = unsafe {
        GetVolumeInformationW(
            root(c).as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            &mut serial,
            &mut maxlen,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    (ok != 0).then(|| (from_wide(&fs), from_wide(&name)))
}

pub fn set_label(c: char, label: &str) -> Result<(), String> {
    let ok = unsafe { SetVolumeLabelW(root(c).as_ptr(), wide(label).as_ptr()) };
    if ok != 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// Включает сжатие NTFS для корня — новые файлы будут сжиматься.
pub fn set_compression(c: char) -> Result<(), String> {
    unsafe {
        let h: HANDLE = CreateFileW(
            root(c).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        let fmt: u16 = 1; // COMPRESSION_FORMAT_DEFAULT
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            h,
            FSCTL_SET_COMPRESSION,
            &fmt as *const u16 as _,
            2,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        );
        let err = (ok == 0).then(last_error);
        CloseHandle(h);
        err.map_or(Ok(()), Err)
    }
}

/// Открывает папку или файл в Проводнике / программе по умолчанию.
pub fn shell_open(target: &str) {
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("open").as_ptr(),
            wide(target).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Процесс запущен с правами администратора.
pub fn is_elevated() -> bool {
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut e: TOKEN_ELEVATION = std::mem::zeroed();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenElevation,
            &mut e as *mut _ as _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(tok);
        ok != 0 && e.TokenIsElevated != 0
    }
}

/// Перезапуск с запросом прав администратора (UAC).
pub fn relaunch_elevated(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("runas").as_ptr(),
            super::wide_path(&exe).as_ptr(),
            wide(args).as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Держит мьютекс единственного экземпляра до конца работы.
pub struct InstanceGuard(HANDLE);
unsafe impl Send for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// `None`, если программа уже запущена.
pub fn single_instance() -> Option<InstanceGuard> {
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, wide("Local\\VramDisk.SingleInstance").as_ptr());
        if h.is_null() {
            return Some(InstanceGuard(h));
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(h);
            return None;
        }
        Some(InstanceGuard(h))
    }
}

/// Показывает окно уже запущенной копии.
pub fn activate_window(title: &str) -> bool {
    unsafe {
        let hwnd: HWND = FindWindowW(std::ptr::null(), wide(title).as_ptr());
        if hwnd.is_null() {
            return false;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
        true
    }
}

pub fn error_box(title: &str, text: &str) {
    unsafe {
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), MB_OK | MB_ICONERROR);
    }
}

/// Принудительно снимает том прямо из процесса (как `imdisk -D`, но без
/// запуска imdisk.exe — при выключении новые процессы ненадёжны).
pub fn force_remove_volume(c: char) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, FILE_FLAG_NO_BUFFERING};
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA,
    };
    unsafe {
        let path = wide(&format!("\\\\.\\{c}:"));
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        let mut dw = 0u32;
        let mut ioctl = |code: u32| {
            DeviceIoControl(h, code, std::ptr::null(), 0, std::ptr::null_mut(), 0, &mut dw as *mut u32, std::ptr::null_mut())
                != 0
        };
        FlushFileBuffers(h);
        if !ioctl(FSCTL_LOCK_VOLUME) {
            ioctl(FSCTL_DISMOUNT_VOLUME);
            ioctl(FSCTL_LOCK_VOLUME);
        } else {
            ioctl(FSCTL_DISMOUNT_VOLUME);
        }
        let ejected = ioctl(IOCTL_STORAGE_EJECT_MEDIA);
        let err = (!ejected).then(last_error);
        ioctl(FSCTL_UNLOCK_VOLUME);
        CloseHandle(h);
        remove_drive_letter(c);
        err.map_or(Ok(()), Err)
    }
}

/// Куда указывает буква диска (`\Device\ImDisk0` и т. п.).
pub fn dos_target(c: char) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let mut buf = vec![0u16; 1024];
    let n = unsafe { QueryDosDeviceW(wide(&format!("{c}:")).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (n > 0).then(|| from_wide(&buf))
}

/// Убирает букву диска так же, как ImDisk: и глобальную ссылку (её видят
/// все, в том числе Проводник без прав администратора), и локальную.
pub fn remove_drive_letter(c: char) {
    use windows_sys::Win32::Storage::FileSystem::{DefineDosDeviceW, DDD_REMOVE_DEFINITION};
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_DRIVEREMOVED, SHCNF_PATHW};
    unsafe {
        let root = wide(&format!("{c}:\\"));
        SHChangeNotify(SHCNE_DRIVEREMOVED as i32, SHCNF_PATHW, root.as_ptr() as _, std::ptr::null());
        DefineDosDeviceW(DDD_REMOVE_DEFINITION, wide(&format!("Global\\{c}:")).as_ptr(), std::ptr::null());
        DefineDosDeviceW(DDD_REMOVE_DEFINITION, wide(&format!("{c}:")).as_ptr(), std::ptr::null());
    }
}

/// Мьютекс фоновой части: она одна на сеанс. Принадлежит потоку, который
/// его получил, и освобождается при `drop` (вызывать из того же потока).
pub struct ServerGuard(HANDLE);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// Становится единственной фоновой частью. Предыдущая после отключения
/// диска ещё несколько секунд отдаёт окну последние события — её ждём до
/// `wait`. `None` — другая фоновая часть работает.
pub fn server_instance(wait: std::time::Duration) -> Option<ServerGuard> {
    use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 1, wide("Local\\VramDisk.Server").as_ptr());
        if h.is_null() {
            crate::logger::log(format!("мьютекс фоновой части: {}", last_error()));
            return None;
        }
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return Some(ServerGuard(h));
        }
        match WaitForSingleObject(h, wait.as_millis().min(u32::MAX as u128 - 1) as u32) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Some(ServerGuard(h)),
            _ => {
                CloseHandle(h);
                None
            }
        }
    }
}

/// Работает ли процесс `pid` и это ли VramDisk (номера процессов
/// повторяются, поэтому сверяем и имя файла программы).
pub fn process_is_vramdisk(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return false;
    }
    unsafe {
        let h = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // Процесса нет (или к нему нет доступа — значит, он не наш).
            return false;
        }
        let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        let mut buf = vec![0u16; 1024];
        let mut n = buf.len() as u32;
        let named = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n) != 0;
        CloseHandle(h);
        if !alive {
            return false;
        }
        if !named {
            return true;
        }
        let theirs = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
        let ours = std::env::current_exe().ok();
        let name = |p: &std::path::Path| p.file_name().map(|f| f.to_string_lossy().to_lowercase());
        match ours {
            Some(o) => name(&theirs) == name(&o),
            None => true,
        }
    }
}

/// Память этого процесса: (частный рабочий набор — как в Диспетчере задач,
/// если Windows его сообщает; выделено всего).
pub fn process_memory() -> Option<(Option<u64>, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX, PROCESS_MEMORY_COUNTERS_EX2,
    };
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) != 0 {
            let pws = (c.PrivateWorkingSetSize > 0).then_some(c.PrivateWorkingSetSize as u64);
            return Some((pws, c.PrivateUsage as u64));
        }
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) != 0 {
            return Some((None, c.PrivateUsage as u64));
        }
        None
    }
}

/// Номер сеанса Windows этого процесса.
pub fn session_id() -> u32 {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut s = 0u32;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut s) };
    s
}

/// Запускает эту же программу с аргументами — отдельно от окна: вне задания
/// (job), в которое могло попасть окно, и с папкой программы как текущей
/// (не на диске VramDisk — иначе его нельзя будет отключить).
pub fn spawn_self(args: &[&str]) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().map(std::path::Path::to_path_buf).unwrap_or_else(crate::settings::app_dir);
    let run = |flags: u32| {
        std::process::Command::new(&exe)
            .args(args)
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    // Выйти из задания можно не всегда (если оно запрещает) — тогда как есть.
    run(CREATE_BREAKAWAY_FROM_JOB).or_else(|_| run(0)).map_err(|e| e.to_string())
}

/// Вопрос «Да/Нет»; `true` — «Да».
pub fn ask_box(title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO};
    unsafe {
        MessageBoxW(std::ptr::null_mut(), wide(text).as_ptr(), wide(title).as_ptr(), MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND)
            == IDYES
    }
}
