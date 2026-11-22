//! 短时探测子进程的统一执行器（W16）：`--version`、`auth status`、`--help`、`features list`、
//! 问包管理器全局目录这类"跑一下、读点文字、很快应该结束"的调用都走这里。
//!
//! 原来各处都是 `timeout(.., child.wait_with_output())`：超时只是丢掉等待的 future，子进程
//! （以及它拉起的子孙，例如 `.cmd` → `node`）继续跑；输出还是无上限读进内存。这里统一成：
//!
//! - **受控**：启动时就自成进程组（Unix），超时或等待出错时终止整棵进程树并回收
//!   （[`process::kill_process_tree`]：Windows 是 `taskkill /T /F`，Unix 对整组发信号）；
//!   调用方丢掉整个 future（被取消）时，[`TreeGuard`] 同步兜底结束整棵树；
//! - **输出有上限**：stdout / stderr 各只留前 [`OUTPUT_CAP`] 字节，超出的部分**读出来丢掉**
//!   （不截断后直接杀：探测输出多一点不是错，杀了反而丢掉版本号之后的退出码）。必须继续
//!   排空管道，否则子进程写满管道就卡住、永远等不到退出；
//! - **管道被孙子进程占着**：直接子进程退出后，输出最多再等 [`POST_EXIT_DRAIN`]，
//!   之后用已经读到的部分，不为一个后台孙子进程拖到整体超时。这时**不去杀**那个孙子进程
//!   （它可能是包管理器有意留下的后台服务；直接子进程已经收尸，进程组号也不再可靠）。

use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::process;

/// stdout / stderr 各自最多保留的字节数。
pub const OUTPUT_CAP: usize = 64 * 1024;

/// 直接子进程退出之后，再等多久让输出管道读到头。
const POST_EXIT_DRAIN: Duration = Duration::from_secs(1);

/// 终止进程树并收尸的整体上限；超过就退回到只杀直接子进程，不再等。
const REAP_LIMIT: Duration = Duration::from_secs(8);

/// 探测失败的原因。
#[derive(Debug)]
pub enum RunError {
    /// 启动失败（程序不存在、没权限、`.cmd` 参数转义被拒……）。
    Spawn(std::io::Error),
    /// 超时：整棵进程树已被终止并回收。
    Timeout,
    /// 等待子进程时出错：同样已终止进程树。
    Wait(std::io::Error),
}

/// 探测的结果：退出状态和（已封顶的）输出。
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// 跑 `cmd`，最多等 `timeout`。
///
/// 标准输入、输出一律由这里设（输入给空，输出走管道），调用方只管程序和参数。
pub async fn run(mut cmd: tokio::process::Command, timeout: Duration) -> Result<Output, RunError> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 最后一道兜底（只杀直接子进程）；整棵树靠下面的 TreeGuard 和显式终止。
        .kill_on_drop(true);
    crate::process_util::hide_console_async(&mut cmd);
    // Windows 没有进程组，结束整棵树靠 `taskkill /T`（按父子关系找）；Unix 在这里自成一组。
    #[cfg(unix)]
    process::isolate_process_group(&mut cmd);

    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let mut guard = TreeGuard(child.id());
    let (stdout, mut stdout_task) = capped_reader(child.stdout.take());
    let (stderr, mut stderr_task) = capped_reader(child.stderr.take());

    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let status = match waited {
        Ok(Ok(status)) => {
            guard.disarm();
            status
        }
        failed => {
            terminate(&mut child).await;
            guard.disarm();
            stdout_task.abort();
            stderr_task.abort();
            return Err(match failed {
                Ok(Err(err)) => RunError::Wait(err),
                _ => RunError::Timeout,
            });
        }
    };

    let drained = async {
        let _ = (&mut stdout_task).await;
        let _ = (&mut stderr_task).await;
    };
    if tokio::time::timeout(POST_EXIT_DRAIN, drained)
        .await
        .is_err()
    {
        stdout_task.abort();
        stderr_task.abort();
    }
    Ok(Output {
        status,
        stdout: take(&stdout),
        stderr: take(&stderr),
    })
}

