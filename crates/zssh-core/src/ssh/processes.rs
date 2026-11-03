//! 服务器上的进程列表与结束进程（V2-U18，概览面板「进程」一节的「进程管理」）。
//!
//! 套路同 [`super::connections`]：在已有连接上开一个 exec 通道（不新建 SSH 连接、不往终端里打字），
//! 跑一段**只读**脚本；解析、筛选、排序全是纯函数，界面只做展示。
//!
//! 采集：强制 `LC_ALL=C` 之后按顺序试几种 `ps`（procps 带 `etimes` → 老 procps 只有 `etime` → BusyBox / BSD
//! 的 `-o` → BusyBox 最简的 `ps w`），都不行再读 `/proc/*/stat`。每一种都先打一行 `@@ZSSH source …`，
//! 失败就退到下一种，解析器取**最后一段**，并且**按表头认列**（不同的 ps 列数、顺序不同）。
//!
//! 结束进程（写操作）：只有用户在界面上确认之后才调用 [`terminate_process`]，它
//! 1. 重新采一次，核对这个 PID 还是用户看到的那个进程（PID 可能在列表过时之后被别的进程复用），
//! 2. 然后 exec `kill -TERM <pid>` / `kill -KILL <pid>`（PID 是 `u32`，经 [`validate_pid`] 校验，命令里
//!    只会出现数字，不拼接任何别的字符串），
//! 3. 没权限就如实返回 [`KillError::NoPermission`]，**绝不自动用 sudo**。
//!
//! 这个动作不暴露给 AI 工具桥（本模块没有被 `ai` 引用）。

use std::cmp::Ordering;
use std::time::Duration;

use super::connection::Connection;
use super::cwd::shell_quote;

/// 单次采集的时限。
pub const PROCESSES_TIMEOUT: Duration = Duration::from_secs(5);
/// 脚本输出的字节上限（超过只取前面的整行）。
pub const PROCESSES_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 最多解析这么多个进程，超过的丢掉并标记 `truncated`。
pub const PROCESSES_MAX_ROWS: usize = 10_000;
/// `kill` 的时限。
pub const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// Linux 的 `pid_max` 上限（2^22）：再大的数字不可能是 PID。
pub const PID_MAX: u32 = 4_194_304;

/// 服务器上跑的脚本。
///
/// `try` 先打来源标记再跑命令，返回命令自己的退出码；`a || b || c` 串起各种 ps，前一个整个失败（不认选项）
/// 才退到下一个，失败时 stdout 是空的，所以多出来的标记不会混进数据。procps 路径用 `-ww` 不截断命令行。
const SCRIPT: &str = r#"LC_ALL=C
export LC_ALL
echo "@@ZSSH uid $(id -u 2>/dev/null)"
echo "@@ZSSH user $(id -un 2>/dev/null)"
echo "@@ZSSH self $$"
echo "@@ZSSH memtotal $(awk '/^MemTotal:/{print $2; exit}' /proc/meminfo 2>/dev/null)"
try() {
  echo "@@ZSSH source $1"
  shift
  "$@" 2>/dev/null
}
procfs() {
  hz=$(getconf CLK_TCK 2>/dev/null) || hz=100
  pg=$(getconf PAGESIZE 2>/dev/null) || pg=4096
  echo "PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND"
  awk -v hz="${hz:-100}" -v pg="${pg:-4096}" -v memkb="$(awk '/^MemTotal:/{print $2; exit}' /proc/meminfo 2>/dev/null)" '
    BEGIN {
      getline up < "/proc/uptime"; split(up, u, " "); uptime = u[1] + 0
      while ((getline line < "/etc/passwd") > 0) { split(line, p, ":"); name[p[3]] = p[1] }
    }
    FILENAME ~ "status$" { if ($1 == "Uid:") { k = split(FILENAME, a, "/"); uid[a[k - 1]] = $2 } next }
    {
      line = $0
      i = index(line, "(")
      j = length(line)
      while (j > i && substr(line, j, 1) != ")") j--
      if (i < 2 || j <= i) next
      pid = substr(line, 1, i - 2)
      comm = substr(line, i + 1, j - i - 1)
      n = split(substr(line, j + 2), f, " ")
      if (n < 22) next
      el = uptime - f[20] / hz
      if (el < 1) el = 1
      cpu = (f[12] + f[13]) / hz / el * 100
      rss = f[22] * pg / 1024
      mem = memkb > 0 ? rss * 100 / memkb : 0
      user = (pid in uid) ? ((uid[pid] in name) ? name[uid[pid]] : uid[pid]) : "-"
      gsub(/[ \t]/, "_", user)
      printf "%s %s %s %.1f %.1f %d %s %d %s\n", pid, f[2], user, cpu, mem, rss, f[1], el, comm
    }' /proc/[0-9]*/status /proc/[0-9]*/stat
}
try procps ps -ww -eo pid,ppid,user:32,pcpu,pmem,rss,stat,etimes,args --sort=-pcpu \
  || try procps ps -ww -eo pid,ppid,user:32,pcpu,pmem,rss,stat,etime,args --sort=-pcpu \
  || try procps ps -eo pid,ppid,user,pcpu,pmem,rss,stat,etime,args --sort=-pcpu \
  || try bb ps -o pid,ppid,user,stat,rss,etime,args \
  || try bb ps -o pid,ppid,user,stat,rss,args \
  || try bb ps w \
  || try proc procfs
exit 0"#;

/// 交给 exec 通道的命令行：外层 `exec sh -c`，登录 shell 是 fish / csh 也照样是 POSIX sh。
pub fn processes_command() -> String {
    format!("exec sh -c {}", shell_quote(SCRIPT))
}

/// 这份列表是从哪来的（界面上的「来源」）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcSource {
    /// procps 的 `ps`：有 CPU% / 内存%。
    Procps,
    /// BusyBox / BSD 的 `ps -o`：没有 CPU%，内存% 用 RSS 除以总内存算。
    PsBasic,
    /// BusyBox 最简的 `ps w`：只有 PID / 用户 / 状态 / 命令。
    PsMinimal,
    /// 读 `/proc/<pid>/stat`：命令只有进程名，没有完整命令行。
    Proc,
}

impl ProcSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Procps => "ps",
            Self::PsBasic => "ps（精简）",
            Self::PsMinimal => "ps（最简）",
            Self::Proc => "/proc",
        }
    }
}

/// 一个进程。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub user: String,
    /// CPU 占比（%）。`ps` 给的是启动以来的平均值；这一档的来源拿不到时是 `None`。
    pub cpu: Option<f64>,
    /// 内存占比（%）。
    pub mem: Option<f64>,
    /// 常驻内存，KiB。
    pub rss_kb: Option<u64>,
    /// 状态码原样（`Ssl`、`R`、`Z`……）。
    pub state: String,
    /// 已运行的秒数。
    pub elapsed_secs: Option<u64>,
    /// 命令行（procps 是完整的 `args`；`/proc` 来源只有进程名）。
    pub command: String,
}

impl ProcRow {
    /// 僵尸进程：已经结束，只等父进程回收。
    pub fn is_zombie(&self) -> bool {
        self.state.starts_with('Z')
    }
}

/// 一次采集的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessReport {
    pub source: ProcSource,
    pub rows: Vec<ProcRow>,
    /// 进程太多：只留了前 [`PROCESSES_MAX_ROWS`] 个（或输出超过字节上限被截了）。
    pub truncated: bool,
    /// 当前登录用户的 uid；`0` = root。
    pub uid: Option<u32>,
    /// 当前登录用户的名字。
    pub user: Option<String>,
    /// 采集脚本自己那个 shell 的 pid（它和它的子进程不会列出来）。
    pub self_pid: Option<u32>,
    /// 采集脚本的 shell 的父进程（承载这条 SSH 连接的 sshd 一类进程），识别"本次连接"用。
    pub self_parent_pid: Option<u32>,
}

/// 结束一个进程有多危险（确认框特别警示用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcRisk {
    /// 普通进程。
    Normal,
    /// PID 1：系统的初始化进程。
    Init,
    /// 承载本次 SSH 连接的进程（采集用的 exec 通道所在 sshd 及它的祖先）：结束它会断开现在的连接。
    SshSession,
}

/// 为什么一个进程不能结束（界面在行上画锁、悬停写原因，按钮置灰）。**没有解锁途径**：受保护的进程一律不能在
/// 界面上结束（用户定的），`terminate_process` 在核心里也再拦一次。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// PID 1（init / systemd）。
    Init,
    /// 内核线程：不是真正的用户进程，结束没有意义。
    KernelThread,
    /// 承载本次 SSH 会话的进程链（sshd 会话进程、它的 shell、采集用的进程）：结束它们就是把自己踢下线。
    SshSession,
    /// sshd 主进程：结束它会断开所有 SSH 连接、之后没法再登录。
    SshDaemon,
    /// 关键系统服务（见 [`CRITICAL_SERVICES`]）。
    CriticalService,
}

