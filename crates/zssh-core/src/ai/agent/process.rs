//! 智能体子进程的通用助手（AI-3）：跑进程树收尾、抓输出尾巴、按行排水。
//!
//! opencode 和 codex 两个驱动共用这些（原来在 `opencode.rs` 里，AI-3 抽出来）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::scrub;

/// 最近若干行输出（出错时一起报出去）。
pub type TailBuf = Arc<Mutex<Vec<String>>>;

/// 建一个尾巴缓冲。
pub fn tail_buf() -> TailBuf {
    Arc::new(Mutex::new(Vec::new()))
}

/// 把一行塞进尾巴缓冲（超出上限丢最老的）。
///
/// 进缓冲之前先脱敏、限长：缓冲里的文字之后会原样进错误提示和日志，智能体刚读到的
/// 私钥 / 令牌不能在那里露出来。私钥块跨很多行，是否还在块里靠“上一条是不是以
/// [`scrub::KEY_BLOCK_OPEN`] 结尾”判断，块内的行直接不记。
pub fn push_tail(tail: &TailBuf, line: &str, keep: usize) {
    if let Ok(mut buf) = tail.lock() {
        let in_block = buf
            .last()
            .is_some_and(|last| last.ends_with(scrub::KEY_BLOCK_OPEN));
        let scrubbed = scrub::scrub_stream_line(line, in_block);
        let Some(text) = scrubbed.text else {
            return;
        };
        buf.push(scrub::clip(&text, scrub::MAX_LINE_CHARS));
        if buf.len() > keep {
            let overflow = buf.len() - keep;
            buf.drain(..overflow);
        }
    }
}

/// 尾巴整段文字最多留这么多字符（留最后的，那里才是出错前最后一刻的输出）。
const MAX_TAIL_TEXT_CHARS: usize = 1500;

/// 把 stdout / stderr 的尾巴拼成一段可读文本，给错误提示和日志用：已脱敏、限长。
pub fn tail_text(stdout: &TailBuf, stderr: &TailBuf) -> String {
    let mut parts = Vec::new();
    for (label, buf) in [("stdout", stdout), ("stderr", stderr)] {
        if let Ok(tail) = buf.lock() {
            if !tail.is_empty() {
                // 缓冲里的行进来时已经脱敏过；这里再过一遍是兜底（有调用方直接改过缓冲）。
                let joined = scrub::scrub_line(&tail.join(" | "));
                parts.push(format!("，{label} 尾部：{}", clip_tail(&joined)));
            }
        }
    }
    parts.concat()
}

/// 只留最后 [`MAX_TAIL_TEXT_CHARS`] 个字符。
fn clip_tail(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_TAIL_TEXT_CHARS {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(total - MAX_TAIL_TEXT_CHARS).collect();
    format!("…{kept}")
}

/// 杀掉整个进程树（智能体底下挂着 zssh-mcp 等子进程）。
pub async fn kill_process_tree(child: &mut tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        // 已经退出了，收尸。
        let _ = child.wait().await;
        return Ok(());
    };
    let mut kill = tokio::process::Command::new("taskkill");
    kill.args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = crate::process_util::hide_console_async(&mut kill)
        .status()
        .await;
    let _ = child.wait().await;
    status.map(|_| ())
}

/// 一个"句柄关掉就结束里面全部进程"的 Job Object（K06）。
///
/// 为什么需要：正常收尾靠 [`kill_process_tree`] 跑 taskkill，但 Z-SSH 崩溃或被强制结束时
/// 根本走不到收尾，智能体和它底下的 zssh-mcp 会一直挂着。把子进程放进带
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的作业里，Z-SSH 进程一结束系统就关掉句柄，
/// 作业里的整棵树（绑进来之后再派生的子孙也自动在作业里）都会被结束。
#[cfg(windows)]
pub struct KillOnCloseJob(windows::Win32::Foundation::HANDLE);

// 作业句柄是内核对象句柄，可以在任意线程使用；只在 Drop 时关一次。
#[cfg(windows)]
unsafe impl Send for KillOnCloseJob {}
#[cfg(windows)]
unsafe impl Sync for KillOnCloseJob {}

