//! 键盘 / 鼠标最近一次操作是什么时候（给“空闲多久自动锁定”用，V2-U26）。
//!
//! 要的是“**ZeeTerm 的窗口**里最近一次操作”，不是整台电脑的空闲时间（`GetLastInputInfo`）：
//! 用户在别的程序里干活时，ZeeTerm 照样算空闲，这正是自动锁定要防的情况。
//!
//! 做法：给界面线程装一个 `WH_GETMESSAGE` 线程钩子。这个进程所有窗口（主窗口、文件窗口、
//! 更新窗口……）的消息都从这条线程的消息队列过，钩子在消息被取出时看一眼：是键盘 / 鼠标 / 指针类
//! 就记下时间。钩子只读消息、不改、不拦，一律交给下一个处理者。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, HHOOK, MSG, WH_GETMESSAGE,
};

/// 进程内的时间零点；`LAST` 存“距零点多少毫秒”。
static EPOCH: OnceLock<Instant> = OnceLock::new();
static LAST: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// 这条窗口消息算不算“用户在操作”。
pub fn is_user_input(message: u32) -> bool {
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_SYSDEADCHAR: u32 = 0x0107;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_MOUSEHWHEEL: u32 = 0x020E;
    const WM_NCMOUSEMOVE: u32 = 0x00A0;
    const WM_NCXBUTTONDBLCLK: u32 = 0x00AE;
    const WM_POINTERUPDATE: u32 = 0x0245;
    const WM_POINTERWHEEL: u32 = 0x024E;
    matches!(message,
        WM_KEYDOWN..=WM_SYSDEADCHAR
        | WM_MOUSEMOVE..=WM_MOUSEHWHEEL
        | WM_NCMOUSEMOVE..=WM_NCXBUTTONDBLCLK
        | WM_POINTERUPDATE..=WM_POINTERWHEEL)
}

/// 手动记一次操作（测试和自动化用；钩子没装上时界面也可以用它兜底）。
pub fn touch() {
    LAST.store(now_ms(), Ordering::Relaxed);
}

/// 距离最近一次操作过了多久。从没记过时，算从第一次问起。
pub fn idle_for() -> Duration {
    Duration::from_millis(now_ms().saturating_sub(LAST.load(Ordering::Relaxed)))
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // code < 0 时按约定不处理，直接交给下一个。
    if code >= 0 && lparam.0 != 0 {
        // SAFETY: WH_GETMESSAGE 的 lParam 指向系统传来的 MSG，钩子返回前一直有效。
        let message = unsafe { (*(lparam.0 as *const MSG)).message };
        if is_user_input(message) {
            touch();
        }
    }
    // SAFETY: 标准写法：把消息交给钩子链上的下一个。
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 给**当前线程**（必须是界面线程）装钩子，并把“最近操作”记成现在。成功返回 `true`。
/// 进程里装一次就够；钩子随线程结束自动卸掉，所以不保存句柄。
pub fn install_on_current_thread() -> bool {
    touch();
    // SAFETY: 线程钩子（dwThreadId = 当前线程），回调是上面的 extern "system" 函数，不引用任何会失效的东西。
    let hook: windows::core::Result<HHOOK> =
        unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(hook_proc), None, GetCurrentThreadId()) };
    hook.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_mouse_and_pointer_messages_count_as_activity() {
        for message in [
            0x0100, // WM_KEYDOWN
            0x0104, // WM_SYSKEYDOWN
            0x0200, // WM_MOUSEMOVE
            0x0201, // WM_LBUTTONDOWN
            0x0204, // WM_RBUTTONDOWN
            0x020A, // WM_MOUSEWHEEL
            0x00A1, // WM_NCLBUTTONDOWN（点标题栏）
            0x0246, // WM_POINTERDOWN
        ] {
            assert!(is_user_input(message), "{message:#06x}");
        }
    }

    #[test]
    fn timers_paints_and_other_traffic_do_not() {
        for message in [
            0x000F, // WM_PAINT
            0x0113, // WM_TIMER
            0x0005, // WM_SIZE
            0x0007, // WM_SETFOCUS
            0x0400, // WM_USER
            0x0000,
        ] {
            assert!(!is_user_input(message), "{message:#06x}");
        }
    }

    #[test]
    fn touching_resets_the_idle_clock() {
        touch();
        assert!(idle_for() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(30));
        assert!(idle_for() >= Duration::from_millis(20));
        touch();
        assert!(idle_for() < Duration::from_millis(20));
    }
}