impl Protection {
    /// 悬停说明里的原因。
    pub fn reason(self) -> &'static str {
        match self {
            Self::Init => "系统初始化进程",
            Self::KernelThread => "内核线程",
            Self::SshSession => "当前 SSH 会话",
            Self::SshDaemon => "结束会断开所有 SSH 连接",
            Self::CriticalService => "关键系统服务",
        }
    }
}

/// 关键系统服务：按**进程名精确匹配**（命令行第一个词去掉目录和结尾的冒号，大小写敏感）。
///
/// 理由：这些进程一旦结束，服务器要么整体不可用、要么失去登录 / 日志 / 网络 / 容器：
/// - `systemd` / `init` / `udevd` / `systemd-udevd`：初始化与设备管理；
/// - `systemd-journald` / `rsyslogd` / `auditd`：日志与审计（丢日志、审计被绕过）；
/// - `systemd-logind` / `agetty` / `polkitd`：登录会话、控制台登录、授权；
/// - `systemd-networkd` / `systemd-resolved` / `NetworkManager`：网络与 DNS，结束后可能连 SSH 都断；
/// - `dbus-daemon` / `dbus-broker`：系统总线，很多服务依赖它；
/// - `cron` / `crond`：定时任务；
/// - `containerd` / `dockerd`：结束会停掉所有容器；`kubelet`：结束会让节点上的 Pod 失控。
pub const CRITICAL_SERVICES: &[&str] = &[
    "systemd",
    "systemd-journald",
    "systemd-logind",
    "systemd-udevd",
    "systemd-networkd",
    "systemd-resolved",
    "dbus-daemon",
    "dbus-broker",
    "init",
    "udevd",
    "containerd",
    "dockerd",
    "kubelet",
    "NetworkManager",
    "polkitd",
    "agetty",
    "cron",
    "crond",
    "rsyslogd",
    "auditd",
];

/// 命令行第一个词的进程名：去掉目录和结尾的 `:`（`sshd: user@pts/0` → `sshd`，`/usr/sbin/cron` → `cron`）。
fn command_name(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    base.trim_end_matches(':')
}

fn is_sshd(row: &ProcRow) -> bool {
    let name = command_name(&row.command);
    name == "sshd" || name.starts_with("sshd-") || name == "dropbear"
}

/// 内核线程：PID 2、父进程是 2、命令是 ps 的 `[名字]` 写法、或者命令行是空的（`/proc/<pid>/cmdline` 为空）。
fn is_kernel_thread(row: &ProcRow) -> bool {
    let command = row.command.trim();
    row.pid == 2
        || row.ppid == Some(2)
        || command.is_empty()
        || (command.starts_with('[') && command.ends_with(']'))
}

impl ProcessReport {
    /// 这个进程受不受保护、为什么。判定顺序：PID 1 → 内核线程 → 当前会话链 → sshd → 关键服务。
    ///
    /// 会话链识别不出来（旧输出没有 `self`）时退一步：所有 sshd / dropbear 进程都按"当前会话"保护，宁可多锁。
    pub fn protection(&self, pid: u32) -> Option<Protection> {
        let row = self.row(pid)?;
        if row.pid == 1 {
            return Some(Protection::Init);
        }
        if is_kernel_thread(row) {
            return Some(Protection::KernelThread);
        }
        let sshd = is_sshd(row);
        if self.session_chain().contains(&pid) || (sshd && self.self_parent_pid.is_none()) {
            return Some(Protection::SshSession);
        }
        // sshd 主进程（监听者）：父进程是 PID 1，或者名字里带 `[listener]`。别的用户的会话进程不在此列。
        if sshd && (row.ppid == Some(1) || row.command.contains("[listener]")) {
            return Some(Protection::SshDaemon);
        }
        CRITICAL_SERVICES
            .contains(&command_name(&row.command))
            .then_some(Protection::CriticalService)
    }

    pub fn cpu_available(&self) -> bool {
        self.rows.iter().any(|row| row.cpu.is_some())
    }

    pub fn mem_available(&self) -> bool {
        self.rows.iter().any(|row| row.mem.is_some())
    }

    pub fn row(&self, pid: u32) -> Option<&ProcRow> {
        self.rows.iter().find(|row| row.pid == pid)
    }

    /// 承载本次连接的进程：采集脚本的 shell 往上数的祖先（不含 PID 1）。
    ///
    /// 只能识别 exec 通道这一条链（OpenSSH 的 `sshd: 用户 [priv]` → 每个会话一个 `sshd: 用户@notty`）。同一条连接上
    /// 终端里的 shell 是它的兄弟进程，这里认不出来——结束它同样会让终端掉线，确认框里照样写清楚命令行和用户。
    pub fn session_chain(&self) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut next = self.self_parent_pid;
        // 链不可能很长；限制步数防止 ppid 成环。
        for _ in 0..16 {
            let Some(pid) = next else { break };
            if pid <= 1 || chain.contains(&pid) {
                break;
            }
            chain.push(pid);
            next = self.row(pid).and_then(|row| row.ppid);
        }
        chain
    }

    pub fn risk_of(&self, pid: u32) -> ProcRisk {
        if pid == 1 {
            ProcRisk::Init
        } else if self.session_chain().contains(&pid) {
            ProcRisk::SshSession
        } else {
            ProcRisk::Normal
        }
    }
}

/// 采集失败的原因（给界面直接显示）。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ProcessesError {
    #[error("这台服务器上读不到进程列表（没有可用的 ps，也读不到 /proc），可能不是 Linux。")]
    Unsupported,
    #[error("采集超时（5 秒没有结果），可以稍后再点刷新。")]
    Timeout,
    #[error("采集失败：{0}")]
    Exec(String),
}

// ---- 解析 ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    Pid,
    Ppid,
    User,
    Cpu,
    Mem,
    Rss,
    Stat,
    Elapsed,
    Command,
    Other,
}

fn col_of(name: &str) -> Col {
    match name.to_ascii_uppercase().as_str() {
        "PID" => Col::Pid,
        "PPID" => Col::Ppid,
        "USER" | "RUSER" => Col::User,
        "%CPU" | "CPU" | "PCPU" => Col::Cpu,
        "%MEM" | "MEM" | "PMEM" => Col::Mem,
        "RSS" | "RSZ" => Col::Rss,
        "STAT" | "S" | "STATE" => Col::Stat,
        "ELAPSED" | "ETIME" | "ETIMES" => Col::Elapsed,
        "COMMAND" | "CMD" | "ARGS" => Col::Command,
        _ => Col::Other,
    }
}

/// 表头：第一个词是 `PID`，并且有命令列（命令列必须是最后一列：它可以带空格）。
fn parse_header(line: &str) -> Option<Vec<Col>> {
    let cols: Vec<Col> = line.split_whitespace().map(col_of).collect();
    let last_is_command = cols.last() == Some(&Col::Command);
    (cols.first() == Some(&Col::Pid) && last_is_command).then_some(cols)
}

/// 取前 `count - 1` 个以空白分隔的词，剩下的（去掉首尾空白，可以是空）是最后一列。
fn split_columns(line: &str, count: usize) -> Option<(Vec<&str>, &str)> {
    let mut rest = line.trim_start();
    let mut tokens = Vec::with_capacity(count);
    for _ in 0..count.saturating_sub(1) {
        if rest.is_empty() {
            return None;
        }
        match rest.find(char::is_whitespace) {
            Some(end) => {
                tokens.push(&rest[..end]);
                rest = rest[end..].trim_start();
            }
            None => {
                tokens.push(rest);
                rest = "";
            }
        }
    }
    Some((tokens, rest.trim_end()))
}

/// `ps` 的运行时间：`etimes` 是纯秒数；`etime` 是 `[[DD-]HH:]MM:SS`。
fn parse_elapsed(text: &str) -> Option<u64> {
    if text.chars().all(|c| c.is_ascii_digit()) && !text.is_empty() {
        return text.parse().ok();
    }
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut secs = 0u64;
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.len() > 3 || parts.len() < 2 {
        return None;
    }
    for part in parts {
        secs = secs
            .checked_mul(60)?
            .checked_add(part.parse::<u64>().ok()?)?;
    }
    days.checked_mul(86_400)?.checked_add(secs)
}

