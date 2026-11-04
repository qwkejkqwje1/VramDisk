//! Службы Windows: проверка и запуск драйвера ImDisk.

use super::{error_text, wide};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QueryServiceStatus, StartServiceW,
    QUERY_SERVICE_CONFIGW, SC_HANDLE, SC_MANAGER_CONNECT, SERVICE_DISABLED, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS, SERVICE_STOPPED,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// Службы нет.
    Missing,
    /// Тип запуска «Отключена».
    Disabled,
    /// Не запустилась: код ошибки Windows.
    Start(u32),
    Timeout,
    /// Узнать не удалось (нет прав и т. п.).
    Unknown(String),
}

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn status(svc: &Handle) -> Result<SERVICE_STATUS, ServiceError> {
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { QueryServiceStatus(svc.0, &mut st) } == 0 {
        return Err(ServiceError::Unknown(format!("QueryServiceStatus: {}", error_text(unsafe { GetLastError() }))));
    }
    Ok(st)
}

/// Тип запуска службы (`None` — узнать не удалось).
fn start_type(svc: &Handle) -> Option<u32> {
    let mut need = 0u32;
    unsafe { QueryServiceConfigW(svc.0, std::ptr::null_mut(), 0, &mut need) };
    if need == 0 {
        return None;
    }
    // Буфер с выравниванием под структуру.
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    let cfg = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
    if unsafe { QueryServiceConfigW(svc.0, cfg, need, &mut need) } == 0 {
        return None;
    }
    Some(unsafe { (*cfg).dwStartType })
}

/// Убедиться, что служба (драйвер) `name` запущена; если остановлена —
/// запустить и подождать.
pub fn ensure_running(name: &str) -> Result<(), ServiceError> {
    let scm = Handle(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) });
    if scm.0.is_null() {
        return Err(ServiceError::Unknown(format!("диспетчер служб: {}", error_text(unsafe { GetLastError() }))));
    }
    let n = wide(name);
    let mut svc = Handle(unsafe { OpenServiceW(scm.0, n.as_ptr(), SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG | SERVICE_START) });
    let mut can_start = true;
    if svc.0.is_null() && unsafe { GetLastError() } == ERROR_ACCESS_DENIED {
        can_start = false;
        svc = Handle(unsafe { OpenServiceW(scm.0, n.as_ptr(), SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG) });
    }
    if svc.0.is_null() {
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_SERVICE_DOES_NOT_EXIST {
            ServiceError::Missing
        } else {
            ServiceError::Unknown(format!("служба {name}: {}", error_text(code)))
        });
    }
    let st = status(&svc)?;
    if st.dwCurrentState == SERVICE_RUNNING {
        return Ok(());
    }
    if start_type(&svc) == Some(SERVICE_DISABLED) {
        return Err(ServiceError::Disabled);
    }
    if st.dwCurrentState == SERVICE_STOPPED {
        if !can_start {
            return Err(ServiceError::Unknown(format!("служба {name} остановлена, а прав на её запуск нет")));
        }
        crate::logger::log(format!("служба {name} остановлена — запускаю"));
        if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_SERVICE_ALREADY_RUNNING {
                crate::logger::log(format!("служба {name} не запустилась: {}", error_text(code)));
                return Err(if code == 1058 { ServiceError::Disabled } else { ServiceError::Start(code) });
            }
        }
    }
    let t = Instant::now();
    loop {
        let st = status(&svc)?;
        if st.dwCurrentState == SERVICE_RUNNING {
            crate::logger::log(format!("служба {name} работает"));
            return Ok(());
        }
        if st.dwCurrentState == SERVICE_STOPPED && t.elapsed() > Duration::from_millis(500) {
            let code = st.dwWin32ExitCode;
            return Err(if code == 0 { ServiceError::Timeout } else { ServiceError::Start(code) });
        }
        if t.elapsed() > Duration::from_secs(10) {
            return Err(ServiceError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
