//! 单实例运行：Windows 用命名互斥体 + 主窗口属性；Unix 用运行目录里的 PID 锁文件。
//!
//! 第一个实例拿到锁，后启动的实例拿不到就退出。Windows 还会把已有窗口还原置前；
//! Unix 的窗口聚焦需要 IPC，留到平台界面阶段接。

#[cfg(windows)]
use windows::core::{BOOL, PCWSTR};
#[cfg(windows)]
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM,
};
#[cfg(windows)]
use windows::Win32::System::Threading::CreateMutexW;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetPropW, SetForegroundWindow, SetPropW, ShowWindow, SW_RESTORE,
};

#[cfg(windows)]
pub struct InstanceGuard(HANDLE);

// 互斥体句柄本身可以跨线程传递。
#[cfg(windows)]
unsafe impl Send for InstanceGuard {}
#[cfg(windows)]
unsafe impl Sync for InstanceGuard {}

#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 拿到命名互斥体；已有实例返回 `None`。创建失败时不阻止启动。
#[cfg(windows)]
pub fn acquire(name: &str) -> Option<InstanceGuard> {
    let name = wide(name);
    unsafe {
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    None
                } else {
                    Some(InstanceGuard(handle))
                }
            }
            Err(_) => Some(InstanceGuard(HANDLE::default())),
        }
    }
}

#[cfg(windows)]
struct FocusArgs {
    prop: *const u16,
    found: bool,
}

#[cfg(windows)]
unsafe extern "system" fn focus_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let args = unsafe { &mut *(lparam.0 as *mut FocusArgs) };
    let prop = unsafe { GetPropW(hwnd, PCWSTR(args.prop)) };
    if !prop.is_invalid() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        args.found = true;
        BOOL(0)
    } else {
        BOOL(1)
    }
}

/// 找到挂了指定属性的窗口并置前；返回是否找到。
#[cfg(windows)]
pub fn focus_existing(prop: &str) -> bool {
    let name = wide(prop);
    let mut args = FocusArgs {
        prop: name.as_ptr(),
        found: false,
    };
    unsafe {
        let _ = EnumWindows(Some(focus_proc), LPARAM(&mut args as *mut _ as isize));
    }
    args.found
}

/// 给窗口挂属性，供后启动的实例查找。
#[cfg(windows)]
pub fn set_window_prop(hwnd: isize, prop: &str) -> bool {
    let prop = wide(prop);
    unsafe { SetPropW(HWND(hwnd as _), PCWSTR(prop.as_ptr()), Some(HANDLE(1 as _))).is_ok() }
}

// ---------------------------------------------------------------------------
// Unix：运行目录里的 PID 锁文件
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub struct InstanceGuard {
    path: std::path::PathBuf,
    /// 拿不到文件时仍要允许启动，所以失败路径是 `None`。
    _file: Option<std::fs::File>,
}

#[cfg(unix)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if self._file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn lock_path(name: &str) -> std::path::PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("{safe}.lock"))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::path::Path::new("/proc")
        .join(pid.to_string())
        .exists()
}

/// 拿到锁文件；已有实例返回 `None`。创建失败时不阻止启动。
#[cfg(unix)]
pub fn acquire(name: &str) -> Option<InstanceGuard> {
    use std::io::Write as _;

    let path = lock_path(name);
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = write!(file, "{}", std::process::id());
                return Some(InstanceGuard {
                    path,
                    _file: Some(file),
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let pid = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                if pid.is_some_and(process_alive) {
                    return None;
                }
                // 上次异常退出留下的锁：清掉再试一次。
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {
                return Some(InstanceGuard {
                    path,
                    _file: None,
                })
            }
        }
    }
    None
}

/// Unix 暂不支持把已有窗口置前（需要 IPC），返回 `false`。
#[cfg(unix)]
pub fn focus_existing(_prop: &str) -> bool {
    false
}

/// Unix 没有窗口属性，返回 `false`。
#[cfg(unix)]
pub fn set_window_prop(_hwnd: isize, _prop: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_existing_instance() {
        let name = format!("zssh-single-instance-test-{}", std::process::id());
        let first = acquire(&name).expect("第一次应拿到互斥体");
        assert!(acquire(&name).is_none(), "第二次应报告已有实例");
        drop(first);
        assert!(acquire(&name).is_some(), "释放后应能重新拿到");
    }
}
