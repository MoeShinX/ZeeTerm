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
#[cfg(windows)]
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

/// 杀掉整个进程树（智能体底下挂着 zssh-mcp 等子进程）。
///
/// Unix 上智能体启动时就放进了独立的进程组（[`isolate_process_group`]，组号 = 子进程号），
/// 所以这里对整组发信号：先 `SIGTERM` 让它们体面收尾，等 [`TERM_GRACE`]，组里还有人
/// 就 `SIGKILL`。返回时直接子进程已经收尸；孙子进程被 `SIGKILL` 之后由内核结束，最多再等
/// [`GROUP_GONE_WAIT`] 让它们被收走（容器里没有 init 收尸时僵尸会一直留着，所以不无限等）。
#[cfg(unix)]
pub async fn kill_process_tree(child: &mut tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        // 已经退出了，收尸。
        let _ = child.wait().await;
        return Ok(());
    };
    let Some(pgid) = own_group_id(pid) else {
        // 不是独立进程组的组长（没走 build_command 启动的）：只能杀这一个进程，不能误伤别的组。
        let _ = child.start_kill();
        return child.wait().await.map(|_| ());
    };

    let _ = signal_group(pgid, libc::SIGTERM);
    wait_until_group_gone(child, pgid, TERM_GRACE).await;
    if group_exists(pgid) {
        let _ = signal_group(pgid, libc::SIGKILL);
    }
    let waited = child.wait().await;
    wait_until_group_gone(child, pgid, GROUP_GONE_WAIT).await;
    waited.map(|_| ())
}

/// `SIGTERM` 之后给整组多久自己收尾，超过就 `SIGKILL`。
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_millis(1500);

/// `SIGKILL` 之后最多再等多久让整组被收走。
#[cfg(unix)]
const GROUP_GONE_WAIT: Duration = Duration::from_millis(1000);

/// 轮询到整个进程组都没人了（或超时）。顺便给直接子进程收尸：僵尸也算组员，不收的话组永远"在"。
#[cfg(unix)]
async fn wait_until_group_gone(
    child: &mut tokio::process::Child,
    pgid: libc::pid_t,
    limit: Duration,
) {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let _ = child.try_wait();
        if !group_exists(pgid) || tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 子进程号对应的进程组号；只有"确实是自己组长"的才返回（pid > 1 且不是本进程所在的组）。
///
/// 为什么要查：`killpg(1, ..)` 或本进程自己的组会误杀不该杀的东西。
#[cfg(unix)]
fn own_group_id(pid: u32) -> Option<libc::pid_t> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    if pid <= 1 {
        return None;
    }
    // SAFETY: getpgid / getpgrp 只读取进程组信息，没有内存副作用。
    let (child_group, own_group) = unsafe { (libc::getpgid(pid), libc::getpgrp()) };
    (child_group == pid && child_group != own_group).then_some(pid)
}

/// 给进程组发信号。组已经没有成员（`ESRCH`）不算错。
#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    // SAFETY: killpg 只发信号；调用方保证 pgid 通过了 `own_group_id` 的检查。
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

