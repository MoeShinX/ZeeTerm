//! 等另一个进程退出。免安装版自更新后重启自己时用：新进程要先等旧进程退出（释放单实例互斥体），
//! 否则会被当成"已经有一个在运行"而直接退出。

#[cfg(windows)]
use std::time::Duration;

/// 等进程号为 `pid` 的进程退出，最多等 `timeout`。返回进程是否已经不在了。
///
/// 打不开那个进程（已经退出、或者不是同一个用户的）按"已经不在了"处理，不阻塞启动。
/// 超时返回 `false`：调用方照样继续启动，最坏的结果是被单实例机制拦下并把旧窗口叫到前台。
#[cfg(windows)]
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    // WaitForSingleObject 的超时是毫秒，u32::MAX 表示无限等，所以留一格。
    let millis = u32::try_from(timeout.as_millis()).map_or(u32::MAX - 1, |ms| ms.min(u32::MAX - 1));
    // SAFETY: 句柄只在本函数里用，等完立刻关掉；OpenProcess 失败（进程已经不在）直接返回。
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return true;
        };
        let result = WaitForSingleObject(handle, millis);
        let _ = CloseHandle(handle);
        result == WAIT_OBJECT_0
    }
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_that_already_exited_is_not_waited_for() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .expect("起一个马上退出的进程");
        let pid = child.id();
        child.wait().expect("等它退出");
        assert!(wait_for_exit(pid, Duration::from_secs(5)));
    }

    #[test]
    fn a_running_process_times_out_instead_of_blocking_forever() {
        // 自己当然还在运行：只等一小会儿就该返回 false。
        let started = std::time::Instant::now();
        assert!(!wait_for_exit(
            std::process::id(),
            Duration::from_millis(100)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