#[cfg(windows)]
impl KillOnCloseJob {
    /// 建一个匿名作业并设好"句柄关闭即结束全部进程"。
    pub fn new() -> std::io::Result<Self> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: 参数都是本函数里的有效值；返回的句柄由 Self 持有，Drop 时关闭。
        let handle = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) }
            .map_err(std::io::Error::other)?;
        let job = Self(handle);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: info 是正确类型的结构体，长度按它自己的大小传。
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(std::io::Error::other)?;
        Ok(job)
    }

    /// 把子进程放进作业。子进程已经退出时返回错误，调用方忽略即可。
    pub fn assign(&self, child: &tokio::process::Child) -> std::io::Result<()> {
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        let Some(raw) = child.raw_handle() else {
            return Err(std::io::Error::other("子进程已经退出"));
        };
        // SAFETY: raw 是 tokio 持有的、仍然有效的进程句柄（child 还活着期间不会关）。
        unsafe { AssignProcessToJobObject(self.0, windows::Win32::Foundation::HANDLE(raw)) }
            .map_err(std::io::Error::other)
    }
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: 句柄由 new() 创建，只在这里关一次。
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// 把智能体子进程绑到 Z-SSH 自己的生命周期上：Z-SSH 退出（包括崩溃）时系统结束整棵树。
///
/// 全进程共用一个作业，存在 static 里永不释放——句柄要一直开到进程结束，由系统关闭。
/// 失败只记日志：绑不上不影响正常使用，正常收尾仍有 [`kill_process_tree`]。
/// 应该在 spawn 之后立刻调用：绑定之前子进程已经派生出的子孙不在作业里。
pub fn bind_to_app_lifetime(child: &tokio::process::Child) {
    #[cfg(windows)]
    {
        static JOB: std::sync::OnceLock<Option<KillOnCloseJob>> = std::sync::OnceLock::new();
        let job = JOB.get_or_init(|| match KillOnCloseJob::new() {
            Ok(job) => Some(job),
            Err(err) => {
                tracing::warn!(%err, "建子进程作业失败，崩溃时智能体进程可能残留");
                None
            }
        });
        if let Some(job) = job {
            if let Err(err) = job.assign(child) {
                tracing::warn!(%err, "子进程绑定作业失败，崩溃时它可能残留");
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = child;
    }
}

/// 进程还在不在：`Some(true)` 在，`Some(false)` 确定不在，`None` 判断不了（比如没权限）。
///
/// 下次启动清理残留文件时用：判断不了就当它还在，不删别人的东西。
pub fn process_is_alive(pid: u32) -> Option<bool> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: 只查询，句柄在本函数里关闭。
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => handle,
            // 没有这个 PID 的进程
            Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Some(false),
            Err(_) => return None,
        };
        let mut code = 0u32;
        // SAFETY: handle 有效，code 是本地变量。
        let queried = unsafe { GetExitCodeProcess(handle, &mut code) };
        // SAFETY: handle 由上面的 OpenProcess 打开，只关一次。
        let _ = unsafe { CloseHandle(handle) };
        queried.ok()?;
        Some(code == STILL_ACTIVE.0 as u32)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// 起一个任务把 reader 逐行读空、塞进尾巴缓冲。
///
/// `tag` 只进 trace 日志。trace 里只记每行的长度，不记内容：智能体的输出可能带着它
/// 刚读到的私钥 / 令牌（M-2）。返回的任务句柄丢着不管即可（进程退出时自然结束）。
pub fn drain_tail_lines<R>(
    reader: R,
    tail: TailBuf,
    tag: &'static str,
    keep: usize,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(reader);
        while let Ok(Some(line)) =
            crate::ai::lines::read_bounded_line(&mut reader, crate::ai::lines::MAX_AGENT_LINE_BYTES)
                .await
        {
            tracing::trace!(
                target: "agent",
                len = line.text.len(),
                truncated = line.truncated,
                "{tag}: 一行输出"
            );
            push_tail(&tail, &line.text, keep);
        }
    })
}

/// 空闲看护的节拍：把总时长切成 10 份，限幅在 20ms 到 5 秒之间。
pub fn idle_tick(idle_timeout: Duration) -> Duration {
    (idle_timeout / 10).clamp(Duration::from_millis(20), Duration::from_secs(5))
}