/// 进程组里还有没有成员（信号 0 只做检查，不发信号）。
#[cfg(unix)]
fn group_exists(pgid: libc::pid_t) -> bool {
    // SAFETY: 信号 0 不会真的发信号。
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    // EPERM = 组在，只是不归我们管：当它还在。
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `Drop` 里的同步兜底：马上结束整棵树，不等、不体面收尾（等同于 Windows 的 `taskkill /T /F`）。
///
/// 没走 `shutdown` 就丢了句柄时才会到这里；正常收尾走 [`kill_process_tree`]。
pub fn kill_tree_now(pid: u32) {
    #[cfg(windows)]
    {
        let mut kill = std::process::Command::new("taskkill");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = crate::process_util::hide_console(&mut kill).status();
    }
    #[cfg(unix)]
    {
        if let Some(pgid) = own_group_id(pid) {
            let _ = signal_group(pgid, libc::SIGKILL);
        }
    }
}

/// 让智能体子进程在 Unix 上自成一个进程组，Linux 上再保证 ZeeTerm 被强杀时它跟着走。
///
/// - `process_group(0)`：组号 = 子进程号，[`kill_process_tree`] 才能一次结束整棵树
///   （含它派生的 MCP 助手等孙子进程）；
/// - Linux 的 `PR_SET_PDEATHSIG(SIGKILL)`：父进程没了内核就杀它。注意这个信号跟的是**创建
///   子进程的那个线程**，不是整个进程——线程退出也会触发。这里够用，因为智能体都在应用常驻的
///   tokio 运行时的工作线程上启动，工作线程和运行时同寿；如果有人在会提前退出的线程上（比如
///   `spawn_blocking` 池里的）启动，子进程会跟着那个线程一起被杀。
///   只杀直接子进程；它派生的孙子进程靠正常收尾的 `killpg` 清理，崩溃时则靠它们自己的
///   标准输入关闭（MCP 助手读到 EOF 就退出）。
#[cfg(unix)]
pub fn isolate_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    {
        // SAFETY: pre_exec 闭包在 fork 之后、exec 之前运行，里面只调用异步信号安全的
        // prctl / getppid，不分配内存、不加锁。
        let expected_parent = std::process::id() as libc::pid_t;
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(
                    libc::PR_SET_PDEATHSIG,
                    libc::SIGKILL as libc::c_ulong,
                    0,
                    0,
                    0,
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                // 设置信号之前父进程就没了的话信号不会补发：自己检查，别留孤儿。
                if libc::getppid() != expected_parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
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
    // Unix 上这件事在启动时就做了（见 `isolate_process_group` 的 PDEATHSIG），这里没有可做的。
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
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid).ok()?;
        // SAFETY: 信号 0 只检查进程存不存在，不发信号。
        if unsafe { libc::kill(pid, 0) } != 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => Some(false),
                _ => None,
            };
        }
        // 已经退出但还没被收尸的僵尸，`kill(pid, 0)` 也成功：按"不在了"算，
        // 和 Windows 上"退出码不是 STILL_ACTIVE"一致（Linux 看 /proc 里的状态位）。
        #[cfg(target_os = "linux")]
        if is_zombie(pid) {
            return Some(false);
        }
        Some(true)
    }
}

