//! Снятие диска при выключении и сбоях.
//!
//! Драйвер ImDisk ждёт ответа сервера без тайм-аута. Если процесс завершится
//! раньше, чем диск снят, Windows при выключении может зависнуть на сбросе
//! кэша этого тома. Поэтому при `WM_ENDSESSION` (и при панике) диск
//! принудительно снимается прямо в процессе — пока сервер ещё жив.

use super::wide;
use crate::logger::log;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, SetProcessShutdownParameters};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumThreadWindows, GetMessageW, GetWindowTextW,
    RegisterClassExW, TranslateMessage, MSG, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSEXW, WS_OVERLAPPED,
};

type Cleanup = Box<dyn FnOnce() + Send>;

static CLEANUP: Mutex<Option<Cleanup>> = Mutex::new(None);

/// Что сделать при выключении (обычно — принудительно снять диск).
pub fn set_cleanup(f: Option<Cleanup>) {
    *CLEANUP.lock().unwrap_or_else(|e| e.into_inner()) = f;
}

/// Выполняет действие один раз; параллельный вызов ждёт окончания первого.
pub fn run_cleanup(why: &str) {
    let mut g = CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = g.take() {
        log(format!("{why}: снимаю диск"));
        f();
        log(format!("{why}: готово"));
    }
}

unsafe extern "system" fn watcher_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wp != 0 {
                ShutdownBlockReasonCreate(hwnd, wide("Отключение диска VramDisk…").as_ptr());
                run_cleanup("завершение сеанса Windows");
                ShutdownBlockReasonDestroy(hwnd);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Невидимое окно в отдельном потоке: получает сообщения о выключении,
/// даже если главное окно занято.
pub fn start_watcher() {
    // Нас завершают последними среди программ: они успеют закрыть файлы на диске.
    unsafe { SetProcessShutdownParameters(0x100, 0) };
    let _ = std::thread::Builder::new().name("session-watcher".into()).spawn(|| unsafe {
        let class = wide("VramDiskSessionWatcher");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(watcher_proc),
            hInstance: GetModuleHandleW(std::ptr::null()),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("VramDisk — выключение").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wc.hInstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            log("не удалось создать окно наблюдения за выключением");
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

unsafe extern "system" fn main_subclass(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_ENDSESSION && wp != 0 {
        run_cleanup("завершение сеанса Windows (главное окно)");
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

static mut FOUND: bool = false;

unsafe extern "system" fn enum_proc(hwnd: HWND, _: LPARAM) -> windows_sys::core::BOOL {
    let mut buf = [0u16; 64];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == crate::APP_TITLE {
        SetWindowSubclass(hwnd, Some(main_subclass), 1, 0);
        FOUND = true;
        return 0;
    }
    1
}

/// Подключается к главному окну (вызывать из потока интерфейса, пока не вернёт true).
pub fn hook_main_window() -> bool {
    unsafe {
        FOUND = false;
        EnumThreadWindows(GetCurrentThreadId(), Some(enum_proc), 0);
        FOUND
    }
}
