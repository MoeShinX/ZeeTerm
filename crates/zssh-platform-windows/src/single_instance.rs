//! 单实例运行：命名互斥体 + 主窗口属性。
//!
//! 第一个实例拿到命名互斥体，并给自己的主窗口挂一个属性；后启动的实例拿不到
//! 互斥体，就按属性找到已有窗口、还原并置前，然后退出。

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetPropW, SetForegroundWindow, SetPropW, ShowWindow, SW_RESTORE,
};

pub struct InstanceGuard(HANDLE);

// 互斥体句柄本身可以跨线程传递。
unsafe impl Send for InstanceGuard {}
unsafe impl Sync for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 拿到命名互斥体；已有实例返回 `None`。创建失败时不阻止启动。
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

struct FocusArgs {
    prop: *const u16,
    found: bool,
}

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
pub fn set_window_prop(hwnd: isize, prop: &str) -> bool {
    let prop = wide(prop);
    unsafe { SetPropW(HWND(hwnd as _), PCWSTR(prop.as_ptr()), Some(HANDLE(1 as _))).is_ok() }
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