/// `/proc/<pid>/stat` 里的状态位是不是 `Z`（僵尸）。读不出来按"不是"算。
#[cfg(target_os = "linux")]
fn is_zombie(pid: libc::pid_t) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // 格式：`pid (comm) S ...`，comm 里可能有空格和括号，所以从最后一个 `)` 往后找状态位。
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.trim_start().chars().next())
        == Some('Z')
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

    /// 启动一个跑 `script` 的 `sh`，放进独立进程组（和产品里 `build_command` 的做法一致）。
    #[cfg(unix)]
    fn spawn_isolated_sh(script: &str) -> tokio::process::Child {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        isolate_process_group(&mut cmd);
        cmd.spawn().expect("起 sh")
    }

    /// 等文件里出现一个进程号（脚本先把孙子进程号写进去）。
    #[cfg(unix)]
    async fn read_pid_file(path: &std::path::Path) -> u32 {
        for _ in 0..200 {
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

    /// 对应 Windows 的同名测试：`kill_process_tree` 返回时孙子进程也已经结束。
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_ends_grandchildren_before_returning_unix() {
        let dir = crate::test_util::TestDir::new("zssh kill tree 测试");
        let pid_file = dir.join("grandchild.pid");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let mut child = spawn_isolated_sh(&script);
        let pid = child.id().expect("pid");
        let grandchild = read_pid_file(&pid_file).await;
        assert_eq!(process_is_alive(pid), Some(true));
        assert_eq!(process_is_alive(grandchild), Some(true));

        let started = std::time::Instant::now();
        kill_process_tree(&mut child).await.expect("杀进程树");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "只用 SIGTERM 就能收尾的，不该等满宽限期：{:?}",
            started.elapsed()
        );
        assert_eq!(process_is_alive(pid), Some(false), "根进程要已经结束");
        assert_eq!(
            process_is_alive(grandchild),
            Some(false),
            "孙子进程也该被结束"
        );
    }

    /// 忽略 SIGTERM 的整棵树要被 SIGKILL 补刀，而且是等满宽限期之后。
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_escalates_to_sigkill_when_sigterm_is_ignored() {
        let dir = crate::test_util::TestDir::new("zssh kill 忽略 term");
        let pid_file = dir.join("grandchild.pid");
        // 忽略 TERM 的设置会被 exec 出来的 sleep 继承，孙子也不会死于 SIGTERM。
        let script = format!(
            "trap '' TERM; sleep 60 & echo $! > '{}'; while :; do sleep 1; done",
            pid_file.display()
        );
        let mut child = spawn_isolated_sh(&script);
        let pid = child.id().expect("pid");
        let grandchild = read_pid_file(&pid_file).await;

        let started = std::time::Instant::now();
        kill_process_tree(&mut child).await.expect("杀进程树");
        assert!(
            started.elapsed() >= TERM_GRACE,
            "SIGTERM 被忽略时应该等满宽限期再 SIGKILL：{:?}",
            started.elapsed()
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(process_is_alive(pid), Some(false));
        assert_eq!(process_is_alive(grandchild), Some(false));
    }

    /// 不是自己进程组组长的子进程（没走 `isolate_process_group`）：只杀它自己，绝不 killpg 别的组。
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_only_kills_the_child_when_it_has_no_group_of_its_own() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("起 sleep");
        let pid = child.id().expect("pid");
        assert!(own_group_id(pid).is_none(), "没独立成组，不该被当成组长");
        kill_process_tree(&mut child).await.expect("杀进程");
        assert_eq!(process_is_alive(pid), Some(false));
        // 测试进程自己当然还在。
        assert_eq!(process_is_alive(std::process::id()), Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn group_ids_that_could_hurt_bystanders_are_refused() {
        assert!(own_group_id(0).is_none());
        assert!(own_group_id(1).is_none());
        // 本进程自己所在的组（不管本进程是不是组长）永远不能 killpg。
        assert!(own_group_id(std::process::id()).is_none());
        assert!(own_group_id(u32::MAX).is_none());
        // 同步兜底对这些也不动手（动手了测试进程就没了）。
        kill_tree_now(0);
        kill_tree_now(1);
        kill_tree_now(std::process::id());
    }

    /// `Drop` 里的同步兜底：马上结束整组，包括孙子进程。
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_tree_now_ends_the_whole_group() {
        let dir = crate::test_util::TestDir::new("zssh kill now");
        let pid_file = dir.join("grandchild.pid");
        let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
        let mut child = spawn_isolated_sh(&script);
        let pid = child.id().expect("pid");
        let grandchild = read_pid_file(&pid_file).await;
        kill_tree_now(pid);
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("根进程应该很快结束")
            .expect("收尸");
        for _ in 0..100 {
            if process_is_alive(grandchild) == Some(false) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("孙子进程没有被结束");
    }

    #[cfg(unix)]
    #[test]
    fn process_is_alive_knows_itself_and_missing_pids() {
        assert_eq!(process_is_alive(std::process::id()), Some(true));
        // 超出 pid_max 的号一定不存在。
        assert_eq!(process_is_alive(0x7fff_fff0), Some(false));
    }

    /// 被 `PR_SET_PDEATHSIG` 保护的子进程，在 ZeeTerm 被强杀（SIGKILL，走不到任何收尾代码）后也要消失。
    ///
    /// 下面 `pdeathsig_parent_helper` 在自己的进程里扮演"ZeeTerm"：用产品的
    /// `isolate_process_group` 起一个子进程、把子进程号写进文件，然后等着被杀。
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_killed_parent_takes_its_isolated_child_with_it() {
        const HELPER: &str = "ai::agent::process::tests::pdeathsig_parent_helper";
        let dir = crate::test_util::TestDir::new("zssh pdeathsig 测试");
        let pid_file = dir.join("child.pid");
        let exe = std::env::current_exe().expect("测试程序路径");
        let mut parent = tokio::process::Command::new(exe)
            .args(["--exact", HELPER, "--nocapture", "--test-threads=1"])
            .env("ZT_PDEATHSIG_CHILD_PID_FILE", &pid_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("起扮演 ZeeTerm 的进程");
        let child_pid = read_pid_file(&pid_file).await;
        assert_eq!(process_is_alive(child_pid), Some(true));

        parent.kill().await.expect("强杀扮演 ZeeTerm 的进程");
        for _ in 0..200 {
            if process_is_alive(child_pid) == Some(false) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("父进程被强杀后，子进程 {child_pid} 还活着");
    }

    /// 只有被上面那条测试用环境变量拉起时才干活；平时一跑就过。
    #[cfg(target_os = "linux")]
    #[test]
    fn pdeathsig_parent_helper() {
        let Some(pid_file) = std::env::var_os("ZT_PDEATHSIG_CHILD_PID_FILE") else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("运行时");
        runtime.block_on(async {
            let mut cmd = tokio::process::Command::new("sleep");
            cmd.arg("120")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            isolate_process_group(&mut cmd);
            let child = cmd.spawn().expect("起 sleep");
            let pid = child.id().expect("pid");
            // 先写临时文件再改名，读的一方不会读到半截。
            let tmp = std::path::PathBuf::from(&pid_file).with_extension("tmp");
            std::fs::write(&tmp, pid.to_string()).expect("写进程号");
            std::fs::rename(&tmp, &pid_file).expect("改名");
            tokio::time::sleep(Duration::from_secs(120)).await;
            drop(child);
        });
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
