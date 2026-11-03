//! 单元测试用的临时目录守卫。
//!
//! 目录建在系统临时目录下、名字以 `zssh-` 开头，`Drop` 时整棵删掉——
//! 断言失败、panic、提前 return 都会走到。需要在它之前释放的句柄/子进程，
//! 要声明在持有它的结构体里**更靠前**的字段（字段按声明顺序 drop）。

use std::ops::Deref;
use std::path::Path;

/// 临时目录守卫；可以直接当 `&Path` 用（`dir.join(..)`、`&dir`）。
pub struct TestDir(std::path::PathBuf);

impl TestDir {
    pub fn new(prefix: &str) -> Self {
        let prefix = if prefix.starts_with("zssh") {
            format!("{prefix}-")
        } else {
            format!("zssh-{prefix}-")
        };
        let dir = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .expect("create temp dir");
        // 自己接管删除：Windows 上后台线程还没放掉文件句柄时第一次删会失败，要重试一会儿。
        Self(dir.keep())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        for _ in 0..20 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        // 句柄被测试进程里还活着的后台任务占着（断言失败提前退出时会这样）：
        // 起一个脱离的 cmd，等本进程退出之后再删，保证任何退出路径都不留目录。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let _ = std::process::Command::new("cmd")
                .args(["/C", "ping -n 6 127.0.0.1 >nul & rmdir /S /Q"])
                .arg(&self.0)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        eprintln!("临时目录暂时没能删掉，已交给后台清理：{}", self.0.display());
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.0.as_path()
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.0.as_path()
    }
}

impl AsRef<std::ffi::OsStr> for TestDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_path().as_os_str()
    }
}

/// 在当前线程里抓 `tracing` 输出（含 TRACE 级别），返回闭包的结果和抓到的全部日志文字。
///
/// 用线程局部订阅者：异步用例要用单线程运行时（`#[tokio::test]` 默认就是），任务才会
/// 在当前线程上跑。别的用例先触发过的 callsite 会缓存“没人订阅”，所以要重建一次缓存。
pub(crate) fn capture_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    struct SinkWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for SinkWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut inner) = self.0.lock() {
                inner.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = SinkWriter;
        fn make_writer(&'a self) -> SinkWriter {
            SinkWriter(self.0.clone())
        }
    }

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(Sink(buffer.clone()))
        .finish();
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let logs = buffer
        .lock()
        .map(|inner| String::from_utf8_lossy(&inner).into_owned())
        .unwrap_or_default();
    (result, logs)
}

/// 判定一次本机 `sh` 的运行结果能不能用：能启动、退出码是 0、stdout 不为空。
/// 不能用就返回原因。纯函数，方便单测。
///
/// 为什么不只看“能不能启动”：装了损坏的 MSYS / Git Bash、WSL 混装的 Windows 上，
/// `sh.exe` 能启动却立刻崩（退出码非 0、什么都不输出），这不是被测代码的问题，测试
/// 应该跳过而不是失败（AGENTS.md：依赖本机环境的测试在环境不满足时跳过并打印原因）。
pub(crate) fn sh_outcome(result: std::io::Result<std::process::Output>) -> Result<String, String> {
    let output = result.map_err(|err| format!("本机没有可用的 sh（{err}）"))?;
    if !output.status.success() {
        return Err(format!("本机的 sh 退出码非 0（{}）", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() {
        return Err("本机的 sh 没有输出任何东西".to_owned());
    }
    Ok(stdout)
}

fn run_sh_raw(script: &str) -> std::io::Result<std::process::Output> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .output()
}

/// 本机 `sh` 能不能跑最小的脚本（`echo ok` 输出 `ok`）。结果缓存，整个测试进程只探一次。
pub(crate) fn sh_probe() -> Result<(), String> {
    static PROBE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    PROBE
        .get_or_init(|| {
            let out = sh_outcome(run_sh_raw("echo ok"))?;
            if out.trim() == "ok" {
                Ok(())
            } else {
                Err(format!(
                    "本机的 sh 对 echo ok 的回答不对（{:?}）",
                    out.trim()
                ))
            }
        })
        .clone()
}

/// 用本机 `sh -c` 跑 `script`，返回 stdout。sh 不可用（探针不过）或这次运行不正常
/// （见 [`sh_outcome`]）时打印“跳过 <test>：<原因>”并返回 `None`，调用方直接 `return`。
pub(crate) fn run_sh(test: &str, script: &str) -> Option<String> {
    if let Err(reason) = sh_probe() {
        println!("跳过 {test}：{reason}");
        return None;
    }
    match sh_outcome(run_sh_raw(script)) {
        Ok(stdout) => Some(stdout),
        Err(reason) => {
            println!("跳过 {test}：{reason}");
            None
        }
    }
}

#[cfg(test)]
mod sh_probe_tests {
    use super::*;
    use std::process::{ExitStatus, Output};

    fn output(code: u32, stdout: &str) -> std::io::Result<Output> {
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code)
        };
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw((code as i32) << 8)
        };
        Ok(Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn a_healthy_sh_run_is_usable() {
        assert_eq!(sh_outcome(output(0, "ok\n")).as_deref(), Ok("ok\n"));
    }

    #[test]
    fn a_missing_sh_is_skipped_with_the_reason() {
        let err = sh_outcome(Err(std::io::Error::from(std::io::ErrorKind::NotFound)))
            .expect_err("启动失败要跳过");
        assert!(err.contains("没有可用的 sh"), "{err}");
    }

    /// 本机 sh 能启动但立刻崩（退出码非 0、stdout 为空）：旧的跳过条件只看“能不能启动”，
    /// 这种情况会让断言失败。
    #[test]
    fn a_sh_that_starts_but_crashes_is_skipped() {
        let err = sh_outcome(output(0xC000_0022, "")).expect_err("非 0 退出码要跳过");
        assert!(err.contains("退出码非 0"), "{err}");
        let err = sh_outcome(output(1, "")).expect_err("非 0 退出码要跳过");
        assert!(err.contains("退出码非 0"), "{err}");
    }

    #[test]
    fn a_sh_that_exits_zero_but_prints_nothing_is_skipped() {
        let err = sh_outcome(output(0, "  \n")).expect_err("空输出要跳过");
        assert!(err.contains("没有输出"), "{err}");
    }
}