fn parse_percent(text: &str) -> Option<f64> {
    let value: f64 = text.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// 解析脚本的输出。
///
/// `@@ZSSH source X` 开一个来源段，退到下一种 ps 时会再来一段，**取最后一段**（前一段的数据丢掉）。
pub fn parse_processes(output: &str) -> Result<ProcessReport, ProcessesError> {
    let mut uid: Option<u32> = None;
    let mut user: Option<String> = None;
    let mut self_pid: Option<u32> = None;
    let mut memtotal_kb: Option<u64> = None;
    let mut source: Option<ProcSource> = None;
    let mut header: Option<Vec<Col>> = None;
    let mut rows: Vec<ProcRow> = Vec::new();
    let mut truncated = false;

    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(directive) = line.strip_prefix("@@ZSSH ") {
            let mut words = directive.split_whitespace();
            match (words.next(), words.next()) {
                (Some("uid"), value) => uid = value.and_then(|v| v.parse().ok()),
                (Some("user"), value) => user = value.map(str::to_owned),
                (Some("self"), value) => self_pid = value.and_then(|v| v.parse().ok()),
                (Some("memtotal"), value) => memtotal_kb = value.and_then(|v| v.parse().ok()),
                (Some("source"), Some(name)) => {
                    source = match name {
                        "procps" => Some(ProcSource::Procps),
                        "bb" => Some(ProcSource::PsBasic),
                        "proc" => Some(ProcSource::Proc),
                        _ => source,
                    };
                    rows.clear();
                    truncated = false;
                    header = None;
                }
                _ => {}
            }
            continue;
        }
        if source.is_none() || line.trim().is_empty() {
            continue;
        }
        let Some(cols) = &header else {
            header = parse_header(line);
            continue;
        };
        if let Some(row) = parse_row(line, cols, memtotal_kb) {
            if rows.len() >= PROCESSES_MAX_ROWS {
                truncated = true;
            } else {
                rows.push(row);
            }
        }
    }

    let mut source = source.ok_or(ProcessesError::Unsupported)?;
    if header.is_none() {
        return Err(ProcessesError::Unsupported);
    }
    // 「最简」ps：表头里没有 RSS / 状态以外的数字列，单独标出来，界面上不再装作有内存。
    if source == ProcSource::PsBasic && !rows.iter().any(|row| row.rss_kb.is_some()) {
        source = ProcSource::PsMinimal;
    }
    // 采集脚本自己（它的 shell 和 ps / awk 之类的子进程）不是用户想看的进程。
    let mut self_parent_pid = None;
    if let Some(me) = self_pid {
        self_parent_pid = rows
            .iter()
            .find(|row| row.pid == me)
            .and_then(|row| row.ppid);
        rows.retain(|row| row.pid != me && row.ppid != Some(me));
    }
    Ok(ProcessReport {
        source,
        rows,
        truncated,
        uid,
        user,
        self_pid,
        self_parent_pid,
    })
}

fn parse_row(line: &str, cols: &[Col], memtotal_kb: Option<u64>) -> Option<ProcRow> {
    let (tokens, command) = split_columns(line, cols.len())?;
    let mut row = ProcRow {
        pid: 0,
        ppid: None,
        user: String::new(),
        cpu: None,
        mem: None,
        rss_kb: None,
        state: String::new(),
        elapsed_secs: None,
        command: command.to_owned(),
    };
    let mut pid_seen = false;
    for (col, token) in cols.iter().zip(tokens) {
        match col {
            Col::Pid => {
                row.pid = token.parse().ok()?;
                pid_seen = true;
            }
            Col::Ppid => row.ppid = token.parse().ok(),
            Col::User => row.user = token.to_owned(),
            Col::Cpu => row.cpu = parse_percent(token),
            Col::Mem => row.mem = parse_percent(token),
            Col::Rss => row.rss_kb = token.parse().ok(),
            Col::Stat => row.state = token.to_owned(),
            Col::Elapsed => row.elapsed_secs = parse_elapsed(token),
            Col::Command | Col::Other => {}
        }
    }
    if !pid_seen || row.pid == 0 {
        return None;
    }
    // 没有内存百分比、但有 RSS 和总内存（BusyBox）：自己算。
    if row.mem.is_none() {
        if let (Some(rss), Some(total)) = (row.rss_kb, memtotal_kb.filter(|t| *t > 0)) {
            row.mem = Some(rss as f64 * 100.0 / total as f64);
        }
    }
    Some(row)
}

// ---- 采集 ----

/// 在**已有的连接**上采集一次（不重新连接）。
pub async fn collect_processes(connection: &Connection) -> Result<ProcessReport, ProcessesError> {
    let capture = connection
        .exec_capture(&processes_command(), PROCESSES_TIMEOUT, PROCESSES_MAX_BYTES)
        .await
        .map_err(|err| ProcessesError::Exec(err.to_string()))?;
    if capture.timed_out {
        return Err(ProcessesError::Timeout);
    }
    if capture.stdout.trim().is_empty() {
        // 连 `@@ZSSH` 行都没有：登录 shell 连 sh 都起不来（比如 Windows 服务器）。
        let detail = capture.stderr.lines().next().unwrap_or("").trim();
        return if detail.is_empty() {
            Err(ProcessesError::Unsupported)
        } else {
            Err(ProcessesError::Exec(detail.to_owned()))
        };
    }
    let mut stdout = capture.stdout;
    let cut = capture.stdout_truncated_bytes > 0;
    if cut {
        // 被字节上限截断：最后一行可能只有半截，丢掉。
        if let Some(end) = stdout.rfind('\n') {
            stdout.truncate(end);
        }
    }
    let mut report = parse_processes(&stdout)?;
    report.truncated |= cut;
    Ok(report)
}

// ---- 筛选 / 搜索 / 排序（界面用，纯函数）----

/// 分段：决定默认怎么排。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcMode {
    Cpu,
    Mem,
}

impl ProcMode {
    pub const ALL: [ProcMode; 2] = [Self::Cpu, Self::Mem];

    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Mem => "内存",
        }
    }

    /// 这一档默认排序的列。
    pub fn default_key(self) -> ProcSortKey {
        match self {
            Self::Cpu => ProcSortKey::Cpu,
            Self::Mem => ProcSortKey::Mem,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcSortKey {
    Pid,
    User,
    Cpu,
    Mem,
    Rss,
    State,
    Elapsed,
    Command,
}

impl ProcSortKey {
    /// 数字列第一次点是从大到小（用户要找的是最大的），文字列是升序。
    pub fn numeric(self) -> bool {
        matches!(
            self,
            Self::Pid | Self::Cpu | Self::Mem | Self::Rss | Self::Elapsed
        )
    }
}

/// 排序设置：`key` 为空 = 按分段的默认（CPU 档按 CPU% 降序、内存档按内存% 降序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ProcSort {
    pub key: Option<ProcSortKey>,
    pub descending: bool,
}

impl ProcSort {
    /// 点一次表头：新的一列先按"该列的常用方向"（数字降序、文字升序），再点反向，第三次回到默认。
    pub fn toggled(self, key: ProcSortKey) -> Self {
        let first_descending = key.numeric() && key != ProcSortKey::Pid;
        match (self.key, self.descending) {
            (Some(current), descending) if current == key => {
                if descending == first_descending {
                    Self {
                        key: Some(key),
                        descending: !first_descending,
                    }
                } else {
                    Self::default()
                }
            }
            _ => Self {
                key: Some(key),
                descending: first_descending,
            },
        }
    }

    /// 实际用的（列，是否降序）。
    pub fn effective(self, mode: ProcMode) -> (ProcSortKey, bool) {
        match self.key {
            Some(key) => (key, self.descending),
            None => (mode.default_key(), true),
        }
    }
}

/// 搜索：PID、用户、命令里包含这段文字（不分大小写、忽略首尾空白）。
pub fn matches_query(row: &ProcRow, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    row.pid.to_string().contains(&query)
        || row.user.to_lowercase().contains(&query)
        || row.command.to_lowercase().contains(&query)
}

/// 没有值的沉底（不管升降序）。
fn cmp_opt<T: PartialOrd>(a: Option<T>, b: Option<T>, descending: bool) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => {
            let order = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
            if descending {
                order.reverse()
            } else {
                order
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_rows(key: ProcSortKey, descending: bool, a: &ProcRow, b: &ProcRow) -> Ordering {
    let directed = |order: Ordering| if descending { order.reverse() } else { order };
    match key {
        ProcSortKey::Pid => directed(a.pid.cmp(&b.pid)),
        ProcSortKey::User => directed(a.user.to_lowercase().cmp(&b.user.to_lowercase())),
        ProcSortKey::Cpu => cmp_opt(a.cpu, b.cpu, descending),
        ProcSortKey::Mem => cmp_opt(a.mem, b.mem, descending),
        ProcSortKey::Rss => cmp_opt(a.rss_kb, b.rss_kb, descending),
        ProcSortKey::State => directed(a.state.cmp(&b.state)),
        ProcSortKey::Elapsed => cmp_opt(a.elapsed_secs, b.elapsed_secs, descending),
        ProcSortKey::Command => directed(a.command.to_lowercase().cmp(&b.command.to_lowercase())),
    }
}

/// 搜索 + 排序之后要显示的行（下标指向 `rows`）。同值时先比另一档（CPU 同值看内存，反之亦然），再比 PID，
/// 所以顺序是确定的。
pub fn view_indices(rows: &[ProcRow], mode: ProcMode, query: &str, sort: ProcSort) -> Vec<usize> {
    let mut shown: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| matches_query(row, query))
        .map(|(index, _)| index)
        .collect();
    let (key, descending) = sort.effective(mode);
    let other = match mode {
        ProcMode::Cpu => ProcSortKey::Mem,
        ProcMode::Mem => ProcSortKey::Cpu,
    };
    shown.sort_by(|&a, &b| {
        let (a, b) = (&rows[a], &rows[b]);
        cmp_rows(key, descending, a, b)
            .then_with(|| cmp_rows(other, true, a, b))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    shown
}

/// 运行时长的显示：`42秒` / `5分12秒` / `3小时12分` / `2天3小时`。
pub fn elapsed_label(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86_400,
        secs % 86_400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}天{h}小时")
    } else if h > 0 {
        format!("{h}小时{m}分")
    } else if m > 0 {
        format!("{m}分{s}秒")
    } else {
        format!("{s}秒")
    }
}

// ---- 结束进程 ----

/// 发给进程的信号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillSignal {
    /// SIGTERM：请进程自己退出（可被忽略）。
    Term,
    /// SIGKILL：内核直接结束，进程来不及清理。
    Kill,
}

impl KillSignal {
    pub fn name(self) -> &'static str {
        match self {
            Self::Term => "TERM",
            Self::Kill => "KILL",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Term => "结束进程",
            Self::Kill => "强制结束",
        }
    }
}