/// 读到头为止，但只留前 [`OUTPUT_CAP`] 字节；返回共享缓冲和读任务（超时放弃时缓冲里已有的部分仍可用）。
fn capped_reader<R>(reader: Option<R>) -> (Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let buf = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&buf);
    let task = tokio::spawn(async move {
        let Some(mut reader) = reader else {
            return;
        };
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut kept = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let room = OUTPUT_CAP.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    });
    (buf, task)
}

fn take(buf: &Mutex<Vec<u8>>) -> Vec<u8> {
    std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()))
}

/// 终止整棵进程树并回收直接子进程。卡住（超过 [`REAP_LIMIT`]）就退回只杀直接子进程。
async fn terminate(child: &mut tokio::process::Child) {
    let reaped = tokio::time::timeout(REAP_LIMIT, process::kill_process_tree(child)).await;
    if reaped.is_err() {
        let _ = child.start_kill();
    }
}

/// 调用方丢掉整个 `run` future（取消）时的同步兜底：结束整棵树。
/// 子进程正常退出或已显式终止后解除，免得进程号被别的进程重用时误杀。
struct TreeGuard(Option<u32>);

impl TreeGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            process::kill_tree_now(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// 本测试程序自己扮演"会挂住、会拉起子孙进程、会狂吐输出"的被探测程序：
    /// 带 `ZT_PROBE_HELPER` 环境变量重新启动自己，只跑 `helper_body` 这一条。
    const HELPER: &str = "ai::agent::short_probe::tests::helper_body";

    fn helper_command(mode: &str, pid_file: Option<&std::path::Path>) -> tokio::process::Command {
        let exe = std::env::current_exe().expect("测试程序路径");
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
            .env("ZT_PROBE_HELPER", mode);
        if let Some(file) = pid_file {
            cmd.env("ZT_PROBE_HELPER_PID_FILE", file);
        }
        cmd
    }

    /// 平时一跑就过；被上面的命令拉起时按 `ZT_PROBE_HELPER` 干活。
    #[test]
    fn helper_body() {
        let Ok(mode) = std::env::var("ZT_PROBE_HELPER") else {
            return;
        };
        let spawn_sleeper = |inherit_stdout: bool| {
            #[cfg(windows)]
            let mut sleeper = {
                let mut c = std::process::Command::new("ping");
                c.args(["-n", "60", "127.0.0.1"]);
                c
            };
            #[cfg(unix)]
            let mut sleeper = {
                let mut c = std::process::Command::new("sleep");
                c.arg("60");
                c
            };
            crate::process_util::hide_console(&mut sleeper);
            sleeper.stdin(std::process::Stdio::null());
            sleeper.stderr(std::process::Stdio::null());
            if !inherit_stdout {
                sleeper.stdout(std::process::Stdio::null());
            }
            // 故意不 wait：它就是要比这个辅助进程活得久，由主测试按进程号收拾。
            #[allow(clippy::zombie_processes)]
            let child = sleeper.spawn().expect("起长寿子进程");
            if let Some(file) = std::env::var_os("ZT_PROBE_HELPER_PID_FILE") {
                // 先写临时文件再改名，读的一方不会读到半截。
                let file = std::path::PathBuf::from(file);
                let tmp = file.with_extension("tmp");
                std::fs::write(&tmp, child.id().to_string()).expect("写进程号");
                std::fs::rename(&tmp, &file).expect("改名");
            }
        };
        match mode.as_str() {
            // 拉起一个长寿孙子进程，然后自己也一直挂着。
            "hang" => {
                spawn_sleeper(false);
                std::thread::sleep(Duration::from_secs(120));
            }
            // 拉起一个拿着输出管道的孙子进程，自己马上正常退出。
            "daemon" => {
                spawn_sleeper(true);
            }
            // 狂吐 5 MiB。
            "flood" => {
                let block = vec![b'x'; 64 * 1024];
                let mut out = std::io::stdout().lock();
                for _ in 0..80 {
                    let _ = out.write_all(&block);
                }
                let _ = out.flush();
                let mut err = std::io::stderr().lock();
                for _ in 0..80 {
                    let _ = err.write_all(&block);
                }
            }
            _ => {}
        }
    }

    async fn read_pid_file(path: &std::path::Path) -> u32 {
        for _ in 0..400 {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("等不到进程号文件 {}", path.display());
    }

    async fn wait_gone(pid: u32) -> bool {
        for _ in 0..200 {
            if process::process_is_alive(pid) == Some(false) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    /// 清理测试里故意留下的长寿进程。
    fn force_kill(pid: u32) {
        #[cfg(windows)]
        {
            let mut kill = std::process::Command::new("taskkill");
            kill.args(["/PID", &pid.to_string(), "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let _ = crate::process_util::hide_console(&mut kill).status();
        }
        #[cfg(unix)]
        {
            if let Ok(pid) = libc::pid_t::try_from(pid) {
                // SAFETY: 只给测试里自己拉起的、已确认号码的进程发 SIGKILL。
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    /// 修复前：超时后孙子进程继续活着；现在整棵树都要被终止并回收。
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        let dir = crate::test_util::TestDir::new("zssh probe hang");
        let pid_file = dir.join("grandchild.pid");
        let cmd = helper_command("hang", Some(&pid_file));

        let started = std::time::Instant::now();
        let result = run(cmd, Duration::from_millis(1500)).await;
        assert!(matches!(result, Err(RunError::Timeout)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时后终止应该很快：{:?}",
            started.elapsed()
        );

        let grandchild = read_pid_file(&pid_file).await;
        if !wait_gone(grandchild).await {
            force_kill(grandchild);
            panic!("超时之后孙子进程 {grandchild} 还活着");
        }
    }

    /// 调用方丢掉整个 future（取消）时，兜底也要结束整棵树。
    #[tokio::test]
    async fn dropping_the_future_kills_the_whole_process_tree() {
        let dir = crate::test_util::TestDir::new("zssh probe drop");
        let pid_file = dir.join("grandchild.pid");
        let cmd = helper_command("hang", Some(&pid_file));

        let outcome = tokio::time::timeout(Duration::from_secs(60), async {
            let probe = run(cmd, Duration::from_secs(120));
            tokio::pin!(probe);
            tokio::select! {
                _ = &mut probe => unreachable!("120 秒的探测不该自己结束"),
                pid = read_pid_file(&pid_file) => pid,
            }
        })
        .await
        .expect("等孙子进程出现");
        // select 结束时 `probe` 随作用域一起被丢掉。
        if !wait_gone(outcome).await {
            force_kill(outcome);
            panic!("future 被丢掉之后孙子进程 {outcome} 还活着");
        }
    }

    /// 修复前：5 MiB 输出全读进内存；现在各封顶 64 KiB，而且管道被排空、不会把子进程卡死。
    #[tokio::test]
    async fn huge_output_is_capped_and_the_pipe_is_still_drained() {
        let started = std::time::Instant::now();
        let output = run(helper_command("flood", None), Duration::from_secs(60))
            .await
            .expect("跑完");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), OUTPUT_CAP);
        assert_eq!(output.stderr.len(), OUTPUT_CAP);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
    }

    /// 直接子进程正常退出、孙子进程却拿着输出管道不放：不能拖到整体超时，用已读到的部分返回。
    #[tokio::test]
    async fn a_grandchild_holding_the_pipe_does_not_stall_a_finished_probe() {
        let dir = crate::test_util::TestDir::new("zssh probe daemon");
        let pid_file = dir.join("grandchild.pid");
        let started = std::time::Instant::now();
        let result = run(
            helper_command("daemon", Some(&pid_file)),
            Duration::from_secs(40),
        )
        .await;
        let elapsed = started.elapsed();
        let grandchild = read_pid_file(&pid_file).await;
        force_kill(grandchild);
        let output = result.expect("直接子进程已正常退出");
        assert!(output.status.success());
        assert!(
            elapsed < Duration::from_secs(20),
            "不该等孙子进程或整体超时：{elapsed:?}"
        );
    }

    #[tokio::test]
    async fn spawn_failure_is_reported_as_spawn() {
        let dir = crate::test_util::TestDir::new("zssh probe missing");
        let missing = dir.join("no-such-program");
        let result = run(
            tokio::process::Command::new(missing),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(result, Err(RunError::Spawn(_))), "{result:?}");
    }
}
