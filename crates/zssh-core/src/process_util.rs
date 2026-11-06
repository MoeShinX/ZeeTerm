//! 启动子进程的公共小工具。
//!
//! 发行版的 ZeeTerm 是 GUI 子系统（没有控制台）。GUI 程序去启动一个控制台程序时，
//! 系统会为这个子进程新建一个控制台窗口，用户就会看到一闪而过（或一直挂着）的黑框。
//! 凡是启动控制台程序、又不需要用户看到窗口的地方，都在 `spawn` / `output` / `status`
//! 之前调用这里的函数。
//!
//! `CREATE_NO_WINDOW` 只影响控制台的创建，不影响标准输入输出的管道，所以管道通信照常。
//! 不要给 ConPTY 的路径用：ConPTY 本来就不弹窗，标志加错地方反而会让伪终端拿不到控制台。
//! 启动 GUI 程序（资源管理器、VcXsrv、浏览器、安装包）不需要它。

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 不给标准库 `Command` 启动的控制台子进程弹窗口。非 Windows 上什么都不做。
pub fn hide_console(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 同 [`hide_console`]，对象是 tokio 的 `Command`。
pub fn hide_console_async(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 隐藏窗口之后管道照常通：stdout 能读到、退出码能拿到。
    #[cfg(windows)]
    #[test]
    fn piped_output_still_works_with_the_flag() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/D", "/C", "echo", "hidden-ok"]);
        let out = hide_console(&mut cmd).output().expect("cmd 能跑");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hidden-ok"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn piped_output_still_works_with_the_flag_async() {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/D", "/C", "echo", "hidden-ok"]);
        let out = hide_console_async(&mut cmd)
            .output()
            .await
            .expect("cmd 能跑");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hidden-ok"));
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    /// 非 Windows 上 `hide_console` 什么都不做，管道照常通：stdout 能读到、退出码能拿到。
    #[test]
    fn piped_output_works_through_the_no_op_helper() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hidden-ok"]);
        let out = hide_console(&mut cmd).output().expect("sh 能跑");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hidden-ok"));
    }

    #[tokio::test]
    async fn piped_output_works_through_the_no_op_helper_async() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "echo hidden-ok"]);
        let out = hide_console_async(&mut cmd)
            .output()
            .await
            .expect("sh 能跑");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hidden-ok"));
    }
}