/// 结束进程失败的原因。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum KillError {
    #[error("PID 不合法（只能是 1 到 {PID_MAX} 的纯数字）。")]
    InvalidPid,
    #[error("没有权限结束这个进程，需要 root 或 sudo。")]
    NoPermission,
    #[error("这个进程已经不存在了。")]
    NoSuchProcess,
    #[error("这个 PID 现在对应的已经不是你选中的那个进程，列表已刷新，请重新选择。")]
    Changed,
    #[error("这是受保护的系统进程（{}），不能结束。", .0.reason())]
    Protected(Protection),
    #[error("等了 5 秒没有结果。")]
    Timeout,
    #[error("结束失败：{0}")]
    Failed(String),
}

impl KillError {
    /// 给界面的完整提示；没权限时点出进程属于谁、当前用户是谁。**不自动 sudo**。
    pub fn describe(&self, owner: &str, login_user: Option<&str>) -> String {
        match self {
            Self::NoPermission => {
                let me = login_user
                    .filter(|u| !u.is_empty())
                    .map(|u| format!("，你现在是 {u}"))
                    .unwrap_or_default();
                format!("没有权限结束这个进程（属于用户 {owner}{me}），需要 root 或 sudo。")
            }
            other => other.to_string(),
        }
    }
}

/// PID 只允许纯数字（不带符号、空白、前缀），范围 1..=[`PID_MAX`]。`0` 和负数会让 `kill` 把信号发给整个进程组 /
/// 所有进程，绝不能出现。
pub fn validate_pid(text: &str) -> Result<u32, KillError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(KillError::InvalidPid);
    }
    let pid: u32 = text.parse().map_err(|_| KillError::InvalidPid)?;
    validate_pid_number(pid)
}

fn validate_pid_number(pid: u32) -> Result<u32, KillError> {
    if (1..=PID_MAX).contains(&pid) {
        Ok(pid)
    } else {
        Err(KillError::InvalidPid)
    }
}

/// 结束进程的命令行：`exec sh -c 'kill -TERM 1234'`。命令里只有常量和校验过的数字。
pub fn kill_command(pid: u32, signal: KillSignal) -> Result<String, KillError> {
    let pid = validate_pid_number(pid)?;
    Ok(format!(
        "exec sh -c {}",
        shell_quote(&format!("kill -{} {}", signal.name(), pid))
    ))
}

/// 把 `kill` 的失败（退出码 + stderr）归类。dash / bash / BusyBox / procps 的措辞不同，按关键词认。
pub fn classify_kill_failure(stderr: &str, stdout: &str) -> KillError {
    let text = format!("{stderr}\n{stdout}").to_lowercase();
    if text.contains("not permitted") || text.contains("permission denied") {
        KillError::NoPermission
    } else if text.contains("no such process") || text.contains("no such pid") {
        KillError::NoSuchProcess
    } else {
        let first = stderr
            .lines()
            .chain(stdout.lines())
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("kill 没有成功，也没有给出原因");
        KillError::Failed(first.to_owned())
    }
}

/// 列表过时之后 PID 可能被别的进程复用：同一个 PID、同一个用户、同一条命令行，并且运行时间没有变短，才算
/// 还是原来那个。
pub fn same_process(before: &ProcRow, now: &ProcRow) -> bool {
    before.pid == now.pid
        && before.user == now.user
        && before.command == now.command
        && match (before.elapsed_secs, now.elapsed_secs) {
            (Some(was), Some(is)) => is >= was,
            _ => true,
        }
}

/// 结束之后再采一次，这个进程的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillVerdict {
    /// 已经不在了。
    Gone,
    /// 还在，但是僵尸（已结束，等父进程回收）。
    Zombie,
    /// 还在运行（可能忽略了 TERM，或者还在退出）。
    StillRunning,
}

pub fn kill_verdict(report: &ProcessReport, target: &ProcRow) -> KillVerdict {
    match report.row(target.pid) {
        None => KillVerdict::Gone,
        // PID 被别的进程复用了：原来那个已经没了。
        Some(now) if now.command != target.command || now.user != target.user => KillVerdict::Gone,
        Some(now) if now.is_zombie() => KillVerdict::Zombie,
        Some(_) => KillVerdict::StillRunning,
    }
}