/// 尾巴缓冲默认留多少行。
pub const TAIL_LINES: usize = 40;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_only_the_newest_lines() {
        let tail = tail_buf();
        for i in 0..10 {
            push_tail(&tail, &format!("line-{i}"), 3);
        }
        let text = tail_text(&tail_buf(), &tail);
        assert!(text.contains("line-7") && text.contains("line-9"));
        assert!(!text.contains("line-6"), "超过上限的丢最老的：{text}");
    }

    // 假秘密拼出来：源码里不放完整的令牌 / 私钥头样式。
    fn fake_sk() -> String {
        format!("{}{}", "sk-", "Zq81xLmN0pQ4rStUvWx9YzAb")
    }

    /// 智能体 stderr 里先吐一段私钥块再吐令牌：错误提示用的尾巴和 trace 日志里都不能有原文。
    #[test]
    fn agent_output_secrets_reach_neither_the_tail_text_nor_the_trace_log() {
        use tokio::io::AsyncWriteExt;

        let body = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW";
        let begin = format!("{}{}", "-----BEGIN OPENSSH ", "PRIVATE KEY-----");
        let end = format!("{}{}", "-----END OPENSSH ", "PRIVATE KEY-----");
        let sk = fake_sk();
        let script = format!(
            "starting
{begin}
{body}
{body}
{end}
using {sk} now
password=hunter2hunter2
last words
"
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("运行时");
        let (text, logs) = crate::test_util::capture_logs(|| {
            runtime.block_on(async {
                let (mut writer, reader) = tokio::io::duplex(64 * 1024);
                let tail = tail_buf();
                let drain = drain_tail_lines(reader, Arc::clone(&tail), "测试 stderr", TAIL_LINES);
                writer.write_all(script.as_bytes()).await.expect("写");
                drop(writer);
                drain.await.expect("排水结束");
                tail_text(&tail_buf(), &tail)
            })
        });

        for secret in [body, sk.as_str(), "hunter2hunter2", "PRIVATE KEY"] {
            assert!(!text.contains(secret), "错误提示里出现了 {secret}：{text}");
            assert!(!logs.contains(secret), "日志里出现了 {secret}：{logs}");
        }
        assert!(
            text.contains("starting") && text.contains("last words"),
            "{text}"
        );
        assert!(
            logs.contains("测试 stderr"),
            "trace 日志没抓到（抓取失效）：{logs}"
        );
    }

    #[test]
    fn tail_text_is_clipped_to_a_bounded_length() {
        let tail = tail_buf();
        for _ in 0..40 {
            push_tail(&tail, &"x".repeat(1000), 40);
        }
        let text = tail_text(&tail_buf(), &tail);
        assert!(text.chars().count() < 1700, "{}", text.chars().count());
    }

    /// K06：作业句柄一关（相当于 Z-SSH 进程结束），里面的进程树要被系统结束，
    /// 包括绑定之后才派生出来的子进程。
    #[cfg(windows)]
    #[tokio::test]
    async fn closing_the_job_kills_the_bound_process_tree() {
        let dir = crate::test_util::TestDir::new("zssh job 测试");
        let marker = dir.join("still-alive.txt");
        // cmd 先等 2 秒再派生 ping，ping 再等 2 秒后写标记文件；整棵树被结束就不会写
        let script = format!(
            "ping -n 3 127.0.0.1 >nul & ping -n 3 127.0.0.1 >nul & echo alive> \"{}\"",
            marker.display()
        );
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/d")
            .arg("/c")
            .raw_arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let pid = child.id().unwrap();

        let job = KillOnCloseJob::new().unwrap();
        job.assign(&child).unwrap();
        assert_eq!(process_is_alive(pid), Some(true));

        tokio::time::sleep(Duration::from_millis(300)).await;
        let closed_at = std::time::Instant::now();
        drop(job);
        // 自己跑完至少要 4 秒；被作业结束的话几乎立刻退出（作业结束的退出码是 0，
        // 所以按耗时判断而不是看退出码）
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("关掉作业后子进程应该很快结束")
            .unwrap();
        assert!(
            closed_at.elapsed() < Duration::from_secs(2),
            "是被结束的，不是自己跑完的"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!marker.exists(), "派生出的子进程也该被结束");
    }

    /// UPD-1：退出时安装更新要求智能体的整棵进程树（含 zssh-mcp 这类孙子进程）在
    /// `kill_process_tree` 返回时已经结束，否则安装程序覆盖文件可能被占用。
    #[cfg(windows)]
    #[tokio::test]
    async fn kill_process_tree_ends_grandchildren_before_returning() {
        let dir = crate::test_util::TestDir::new("zssh kill tree 测试");
        let marker = dir.join("grandchild-alive.txt");
        // cmd 先等 2 秒再派生出的 ping 又等 2 秒，之后写标记；整棵树被杀就不会写
        let script = format!(
            "ping -n 3 127.0.0.1 >nul & ping -n 3 127.0.0.1 >nul & echo alive> \"{}\"",
            marker.display()
        );
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/d")
            .arg("/c")
            .raw_arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let pid = child.id().unwrap();
        // 等孙子进程（ping）派生出来
        tokio::time::sleep(Duration::from_millis(500)).await;
        kill_process_tree(&mut child).await.unwrap();
        assert_eq!(process_is_alive(pid), Some(false), "根进程要已经结束");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!marker.exists(), "孙子进程也该被结束");
    }

    #[cfg(windows)]
    #[test]
    fn process_is_alive_knows_itself() {
        assert_eq!(process_is_alive(std::process::id()), Some(true));
    }

    #[test]
    fn idle_tick_is_bounded() {
        assert_eq!(idle_tick(Duration::from_secs(600)), Duration::from_secs(5));
        assert_eq!(
            idle_tick(Duration::from_millis(100)),
            Duration::from_millis(20)
        );
        assert_eq!(idle_tick(Duration::from_secs(60)), Duration::from_secs(5));
    }
}
