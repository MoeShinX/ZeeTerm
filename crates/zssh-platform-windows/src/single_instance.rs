//! 单实例运行：命名互斥体（Windows）/ 文件锁（Unix）+ 主窗口属性。
//!
//! 第一个实例拿到命名互斥体，并给自己的主窗口挂一个属性；后启动的实例拿不到
//! 互斥体，就按属性找到已有窗口、还原并置前，然后退出。
//!
//! macOS 上暂时没有等价的“按属性找窗口并置前”的公开接口：拿不到锁的第二个实例会直接退出，
//! 不会把已有窗口带到前台。这是 P0 的已知缺口，不把它记成已完成。

#[cfg(windows)]
mod imp {
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
}

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;

    /// 文件锁句柄：进程退出（或崩溃）时内核自动释放，不会留下“永远占着”的陈旧锁。
    pub struct InstanceGuard {
        file: Option<File>,
        path: PathBuf,
    }

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            // 只关闭句柄；锁文件保留（0 字节），删掉它会在两个进程同时启动时产生竞态。
            if let Some(file) = self.file.take() {
                // SAFETY: fd 来自仍然打开的 File，调用期间有效。
                unsafe {
                    let _ = libc::flock(file.as_raw_fd(), libc::LOCK_UN);
                }
            }
            let _ = &self.path;
        }
    }

    /// 锁文件放在临时目录（不是数据目录）：它只是进程间互斥，丢了不影响任何用户数据。
    /// 名字里带 uid，多用户共用一台机器时互不影响。
    fn lock_path(name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        // SAFETY: getuid 无参数、只读进程凭证。
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir().join(format!("{safe}-{uid}.lock"))
    }

    /// 拿到文件锁；已有实例返回 `None`。打不开锁文件时不阻止启动（只留一条日志）。
    pub fn acquire(name: &str) -> Option<InstanceGuard> {
        let path = lock_path(name);
        let file = match OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(%err, "打不开单实例锁文件，跳过单实例检查");
                return Some(InstanceGuard { file: None, path });
            }
        };
        // SAFETY: fd 来自仍然打开的 File；非阻塞排他锁，调用立即返回。
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if locked {
            Some(InstanceGuard {
                file: Some(file),
                path,
            })
        } else {
            None
        }
    }

    /// macOS 还没有“按窗口属性找到并置前”的实现：第二个实例退出，不抢焦点。
    pub fn focus_existing(_prop: &str) -> bool {
        false
    }

    /// 没有 Win32 窗口属性可挂；返回 `false` 表示未执行任何操作。
    pub fn set_window_prop(_hwnd: isize, _prop: &str) -> bool {
        false
    }
}

pub use imp::{acquire, focus_existing, set_window_prop, InstanceGuard};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_existing_instance() {
        let name = format!("zssh-single-instance-test-{}", std::process::id());
        let first = acquire(&name).expect("第一次应拿到锁");
        assert!(acquire(&name).is_none(), "第二次应报告已有实例");
        drop(first);
        assert!(acquire(&name).is_some(), "释放后应能重新拿到");
    }
}