/// 结束一个进程（写操作，只在用户确认之后调用）。成功只表示信号发出去了，是否真的结束看
/// [`kill_verdict`]。
///
/// 先重新采一次核对 PID 还是用户选中的那个进程，再按**这次新采的**列表判断它受不受保护（受保护的一律拒绝，
/// 没有解锁途径）——界面已经置灰，这里是最后一道关；没权限返回 [`KillError::NoPermission`]，不会换 sudo 重试。
pub async fn terminate_process(
    connection: &Connection,
    target: &ProcRow,
    signal: KillSignal,
) -> Result<(), KillError> {
    let command = kill_command(target.pid, signal)?;
    let fresh = collect_processes(connection)
        .await
        .map_err(|err| KillError::Failed(err.to_string()))?;
    match fresh.row(target.pid) {
        None => return Err(KillError::NoSuchProcess),
        Some(now) if !same_process(target, now) => return Err(KillError::Changed),
        Some(_) => {}
    }
    if let Some(protection) = fresh.protection(target.pid) {
        return Err(KillError::Protected(protection));
    }
    let capture = connection
        .exec_capture(&command, KILL_TIMEOUT, 64 * 1024)
        .await
        .map_err(|err| KillError::Failed(err.to_string()))?;
    if capture.timed_out {
        return Err(KillError::Timeout);
    }
    match capture.exit_code {
        Some(0) => Ok(()),
        _ => Err(classify_kill_failure(&capture.stderr, &capture.stdout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// procps（带 `etimes`）：一台 root 登录的机器；4242 是采集脚本的 shell，3100 / 3090 / 800 是承载连接的 sshd 链，
    /// 4250 是脚本里跑的 `ps`（要被滤掉）。命令行带空格、长命令、僵尸、内核线程都有。
    const PROCPS_SAMPLE: &str = "\
@@ZSSH uid 0
@@ZSSH user root
@@ZSSH self 4242
@@ZSSH memtotal 16000000
@@ZSSH source procps
    PID    PPID USER     %CPU %MEM   RSS STAT ELAPSED COMMAND
    812       1 app      55.5 12.4 1984000 Ssl    93784 /usr/bin/app-worker --queue=high --workers 4
   1290       1 www-data  7.0  6.1 976000 S       5000 /usr/sbin/web-server -g daemon on; master_process on;
      1       0 root      0.0  0.3 48000 Ss     172800 /sbin/init splash
    800       1 root      0.0  0.1 15000 Ss     172700 sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups
   3090     800 root      0.0  0.1 11000 Ss       3600 sshd: app [priv]
   3100    3090 app       0.0  0.1  9000 S        3600 sshd: app@notty
   4242    3100 app       0.4  0.0  3000 Ss          0 sh -c exec stuff
   4250    4242 app      99.0  0.0  3200 R           0 ps -ww -eo pid,ppid,user:32,pcpu
   5001     812 app       0.0  0.0     0 Z        1200 [worker] <defunct>
      2       0 root      0.0  0.0     0 S      172800 [kthreadd]
";

    #[test]
    fn procps_output_parses_with_columns_from_the_header() {
        let report = parse_processes(PROCPS_SAMPLE).expect("解析");
        assert_eq!(report.source, ProcSource::Procps);
        assert_eq!(
            (report.uid, report.user.as_deref()),
            (Some(0), Some("root"))
        );
        assert!(report.cpu_available() && report.mem_available());
        // 采集脚本自己（4242）和它的子进程（ps 4250）不在列表里。
        assert!(report.row(4242).is_none() && report.row(4250).is_none());
        assert_eq!(report.rows.len(), 8);
        let app = report.row(812).expect("812");
        assert_eq!(app.ppid, Some(1));
        assert_eq!(app.user, "app");
        assert_eq!(app.cpu, Some(55.5));
        assert_eq!(app.mem, Some(12.4));
        assert_eq!(app.rss_kb, Some(1_984_000));
        assert_eq!(app.state, "Ssl");
        assert_eq!(app.elapsed_secs, Some(93_784));
        // 命令行带空格、`;` 原样保留。
        assert_eq!(app.command, "/usr/bin/app-worker --queue=high --workers 4");
        assert_eq!(
            report.row(1290).map(|r| r.command.as_str()),
            Some("/usr/sbin/web-server -g daemon on; master_process on;")
        );
        assert!(report.row(5001).is_some_and(ProcRow::is_zombie));
        assert!(!app.is_zombie());
    }

    #[test]
    fn the_ssh_session_chain_and_init_are_flagged() {
        let report = parse_processes(PROCPS_SAMPLE).expect("解析");
        assert_eq!(report.session_chain(), vec![3100, 3090, 800]);
        assert_eq!(report.risk_of(1), ProcRisk::Init);
        for pid in [3100, 3090, 800] {
            assert_eq!(report.risk_of(pid), ProcRisk::SshSession, "{pid}");
        }
        assert_eq!(report.risk_of(812), ProcRisk::Normal);
        assert_eq!(report.risk_of(5001), ProcRisk::Normal);
        // 不知道脚本 shell 是谁（旧输出 / 没有 self 行）：不乱标。
        let blind =
            parse_processes(&PROCPS_SAMPLE.replace("@@ZSSH self 4242\n", "")).expect("解析");
        assert!(blind.session_chain().is_empty());
        assert_eq!(blind.risk_of(3100), ProcRisk::Normal);
        assert_eq!(blind.risk_of(1), ProcRisk::Init);
        assert!(blind.row(4242).is_some(), "不知道 self 就不过滤");
    }

    #[test]
    fn old_procps_with_etime_text_parses_the_run_time() {
        let out = "\
@@ZSSH uid 1000
@@ZSSH user app
@@ZSSH source procps
  PID  PPID USER      %CPU %MEM   RSS STAT     ELAPSED COMMAND
  100     1 root       0.1  0.2  2000 Ss    12-03:04:05 /usr/sbin/cron -f
  101     1 app        1.5  1.0  8000 S        03:04:05 python3 job.py
  102     1 app        0.0  0.0   100 S           04:05 sleep 300
  103     1 app        0.0  0.0   100 S           00:07 tail -f /var/log/app.log
";
        let report = parse_processes(out).expect("解析");
        assert_eq!(report.uid, Some(1000));
        let secs = |pid| report.row(pid).and_then(|r| r.elapsed_secs);
        assert_eq!(secs(100), Some(12 * 86_400 + 3 * 3600 + 4 * 60 + 5));
        assert_eq!(secs(101), Some(3 * 3600 + 4 * 60 + 5));
        assert_eq!(secs(102), Some(4 * 60 + 5));
        assert_eq!(secs(103), Some(7));
    }

    #[test]
    fn busybox_ps_with_o_has_no_cpu_and_computes_memory_from_rss() {
        let out = "\
@@ZSSH uid 0
@@ZSSH memtotal 1000000
@@ZSSH source procps
@@ZSSH source bb
  PID  PPID USER     STAT   RSS ELAPSED COMMAND
    1     0 root     S     1200 5-02:00:00 init
  310     1 root     S    50000    01:02:03 /usr/sbin/dropbear -F -p 22
  399   310 admin    R     2500       00:05 -sh
";
        let report = parse_processes(out).expect("解析（取最后一段）");
        assert_eq!(report.source, ProcSource::PsBasic);
        assert!(!report.cpu_available(), "BusyBox 的 ps 没有 CPU%");
        assert!(report.mem_available());
        let db = report.row(310).expect("310");
        assert_eq!(db.cpu, None);
        assert_eq!(db.rss_kb, Some(50_000));
        assert!((db.mem.expect("算出来的") - 5.0).abs() < 1e-9);
        assert_eq!(db.elapsed_secs, Some(3723));
        assert_eq!(db.command, "/usr/sbin/dropbear -F -p 22");
        assert_eq!(report.row(399).map(|r| r.state.as_str()), Some("R"));
    }

    #[test]
    fn minimal_busybox_ps_only_has_pid_user_state_and_command() {
        let out = "\
@@ZSSH uid 0
@@ZSSH source bb
@@ZSSH source bb
@@ZSSH source bb
PID   USER     VSZ STAT COMMAND
    1 root     1356m S    init
  202 root     3144 S    /sbin/syslogd -n
  203 root     1356 R    ps w
";
        let report = parse_processes(out).expect("解析");
        assert_eq!(report.source, ProcSource::PsMinimal);
        assert!(!report.cpu_available() && !report.mem_available());
        assert_eq!(report.rows.len(), 3);
        assert_eq!(report.row(202).map(|r| r.user.as_str()), Some("root"));
        assert_eq!(report.row(202).map(|r| r.state.as_str()), Some("S"));
        assert_eq!(report.row(202).and_then(|r| r.ppid), None);
        // 新版 BusyBox 的 `ps` 表头是 PID USER TIME COMMAND：TIME 是 CPU 时间，不是运行时长，不能当运行时长用。
        let new = "@@ZSSH source bb\nPID   USER     TIME  COMMAND\n    1 root      0:03 init\n";
        let report = parse_processes(new).expect("解析");
        assert_eq!(report.rows[0].elapsed_secs, None);
        assert_eq!(report.rows[0].command, "init");
    }

    #[test]
    fn proc_stat_fallback_output_parses_like_ps() {
        let out = "\
@@ZSSH uid 1000
@@ZSSH source proc
PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
1 0 root 0.0 0.1 4812 S 86400 systemd
23 1 app 12.5 3.0 31000 R 120 tmux: server
";
        let report = parse_processes(out).expect("解析");
        assert_eq!(report.source, ProcSource::Proc);
        assert!(report.cpu_available());
        let tmux = report.row(23).expect("23");
        assert_eq!(tmux.command, "tmux: server");
        assert_eq!(tmux.cpu, Some(12.5));
        assert_eq!(tmux.elapsed_secs, Some(120));
    }

    #[test]
    fn the_last_source_wins_and_garbage_is_skipped() {
        let out = "\
@@ZSSH source procps
    PID    PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
      9       1 old  1.0  1.0   1 S 1 stale-row-from-an-abandoned-attempt
@@ZSSH source procps
    PID    PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
     10       1 root  2.0  1.0  10 S 5 init
this line is not a process
  abc  1 root 1 1 1 S 1 bad pid
     11       1 root
";
        let report = parse_processes(out).expect("解析");
        let pids: Vec<u32> = report.rows.iter().map(|r| r.pid).collect();
        assert_eq!(pids, vec![10]);
    }

    #[test]
    fn unsupported_servers_are_reported_as_such() {
        assert_eq!(parse_processes(""), Err(ProcessesError::Unsupported));
        assert_eq!(
            parse_processes("@@ZSSH uid 1\n@@ZSSH self 1\n"),
            Err(ProcessesError::Unsupported)
        );
        // 所有 ps 都失败、/proc 也读不到：来源标了，但一行表头都没有。
        assert_eq!(
            parse_processes("@@ZSSH source procps\n@@ZSSH source proc\n"),
            Err(ProcessesError::Unsupported)
        );
    }

    #[test]
    fn more_than_ten_thousand_processes_are_truncated_and_flagged() {
        let mut out = String::from(
            "@@ZSSH source procps\nPID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND\n",
        );
        for pid in 1..=(PROCESSES_MAX_ROWS + 25) {
            out.push_str(&format!("{pid} 1 app 1.0 0.5 100 S 60 worker-{pid}\n"));
        }
        let report = parse_processes(&out).expect("解析");
        assert_eq!(report.rows.len(), PROCESSES_MAX_ROWS);
        assert!(report.truncated);
        let exact = out
            .lines()
            .take(2 + PROCESSES_MAX_ROWS)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!parse_processes(&exact).expect("解析").truncated);
    }

    #[test]
    fn elapsed_times_in_both_ps_formats() {
        assert_eq!(parse_elapsed("93784"), Some(93_784));
        assert_eq!(parse_elapsed("05:09"), Some(309));
        assert_eq!(parse_elapsed("1-00:00:01"), Some(86_401));
        assert_eq!(parse_elapsed("-"), None);
        assert_eq!(parse_elapsed("12"), Some(12));
        assert_eq!(parse_elapsed("1:2:3:4"), None);
        assert_eq!(parse_elapsed(""), None);
        assert_eq!(elapsed_label(42), "42秒");
        assert_eq!(elapsed_label(309), "5分9秒");
        assert_eq!(elapsed_label(3 * 3600 + 12 * 60 + 9), "3小时12分");
        assert_eq!(elapsed_label(2 * 86_400 + 3 * 3600), "2天3小时");
    }

    fn sample_rows() -> Vec<ProcRow> {
        let row = |pid, user: &str, cpu: Option<f64>, mem: Option<f64>, command: &str| ProcRow {
            pid,
            ppid: Some(1),
            user: user.to_owned(),
            cpu,
            mem,
            rss_kb: mem.map(|m| (m * 1000.0) as u64),
            state: "S".to_owned(),
            elapsed_secs: Some(u64::from(pid) * 10),
            command: command.to_owned(),
        };
        vec![
            row(10, "root", Some(1.0), Some(9.0), "/sbin/init"),
            row(20, "app", Some(50.0), Some(3.0), "/usr/bin/App-Worker --x"),
            row(30, "www-data", Some(50.0), Some(7.0), "web-server"),
            row(40, "app", None, None, "no-numbers"),
            row(250, "app", Some(0.0), Some(0.0), "sleep 20"),
        ]
    }

    fn pids(rows: &[ProcRow], shown: &[usize]) -> Vec<u32> {
        shown.iter().map(|&i| rows[i].pid).collect()
    }

    #[test]
    fn each_mode_sorts_by_its_own_column_with_the_other_as_tiebreak() {
        let rows = sample_rows();
        let cpu = view_indices(&rows, ProcMode::Cpu, "", ProcSort::default());
        // 20 和 30 的 CPU 同为 50：内存大的（30）在前；没有数值的（40）沉底。
        assert_eq!(pids(&rows, &cpu), vec![30, 20, 10, 250, 40]);
        let mem = view_indices(&rows, ProcMode::Mem, "", ProcSort::default());
        assert_eq!(pids(&rows, &mem), vec![10, 30, 20, 250, 40]);
    }

    #[test]
    fn header_clicks_cycle_through_the_common_direction_then_the_reverse_then_the_default() {
        let rows = sample_rows();
        // 数字列：先降序，再升序，再回默认。
        let mut sort = ProcSort::default().toggled(ProcSortKey::Rss);
        assert_eq!((sort.key, sort.descending), (Some(ProcSortKey::Rss), true));
        let shown = view_indices(&rows, ProcMode::Cpu, "", sort);
        assert_eq!(pids(&rows, &shown), vec![10, 30, 20, 250, 40]);
        sort = sort.toggled(ProcSortKey::Rss);
        assert!(!sort.descending);
        let shown = view_indices(&rows, ProcMode::Cpu, "", sort);
        // 升序：没有数值的仍然沉底。
        assert_eq!(pids(&rows, &shown), vec![250, 20, 30, 10, 40]);
        sort = sort.toggled(ProcSortKey::Rss);
        assert_eq!(sort, ProcSort::default());
        // 文字列：先升序（不分大小写），再降序，再回默认。
        let sort = ProcSort::default().toggled(ProcSortKey::Command);
        assert!(!sort.descending);
        let shown = view_indices(&rows, ProcMode::Cpu, "", sort);
        assert_eq!(pids(&rows, &shown), vec![10, 20, 40, 250, 30]);
        let sort = sort.toggled(ProcSortKey::Command);
        assert!(sort.descending);
        assert_eq!(sort.toggled(ProcSortKey::Command), ProcSort::default());
        // PID 虽然是数字，第一次点是升序（小的在上，和 ps 的习惯一致）。
        assert!(!ProcSort::default().toggled(ProcSortKey::Pid).descending);
        // 换一列：从那一列的常用方向重新开始。
        let sort = ProcSort::default()
            .toggled(ProcSortKey::User)
            .toggled(ProcSortKey::Cpu);
        assert_eq!((sort.key, sort.descending), (Some(ProcSortKey::Cpu), true));
    }

    #[test]
    fn search_matches_pid_user_and_command_ignoring_case() {
        let rows = sample_rows();
        let find = |q: &str| {
            pids(
                &rows,
                &view_indices(&rows, ProcMode::Cpu, q, ProcSort::default()),
            )
        };
        assert_eq!(find("app-worker"), vec![20]);
        assert_eq!(find("  WWW "), vec![30]);
        assert_eq!(find("25"), vec![250]);
        assert_eq!(find("app"), vec![20, 250, 40]);
        assert!(find("nothing-like-this").is_empty());
        assert_eq!(find("").len(), rows.len());
    }

    #[test]
    fn a_big_list_sorts_and_filters_quickly() {
        let rows: Vec<ProcRow> = (1..=PROCESSES_MAX_ROWS as u32)
            .map(|pid| ProcRow {
                pid,
                ppid: Some(1),
                user: "app".to_owned(),
                cpu: Some(f64::from(pid % 97)),
                mem: Some(f64::from(pid % 31)),
                rss_kb: Some(u64::from(pid)),
                state: "S".to_owned(),
                elapsed_secs: Some(u64::from(pid)),
                command: format!("worker-{pid}"),
            })
            .collect();
        let started = std::time::Instant::now();
        let shown = view_indices(&rows, ProcMode::Cpu, "worker-9", ProcSort::default());
        assert!(!shown.is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn only_plain_digits_are_a_pid() {
        assert_eq!(validate_pid("1"), Ok(1));
        assert_eq!(validate_pid("4242"), Ok(4242));
        assert_eq!(validate_pid("4194304"), Ok(PID_MAX));
        for bad in [
            "",
            "0",
            "-1",
            "+5",
            " 5",
            "5 ",
            "5;reboot",
            "$(reboot)",
            "5\n6",
            "１２",
            "0x10",
            "1e3",
            "4194305",
            "99999999999999999999",
            "-",
            "5 6",
            "5&&id",
        ] {
            assert_eq!(validate_pid(bad), Err(KillError::InvalidPid), "{bad:?}");
        }
    }

    #[test]
    fn the_kill_command_only_ever_contains_the_signal_and_the_number() {
        assert_eq!(
            kill_command(4242, KillSignal::Term).expect("命令"),
            "exec sh -c 'kill -TERM 4242'"
        );
        assert_eq!(
            kill_command(7, KillSignal::Kill).expect("命令"),
            "exec sh -c 'kill -KILL 7'"
        );
        // 0 会让 kill 把信号发给整个进程组，超过 pid_max 的数也不可能是 PID。
        assert_eq!(
            kill_command(0, KillSignal::Term),
            Err(KillError::InvalidPid)
        );
        assert_eq!(
            kill_command(PID_MAX + 1, KillSignal::Kill),
            Err(KillError::InvalidPid)
        );
        // 任何合法 PID 生成的命令都是这一个固定形状（不会夹带别的东西）。
        for pid in [1, 2, 99, 65_535, PID_MAX] {
            for signal in [KillSignal::Term, KillSignal::Kill] {
                let command = kill_command(pid, signal).expect("命令");
                assert_eq!(
                    command,
                    format!("exec sh -c 'kill -{} {pid}'", signal.name())
                );
                assert!(!command.contains("sudo"));
            }
        }
    }

    #[test]
    fn kill_failures_map_to_the_right_reason_across_shells() {
        let cases = [
            (
                "sh: 1: kill: Operation not permitted",
                KillError::NoPermission,
            ),
            (
                "kill: (1234) - Operation not permitted",
                KillError::NoPermission,
            ),
            (
                "kill: can't kill pid 1234: Operation not permitted",
                KillError::NoPermission,
            ),
            ("kill: Permission denied", KillError::NoPermission),
            ("sh: 1: kill: No such process", KillError::NoSuchProcess),
            ("kill: (1234) - No such process", KillError::NoSuchProcess),
            (
                "kill: can't kill pid 1234: No such process",
                KillError::NoSuchProcess,
            ),
        ];
        for (stderr, want) in cases {
            assert_eq!(classify_kill_failure(stderr, ""), want, "{stderr}");
        }
        assert_eq!(
            classify_kill_failure("kill: something else broke\nmore", ""),
            KillError::Failed("kill: something else broke".to_owned())
        );
        assert_eq!(
            classify_kill_failure("", ""),
            KillError::Failed("kill 没有成功，也没有给出原因".to_owned())
        );
    }

    #[test]
    fn the_permission_message_names_the_owner_and_never_suggests_an_automatic_sudo() {
        let text = KillError::NoPermission.describe("root", Some("app"));
        assert!(text.contains("没有权限结束这个进程（属于用户 root"));
        assert!(text.contains("你现在是 app"));
        assert!(text.contains("需要 root 或 sudo"));
        let unknown = KillError::NoPermission.describe("www-data", None);
        assert!(unknown.contains("属于用户 www-data）"));
        assert_eq!(
            KillError::NoSuchProcess.describe("x", None),
            "这个进程已经不存在了。"
        );
    }

    #[test]
    fn a_reused_pid_is_not_the_same_process() {
        let old = sample_rows().remove(1);
        let mut same = old.clone();
        same.elapsed_secs = Some(old.elapsed_secs.unwrap_or(0) + 60);
        assert!(same_process(&old, &same));
        // 运行时间反而变短：PID 被复用了。
        let mut reused = old.clone();
        reused.elapsed_secs = Some(1);
        assert!(!same_process(&old, &reused));
        let mut other_command = old.clone();
        other_command.command = "/bin/other".to_owned();
        assert!(!same_process(&old, &other_command));
        let mut other_user = old.clone();
        other_user.user = "root".to_owned();
        assert!(!same_process(&old, &other_user));
        // 没有运行时长（BusyBox）：只看用户和命令。
        let (mut a, mut b) = (old.clone(), old.clone());
        a.elapsed_secs = None;
        b.elapsed_secs = Some(5);
        assert!(same_process(&a, &b));
    }

    #[test]
    fn the_verdict_after_a_kill_distinguishes_gone_zombie_and_still_running() {
        let report = parse_processes(PROCPS_SAMPLE).expect("解析");
        let running = report.row(812).expect("812").clone();
        assert_eq!(kill_verdict(&report, &running), KillVerdict::StillRunning);
        let zombie = report.row(5001).expect("5001").clone();
        assert_eq!(kill_verdict(&report, &zombie), KillVerdict::Zombie);
        let gone = ProcRow {
            pid: 999_999,
            ..running.clone()
        };
        assert_eq!(kill_verdict(&report, &gone), KillVerdict::Gone);
        // 同一个 PID 现在是另一条命令：原来那个已经没了。
        let renamed = ProcRow {
            command: "/bin/something-else".to_owned(),
            ..running
        };
        assert_eq!(kill_verdict(&report, &renamed), KillVerdict::Gone);
    }

    #[test]
    fn the_collection_script_is_read_only() {
        let command = processes_command();
        assert!(command.starts_with("exec sh -c '"));
        assert_eq!(command.matches('\'').count() % 2, 0, "引号要成对");
        let lower = SCRIPT.to_lowercase();
        for forbidden in [
            "kill", "rm ", "sudo", "reboot", "shutdown", "mv ", "chmod", "chown", "tee ", "dd ",
            "mkfs",
        ] {
            assert!(!lower.contains(forbidden), "采集脚本里不该有 {forbidden:?}");
        }
        // 唯一的重定向是把错误丢进 /dev/null。
        assert_eq!(
            SCRIPT.matches(">/").count(),
            SCRIPT.matches(">/dev/null").count()
        );
    }

    /// 用本机 sh 真跑采集脚本：`ps` 用 shell 函数按参数顶替，验证 fallback 的先后顺序、来源标记，
    /// 并把整份输出交给解析器。本机没有 `sh`、sh 能启动但立刻崩（退出码非 0 / 没有输出）都跳过
    /// 并打印原因（见 [`crate::test_util::run_sh`]）。
    fn run_script_with(test: &str, stubs: &str) -> Option<String> {
        crate::test_util::run_sh(test, &format!("{stubs}\n{SCRIPT}"))
    }

    #[test]
    fn the_script_falls_back_through_the_ps_variants_in_order() {
        // 1) 现代 procps：第一种就成功，后面的不会再跑。
        let modern = r#"ps() { case "$*" in
  *etimes*) printf '  PID  PPID USER %%CPU %%MEM RSS STAT ELAPSED COMMAND\n  812     1 app 55.5 12.4 1000 S 100 app-worker\n';;
  *) return 1;; esac; }"#;
        const NAME: &str = "the_script_falls_back_through_the_ps_variants_in_order";
        let Some(out) = run_script_with(NAME, modern) else {
            return;
        };
        assert_eq!(out.matches("@@ZSSH source").count(), 1, "{out}");
        let report = parse_processes(&out).expect("解析");
        assert_eq!((report.source, report.rows.len()), (ProcSource::Procps, 1));
        assert!(report.self_pid.is_some(), "脚本要报告自己的 pid");

        // 2) 老 procps：只认 etime（不认 etimes 和 user:32）。
        let old = r#"ps() { case "$*" in
  *etimes*|*user:32*) return 1;;
  *--sort*) printf '  PID  PPID USER %%CPU %%MEM RSS STAT ELAPSED COMMAND\n  812     1 app 55.5 12.4 1000 S 01:40 app-worker\n';;
  *) return 1;; esac; }"#;
        let Some(out) = run_script_with(NAME, old) else {
            return;
        };
        let report = parse_processes(&out).expect("解析");
        assert_eq!(report.source, ProcSource::Procps);
        assert_eq!(report.rows[0].elapsed_secs, Some(100));

        // 3) BusyBox：不认 --sort，认 -o（带 etime）。
        let busybox = r#"ps() { case "$*" in
  *--sort*) return 1;;
  *"-o pid,ppid,user,stat,rss,etime,args"*) printf '  PID  PPID USER STAT RSS ELAPSED COMMAND\n  310     1 root S 5000 01:02:03 dropbear -F\n';;
  *) return 1;; esac; }"#;
        let Some(out) = run_script_with(NAME, busybox) else {
            return;
        };
        let report = parse_processes(&out).expect("解析");
        assert_eq!(report.source, ProcSource::PsBasic);
        assert_eq!(report.rows[0].elapsed_secs, Some(3723));

        // 4) 最简 BusyBox：只认 `ps w`。
        let minimal = r#"ps() { case "$*" in
  w) printf 'PID   USER     VSZ STAT COMMAND\n    1 root     1356 S    init\n';;
  *) return 1;; esac; }"#;
        let Some(out) = run_script_with(NAME, minimal) else {
            return;
        };
        let report = parse_processes(&out).expect("解析");
        assert_eq!(report.source, ProcSource::PsMinimal);
        assert_eq!(report.rows[0].command, "init");
    }

    #[test]
    fn the_proc_fallback_awk_computes_the_same_columns_from_proc_files() {
        // /proc 换成临时目录里的样本：两个进程，一个命令名带空格，一个带括号。
        let dir = std::env::temp_dir().join(format!("zssh-proc-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proc = dir.join("proc");
        for (pid, stat, uid) in [
            (
                "123",
                "123 (app worker) S 1 123 123 0 -1 4194560 100 0 0 0 600 300 0 0 20 0 4 0 90000 100000000 2500 0 0",
                "1000",
            ),
            (
                "7",
                "7 (we (ird)) R 2 7 7 0 -1 4194560 100 0 0 0 100 0 0 0 20 0 1 0 99900 1000 0 0 0",
                "0",
            ),
        ] {
            let d = proc.join(pid);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), format!("{stat}\n")).unwrap();
            std::fs::write(
                d.join("status"),
                format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
            )
            .unwrap();
        }
        std::fs::write(proc.join("uptime"), "1000.00 900.00\n").unwrap();
        std::fs::write(proc.join("meminfo"), "MemTotal:       1000000 kB\n").unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\nalice:x:1000:1000::/home/alice:/bin/sh\n",
        )
        .unwrap();
        let to_sh = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
        let script = SCRIPT
            .replace("/proc/", &format!("{}/", to_sh(&proc)))
            .replace("/etc/passwd", &to_sh(&dir.join("etc/passwd")));
        // 所有 ps 都不可用。
        // getconf 也顶替掉：Git 自带的那个会报 1000，样本按常见的 100 写。
        let full = format!(
            "ps() {{ return 1; }}\ngetconf() {{ case \"$1\" in CLK_TCK) echo 100;; *) echo 4096;; esac; }}\n{script}"
        );
        let Some(text) = crate::test_util::run_sh(
            "the_proc_fallback_awk_computes_the_same_columns_from_proc_files",
            &full,
        ) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let report = parse_processes(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(report.source, ProcSource::Proc, "{text}");
        let app = report.row(123).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(app.command, "app worker");
        assert_eq!(app.user, "alice");
        assert_eq!(app.ppid, Some(1));
        assert_eq!(app.state, "S");
        // 运行时间 = 1000 - 90000/100 = 100 秒；CPU = (600+300)/100 / 100 * 100 = 9%；RSS = 2500 页 * 4096 / 1024 = 10000 KiB。
        assert_eq!(app.elapsed_secs, Some(100));
        assert!(
            (app.cpu.unwrap_or(-1.0) - 9.0).abs() < 0.11,
            "{:?}",
            app.cpu
        );
        assert_eq!(app.rss_kb, Some(10_000));
        assert!(
            (app.mem.unwrap_or(-1.0) - 1.0).abs() < 0.11,
            "{:?}",
            app.mem
        );
        // 命令名里带括号也不会切错。
        let odd = report.row(7).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(odd.command, "we (ird)");
        assert_eq!(odd.user, "root");
    }

    const PROTECTION_SAMPLE: &str = "\
@@ZSSH uid 0
@@ZSSH user root
@@ZSSH self 4242
@@ZSSH source procps
PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
1 0 root 0.0 0.3 4800 Ss 1000 /sbin/init splash
2 0 root 0.0 0.0 0 S 1000 [kthreadd]
14 2 root 0.0 0.0 0 I 1000 [rcu_sched]
15 1 root 0.0 0.0 0 S 1000 [odd-bracket-name]
77 1 root 0.0 0.0 0 S 1000
330 1 root 0.0 0.2 3000 Ss 1000 /lib/systemd/systemd-journald
331 1 root 0.0 0.2 3000 Ss 1000 /lib/systemd/systemd-logind
332 1 root 0.0 0.2 3000 Ss 1000 systemd-networkd
333 1 root 0.0 0.2 3000 Ss 1000 /usr/bin/dbus-daemon --system --address=systemd:
334 1 root 0.0 0.2 3000 Ss 1000 /usr/sbin/NetworkManager --no-daemon
335 1 root 0.0 0.2 3000 Ss 1000 /sbin/agetty -o -p -- \\u --noclear tty1 linux
336 1 root 0.0 0.2 3000 Ss 1000 /usr/sbin/crond -n
337 1 root 0.0 0.2 3000 Ss 1000 /usr/bin/dockerd -H fd://
338 1 root 0.0 0.2 3000 Ss 1000 /usr/bin/containerd
339 1 root 0.0 0.2 3000 Ss 1000 /usr/bin/kubelet --config /etc/k.yaml
340 1 root 0.0 0.2 3000 Ss 1000 /usr/sbin/rsyslogd -n
341 1 root 0.0 0.2 3000 Ss 1000 /sbin/auditd
342 1 root 0.0 0.2 3000 Ss 1000 /usr/lib/polkit-1/polkitd --no-debug
343 1 root 0.0 0.2 3000 Ss 1000 /lib/systemd/systemd-udevd
344 1 root 0.0 0.2 3000 Ss 1000 /lib/systemd/systemd-resolved
345 1 root 0.0 0.2 3000 Ss 1000 /usr/bin/dbus-broker --log 4 --controller 9
800 1 root 0.0 0.1 9000 Ss 1000 sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups
3090 800 root 0.0 0.1 9000 Ss 100 sshd: root [priv]
3100 3090 root 0.0 0.1 9000 S 100 sshd: root@notty
4242 3100 root 0.0 0.0 3000 Ss 0 sh -c exec
5500 800 alice 0.0 0.1 9000 S 100 sshd: alice@pts/1
5501 5500 alice 0.0 0.1 3000 Ss 100 -bash
900 1 root 0.0 0.1 9000 Ss 1000 sshd: /usr/sbin/sshd -D [listener] 0 of 10-100 startups
901 1 root 0.0 0.1 9000 Ss 1000 /usr/sbin/dropbear -F -p 2222
812 1 app 5.0 1.0 9000 S 100 /usr/bin/app-worker --queue=high
813 812 app 0.0 0.0 0 Z 100 [worker] <defunct>
820 1 app 0.0 0.0 9000 S 100 /usr/local/bin/cron-like-job --every 5m
821 1 app 0.0 0.0 9000 S 100 /opt/app/mydockerd-proxy
";

    #[test]
    fn protection_covers_every_class() {
        use Protection::*;
        let report = parse_processes(PROTECTION_SAMPLE).expect("解析");
        let check = |pid: u32, want: Option<Protection>| {
            assert_eq!(report.protection(pid), want, "PID {pid}");
        };
        // 系统初始化进程。
        check(1, Some(Init));
        // 内核线程：PID 2、父进程是 2、`[名字]` 写法、命令行为空。
        for pid in [2, 14, 15, 77] {
            check(pid, Some(KernelThread));
        }
        // 当前 SSH 会话的进程链：会话进程、[priv] 监控进程（采集 shell 往上数的祖先，不含 PID 1）。
        check(3100, Some(SshSession));
        check(3090, Some(SshSession));
        // 本次会话的监听者是链上的祖先，按会话保护；别的 sshd / dropbear 主进程（父进程是 PID 1）是主进程保护。
        check(800, Some(SshSession));
        check(900, Some(SshDaemon));
        check(901, Some(SshDaemon));
        // 关键系统服务：名称精确匹配，带目录、带参数、带冒号都认。
        for pid in 330..=345 {
            check(pid, Some(CriticalService));
        }
        // 别的用户的 sshd 会话进程 / shell：不受保护（结束它只会踢掉那个用户）。
        check(5500, None);
        check(5501, None);
        // 普通进程、僵尸（`[x] <defunct>` 不是内核线程）、名字只是像：不受保护。
        for pid in [812, 813, 820, 821] {
            check(pid, None);
        }
        // 不存在的 PID。
        check(999_999, None);
    }

    #[test]
    fn the_session_chain_is_checked_by_pid_not_by_name() {
        // 链上的进程名叫什么都保护（比如换了 sshd 实现 / 被改名）。
        let renamed = PROTECTION_SAMPLE.replace("sshd: root@notty", "my-session-helper");
        let report = parse_processes(&renamed).expect("解析");
        assert_eq!(report.protection(3100), Some(Protection::SshSession));
    }

    #[test]
    fn without_the_session_chain_every_sshd_is_protected_as_the_current_session() {
        let blind =
            parse_processes(&PROTECTION_SAMPLE.replace("@@ZSSH self 4242\n", "")).expect("解析");
        // 识别不出本次会话是哪一条：所有 sshd 一律按"当前会话"保护（包括别人的会话和主进程）。
        for pid in [800, 900, 901, 3090, 3100, 5500] {
            assert_eq!(blind.protection(pid), Some(Protection::SshSession), "{pid}");
        }
        // 其他类别不受影响。
        assert_eq!(blind.protection(1), Some(Protection::Init));
        assert_eq!(blind.protection(14), Some(Protection::KernelThread));
        assert_eq!(blind.protection(330), Some(Protection::CriticalService));
        assert_eq!(blind.protection(812), None);
        assert_eq!(blind.protection(5501), None);
    }

    #[test]
    fn every_protection_has_a_reason_for_the_lock_tooltip() {
        for (protection, reason) in [
            (Protection::Init, "系统初始化进程"),
            (Protection::KernelThread, "内核线程"),
            (Protection::SshSession, "当前 SSH 会话"),
            (Protection::SshDaemon, "结束会断开所有 SSH 连接"),
            (Protection::CriticalService, "关键系统服务"),
        ] {
            assert_eq!(protection.reason(), reason);
            // 没有解锁途径：提示里只说不能结束，不提任何开关。
            let text = KillError::Protected(protection).describe("root", None);
            assert!(text.contains(reason) && text.contains("不能结束"), "{text}");
            assert!(!text.contains("允许"), "{text}");
        }
    }

    #[test]
    fn the_critical_service_list_is_exactly_the_documented_one() {
        let mut have: Vec<&str> = CRITICAL_SERVICES.to_vec();
        have.sort_unstable();
        let mut want = vec![
            "NetworkManager",
            "agetty",
            "auditd",
            "containerd",
            "cron",
            "crond",
            "dbus-broker",
            "dbus-daemon",
            "dockerd",
            "init",
            "kubelet",
            "polkitd",
            "rsyslogd",
            "systemd",
            "systemd-journald",
            "systemd-logind",
            "systemd-networkd",
            "systemd-resolved",
            "systemd-udevd",
            "udevd",
        ];
        want.sort_unstable();
        assert_eq!(have, want);
    }
}
