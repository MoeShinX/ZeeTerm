//! 服务器上的网络连接列表（概览面板「网卡」一节的"所有连接"）。
//!
//! 在已有连接上开一个 exec 通道（不新建 SSH 连接、不往终端里打字），跑一段只读脚本：
//! 优先 `ss -tunapH`，没有 `ss`（或它整个失败）退到 `netstat -tunap`，再不行读
//! `/proc/net/{tcp,tcp6,udp,udp6}`。脚本用 `@@ZSSH …` 行标出来源，解析全在这里（纯函数，
//! 三种格式各有夹具测试），界面只做展示、筛选、排序。
//!
//! 只读：不结束进程、不断开连接。命令里强制 `LC_ALL=C`，这样状态名、表头不会被翻译。

use std::cmp::Ordering;
use std::net::Ipv6Addr;
use std::time::Duration;

use super::connection::Connection;
use super::cwd::shell_quote;

/// 单次采集的时限：超时就放弃这一次，终端照常。
pub const CONNECTIONS_TIMEOUT: Duration = Duration::from_secs(5);
/// 脚本输出的字节上限（超过只取前面的整行）。
pub const CONNECTIONS_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 最多解析这么多行连接，超过的丢掉并标记 `truncated`。
pub const CONNECTIONS_MAX_ROWS: usize = 20_000;

/// 服务器上跑的脚本。
///
/// 用 `if/&&` 而不是 `||` 串起三个来源：前一个整个失败（没有命令、netlink 被容器拦了）才退到下一个；
/// `ss -tunapH` 不认 `-H`（老 iproute2）时再试一次不带 `-H` 的，表头由解析器跳过。
const SCRIPT: &str = r#"LC_ALL=C
export LC_ALL
echo "@@ZSSH uid $(id -u 2>/dev/null)"
rc=127
if command -v ss >/dev/null 2>&1; then
  echo "@@ZSSH source ss"
  ss -tunapH 2>/dev/null || ss -tunap 2>/dev/null
  rc=$?
fi
if [ "$rc" -ne 0 ] && command -v netstat >/dev/null 2>&1; then
  echo "@@ZSSH source netstat"
  netstat -tunap 2>/dev/null
  rc=$?
fi
if [ "$rc" -ne 0 ]; then
  echo "@@ZSSH source proc"
  for f in tcp tcp6 udp udp6; do
    echo "@@ZSSH file $f"
    cat "/proc/net/$f" 2>/dev/null
  done
fi
exit 0"#;

/// 交给 exec 通道的命令行：外层 `exec sh -c`，登录 shell 是 fish / csh 也照样是 POSIX sh。
pub fn connections_command() -> String {
    format!("exec sh -c {}", shell_quote(SCRIPT))
}

/// 这份列表是从哪来的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnSource {
    Ss,
    Netstat,
    /// 读 `/proc/net`：拿不到进程。
    Proc,
}

impl ConnSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ss => "ss",
            Self::Netstat => "netstat",
            Self::Proc => "/proc/net",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetProto {
    Tcp,
    Udp,
}

/// 连接状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Listen,
    Established,
    SynSent,
    SynRecv,
    FinWait1,
    FinWait2,
    TimeWait,
    Close,
    CloseWait,
    LastAck,
    Closing,
    /// UDP 没连对端（绑定着等数据）。
    Unconn,
    Other(String),
}

impl ConnState {
    /// ss 写 `ESTAB` / `SYN-SENT`，netstat 写 `ESTABLISHED` / `SYN_SENT`：去掉 `-` `_` 再比。
    fn parse(word: &str) -> Self {
        let squashed: String = word
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        match squashed.as_str() {
            "LISTEN" => Self::Listen,
            "ESTAB" | "ESTABLISHED" => Self::Established,
            "SYNSENT" => Self::SynSent,
            "SYNRECV" => Self::SynRecv,
            "FINWAIT1" => Self::FinWait1,
            "FINWAIT2" => Self::FinWait2,
            "TIMEWAIT" => Self::TimeWait,
            "CLOSE" => Self::Close,
            "CLOSEWAIT" => Self::CloseWait,
            "LASTACK" => Self::LastAck,
            "CLOSING" => Self::Closing,
            "UNCONN" => Self::Unconn,
            _ => Self::Other(word.to_owned()),
        }
    }

    /// `/proc/net/tcp` 的 `st` 列（十六进制）。
    fn from_proc_tcp(code: u8) -> Self {
        match code {
            0x01 => Self::Established,
            0x02 => Self::SynSent,
            0x03 => Self::SynRecv,
            0x04 => Self::FinWait1,
            0x05 => Self::FinWait2,
            0x06 => Self::TimeWait,
            0x07 => Self::Close,
            0x08 => Self::CloseWait,
            0x09 => Self::LastAck,
            0x0A => Self::Listen,
            0x0B => Self::Closing,
            other => Self::Other(format!("{other:02X}")),
        }
    }

    /// `/proc/net/udp` 的 `st` 列：01 = 连了对端，07 = 没连。
    fn from_proc_udp(code: u8) -> Self {
        match code {
            0x01 => Self::Established,
            0x07 => Self::Unconn,
            other => Self::Other(format!("{other:02X}")),
        }
    }

    /// 界面上显示的名字（统一写成 netstat 那种，不管来源）。
    pub fn label(&self) -> String {
        match self {
            Self::Listen => "LISTEN".to_owned(),
            Self::Established => "ESTABLISHED".to_owned(),
            Self::SynSent => "SYN_SENT".to_owned(),
            Self::SynRecv => "SYN_RECV".to_owned(),
            Self::FinWait1 => "FIN_WAIT1".to_owned(),
            Self::FinWait2 => "FIN_WAIT2".to_owned(),
            Self::TimeWait => "TIME_WAIT".to_owned(),
            Self::Close => "CLOSE".to_owned(),
            Self::CloseWait => "CLOSE_WAIT".to_owned(),
            Self::LastAck => "LAST_ACK".to_owned(),
            Self::Closing => "CLOSING".to_owned(),
            Self::Unconn => "UNCONN".to_owned(),
            Self::Other(word) => word.clone(),
        }
    }

    /// 按状态排序时的名次：先监听、已建立，再过渡态，最后收尾态。
    fn rank(&self) -> u8 {
        match self {
            Self::Listen => 0,
            Self::Established => 1,
            Self::Unconn => 2,
            Self::SynSent => 3,
            Self::SynRecv => 4,
            Self::FinWait1 => 5,
            Self::FinWait2 => 6,
            Self::CloseWait => 7,
            Self::Closing => 8,
            Self::LastAck => 9,
            Self::TimeWait => 10,
            Self::Close => 11,
            Self::Other(_) => 12,
        }
    }
}

/// 一端的地址：主机部分（IPv6 不带方括号，可带 `%网卡` 后缀）和端口（`*` 表示任意）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: String,
}

impl Endpoint {
    /// 端口的数值；`*` 或不是数字是 `None`。
    pub fn port_num(&self) -> Option<u16> {
        self.port.parse().ok()
    }

    /// 完整显示：IPv6 加方括号，`%网卡` 后缀留在方括号外（同 ss 的写法）。
    pub fn display(&self) -> String {
        let (addr, scope) = match self.host.split_once('%') {
            Some((addr, scope)) => (addr, Some(scope)),
            None => (self.host.as_str(), None),
        };
        let scope = scope.map(|s| format!("%{s}")).unwrap_or_default();
        if addr.contains(':') {
            format!("[{addr}]{scope}:{}", self.port)
        } else {
            format!("{addr}{scope}:{}", self.port)
        }
    }
}

/// 占着这个连接的进程。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRef {
    pub pid: u32,
    pub name: String,
}

/// 一条连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConn {
    pub proto: NetProto,
    pub local: Endpoint,
    pub remote: Endpoint,
    pub state: ConnState,
    /// 空 = 拿不到（没权限、`/proc` 来源、TIME_WAIT 这类没有属主的连接）。
    pub processes: Vec<ProcRef>,
}

impl NetConn {
    /// `TCP` / `TCP6` / `UDP` / `UDP6`（`6` 看本地或对端地址里有没有冒号）。
    pub fn proto_label(&self) -> &'static str {
        let v6 = self.local.host.contains(':') || self.remote.host.contains(':');
        match (self.proto, v6) {
            (NetProto::Tcp, false) => "TCP",
            (NetProto::Tcp, true) => "TCP6",
            (NetProto::Udp, false) => "UDP",
            (NetProto::Udp, true) => "UDP6",
        }
    }

    /// 监听中：TCP LISTEN，或没连对端的 UDP（和 `ss -l` 一样算监听）。
    pub fn is_listening(&self) -> bool {
        self.state == ConnState::Listen
            || (self.proto == NetProto::Udp && self.state == ConnState::Unconn)
    }

    /// 「进程」一列的文字：`nginx (123, 124)`；多个不同的进程用顿号隔开；没有是 `—`。
    pub fn process_label(&self) -> String {
        if self.processes.is_empty() {
            return "—".to_owned();
        }
        let mut groups: Vec<(&str, Vec<u32>)> = Vec::new();
        for proc in &self.processes {
            match groups.iter_mut().find(|(name, _)| *name == proc.name) {
                Some((_, pids)) => {
                    if !pids.contains(&proc.pid) {
                        pids.push(proc.pid);
                    }
                }
                None => groups.push((proc.name.as_str(), vec![proc.pid])),
            }
        }
        groups
            .iter()
            .map(|(name, pids)| {
                let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
                format!("{name} ({})", pids.join(", "))
            })
            .collect::<Vec<_>>()
            .join("、")
    }
}

/// 一次采集的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionsReport {
    pub source: ConnSource,
    pub rows: Vec<NetConn>,
    /// 连接太多：只留了前 [`CONNECTIONS_MAX_ROWS`] 条（或输出超过字节上限被截了）。
    pub truncated: bool,
    /// 服务器上当前用户的 uid；`0` = root。
    pub uid: Option<u32>,
}

/// 进程列为什么大面积是 `—`（界面在表格上方给一行淡色提示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessHint {
    /// 非 root：看不到其他用户的进程。
    NotRoot,
    /// 来源是 `/proc/net`：本来就没有进程信息。
    NoProcessSource,
}

impl ConnectionsReport {
    pub fn process_hint(&self) -> Option<ProcessHint> {
        if self.source == ConnSource::Proc {
            return Some(ProcessHint::NoProcessSource);
        }
        let lacking = self.rows.iter().any(|row| row.processes.is_empty());
        (self.uid.is_some_and(|uid| uid != 0) && lacking).then_some(ProcessHint::NotRoot)
    }
}

/// 采集失败的原因（给界面直接显示）。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ConnectionsError {
    #[error(
        "这台服务器上没有 ss / netstat，也读不到 /proc/net（可能不是 Linux），无法列出网络连接。"
    )]
    Unsupported,
    #[error("采集超时（5 秒没有结果），可以稍后再点刷新。")]
    Timeout,
    #[error("采集失败：{0}")]
    Exec(String),
}

// ---- 解析 ----

/// 解析脚本的输出。
///
/// 输出里 `@@ZSSH source X` 开一个来源段，失败退到下一个来源时会再来一段，**取最后一段**。
pub fn parse_connections(output: &str) -> Result<ConnectionsReport, ConnectionsError> {
    let mut uid: Option<u32> = None;
    let mut source: Option<ConnSource> = None;
    let mut file: Option<(NetProto, bool)> = None;
    let mut rows: Vec<NetConn> = Vec::new();
    let mut truncated = false;
    // /proc 来源下，有没有见到过任何一个文件的表头（见到 = 至少这台机器有 /proc/net）。
    let mut proc_header_seen = false;

    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(directive) = line.strip_prefix("@@ZSSH ") {
            let mut words = directive.split_whitespace();
            match (words.next(), words.next()) {
                (Some("uid"), value) => uid = value.and_then(|v| v.parse().ok()),
                (Some("source"), Some(name)) => {
                    source = match name {
                        "ss" => Some(ConnSource::Ss),
                        "netstat" => Some(ConnSource::Netstat),
                        "proc" => Some(ConnSource::Proc),
                        _ => source,
                    };
                    // 退到下一个来源：前一个来源的残留不要。
                    rows.clear();
                    truncated = false;
                    file = None;
                }
                (Some("file"), Some(name)) => {
                    file = match name {
                        "tcp" => Some((NetProto::Tcp, false)),
                        "tcp6" => Some((NetProto::Tcp, true)),
                        "udp" => Some((NetProto::Udp, false)),
                        "udp6" => Some((NetProto::Udp, true)),
                        _ => None,
                    };
                }
                _ => {}
            }
            continue;
        }
        let Some(current) = source else { continue };
        if current == ConnSource::Proc && line.trim_start().starts_with("sl ") {
            proc_header_seen = true;
            continue;
        }
        let parsed = match current {
            ConnSource::Ss => parse_ss_line(line),
            ConnSource::Netstat => parse_netstat_line(line),
            ConnSource::Proc => file.and_then(|(proto, v6)| parse_proc_line(line, proto, v6)),
        };
        if let Some(row) = parsed {
            if rows.len() >= CONNECTIONS_MAX_ROWS {
                truncated = true;
            } else {
                rows.push(row);
            }
        }
    }

    let source = source.ok_or(ConnectionsError::Unsupported)?;
    if source == ConnSource::Proc && !proc_header_seen && rows.is_empty() {
        return Err(ConnectionsError::Unsupported);
    }
    Ok(ConnectionsReport {
        source,
        rows,
        truncated,
        uid,
    })
}

/// 解析 `host:port`、`[v6]:port`、`[fe80::1]%eth0:546`、`127.0.0.53%lo:53`、`*:*`、`:::80`。
fn parse_endpoint(text: &str) -> Option<Endpoint> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        // `]` 后面可能是 `%网卡:端口`，也可能直接是 `:端口`。
        let (scope, port) = match after.strip_prefix('%') {
            Some(tail) => {
                let (scope, port) = tail.rsplit_once(':')?;
                (Some(scope), port)
            }
            None => (None, after.strip_prefix(':')?),
        };
        let host = match scope {
            Some(scope) => format!("{host}%{scope}"),
            None => host.to_owned(),
        };
        return Some(Endpoint {
            host,
            port: port.to_owned(),
        });
    }
    let (host, port) = text.rsplit_once(':')?;
    Some(Endpoint {
        host: host.to_owned(),
        port: port.to_owned(),
    })
}

/// 解析 `ss` 的一行：`Netid State Recv-Q Send-Q Local Peer [users:(("name",pid=1,fd=2),…)]`。
fn parse_ss_line(line: &str) -> Option<NetConn> {
    let mut words = line.split_whitespace();
    let netid = words.next()?;
    let proto = match netid {
        "tcp" | "tcp6" => NetProto::Tcp,
        "udp" | "udp6" => NetProto::Udp,
        _ => return None, // 表头（Netid）和别的协议
    };
    let state = ConnState::parse(words.next()?);
    let _recv_q = words.next()?;
    let _send_q = words.next()?;
    let local = parse_endpoint(words.next()?)?;
    let remote = parse_endpoint(words.next()?)?;
    let processes = line
        .split_once("users:(")
        .map(|(_, tail)| parse_ss_users(tail))
        .unwrap_or_default();
    Some(NetConn {
        proto,
        local,
        remote,
        state,
        processes,
    })
}

/// `(("nginx",pid=123,fd=6),("nginx",pid=124,fd=6))` → 每个括号一个进程。
fn parse_ss_users(tail: &str) -> Vec<ProcRef> {
    let mut found = Vec::new();
    let mut rest = tail;
    while let Some(start) = rest.find("(\"") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find("\",pid=") else {
            break;
        };
        let name = &rest[..end];
        rest = &rest[end + 6..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(pid) = digits.parse::<u32>() {
            found.push(ProcRef {
                pid,
                name: name.to_owned(),
            });
        }
    }
    found
}

/// 解析 `netstat -tunap` 的一行：`Proto Recv-Q Send-Q Local Foreign [State] [PID/Program name]`。
///
/// UDP 常常没有 State 列；程序名里可以有空格（`nginx: master process`），所以 PID/名字取行尾整段。
fn parse_netstat_line(line: &str) -> Option<NetConn> {
    let mut words = line.split_whitespace();
    let proto = match words.next()? {
        "tcp" | "tcp6" => NetProto::Tcp,
        "udp" | "udp6" => NetProto::Udp,
        _ => return None, // "Active Internet connections…"、表头
    };
    let _recv_q = words.next()?;
    let _send_q = words.next()?;
    let local = parse_endpoint(words.next()?)?;
    let remote = parse_endpoint(words.next()?)?;
    let rest: Vec<&str> = words.collect();
    let (state, tail) = match rest.split_first() {
        Some((first, tail)) if looks_like_state(first) => (ConnState::parse(first), tail),
        _ => (ConnState::Unconn, rest.as_slice()),
    };
    let processes = parse_netstat_program(&tail.join(" "));
    Some(NetConn {
        proto,
        local,
        remote,
        state,
        processes,
    })
}

/// 全大写字母加下划线的词才当状态（`PID/Program` 以数字开头，`-` 是没有进程）。
fn looks_like_state(word: &str) -> bool {
    word.len() >= 4
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && word.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn parse_netstat_program(text: &str) -> Vec<ProcRef> {
    let text = text.trim();
    let Some((pid, name)) = text.split_once('/') else {
        return Vec::new();
    };
    match pid.parse::<u32>() {
        Ok(pid) if !name.is_empty() => vec![ProcRef {
            pid,
            name: name.to_owned(),
        }],
        _ => Vec::new(),
    }
}

/// 解析 `/proc/net/{tcp,udp}[6]` 的一行：
/// `sl local_address rem_address st tx_queue:rx_queue …`，地址是十六进制、按 32 位字小端存放。
fn parse_proc_line(line: &str, proto: NetProto, v6: bool) -> Option<NetConn> {
    let mut words = line.split_whitespace();
    let slot = words.next()?;
    if !slot.ends_with(':') {
        return None;
    }
    let local = parse_proc_endpoint(words.next()?, v6)?;
    let remote = parse_proc_endpoint(words.next()?, v6)?;
    let code = u8::from_str_radix(words.next()?, 16).ok()?;
    let state = match proto {
        NetProto::Tcp => ConnState::from_proc_tcp(code),
        NetProto::Udp => ConnState::from_proc_udp(code),
    };
    Some(NetConn {
        proto,
        local,
        remote,
        state,
        processes: Vec::new(),
    })
}

fn parse_proc_endpoint(text: &str, v6: bool) -> Option<Endpoint> {
    let (addr, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let host = if v6 {
        proc_ipv6(addr)?
    } else {
        proc_ipv4(addr)?
    };
    // 全 0 的地址 / 端口是「任意」，显示成 `*` 和 ss 一致。
    let port_text = if port == 0 {
        "*".to_owned()
    } else {
        port.to_string()
    };
    Some(Endpoint {
        host,
        port: port_text,
    })
}

fn proc_ipv4(hex: &str) -> Option<String> {
    if hex.len() != 8 {
        return None;
    }
    let word = u32::from_str_radix(hex, 16).ok()?;
    // 内核按本机字节序打印 32 位字：x86 / ARM 小端，低字节是第一段。
    let [a, b, c, d] = word.to_le_bytes();
    Some(format!("{a}.{b}.{c}.{d}"))
}

fn proc_ipv6(hex: &str) -> Option<String> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(8).enumerate() {
        let word = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    Some(Ipv6Addr::from(bytes).to_string())
}

// ---- 采集 ----

/// 在**已有的连接**上采集一次（不重新连接）。
pub async fn collect_connections(
    connection: &Connection,
) -> Result<ConnectionsReport, ConnectionsError> {
    let capture = connection
        .exec_capture(
            &connections_command(),
            CONNECTIONS_TIMEOUT,
            CONNECTIONS_MAX_BYTES,
        )
        .await
        .map_err(|err| ConnectionsError::Exec(err.to_string()))?;
    if capture.timed_out {
        return Err(ConnectionsError::Timeout);
    }
    if capture.stdout.trim().is_empty() {
        // 连 `@@ZSSH` 行都没有：登录 shell 连 sh 都起不来（比如 Windows 服务器）。
        let detail = capture.stderr.lines().next().unwrap_or("").trim();
        return if detail.is_empty() {
            Err(ConnectionsError::Unsupported)
        } else {
            Err(ConnectionsError::Exec(detail.to_owned()))
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
    let mut report = parse_connections(&stdout)?;
    report.truncated |= cut;
    Ok(report)
}

// ---- 筛选 / 搜索 / 排序（界面用，纯函数）----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnFilter {
    All,
    Tcp,
    Udp,
    Listen,
    Established,
}

impl ConnFilter {
    pub const ALL: [ConnFilter; 5] = [
        Self::All,
        Self::Tcp,
        Self::Udp,
        Self::Listen,
        Self::Established,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Tcp => "TCP",
            Self::Udp => "UDP",
            Self::Listen => "监听",
            Self::Established => "已建立",
        }
    }

    pub fn accepts(self, row: &NetConn) -> bool {
        match self {
            Self::All => true,
            Self::Tcp => row.proto == NetProto::Tcp,
            Self::Udp => row.proto == NetProto::Udp,
            Self::Listen => row.is_listening(),
            Self::Established => row.state == ConnState::Established,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Proto,
    /// 按本地端口（数值），同端口再按地址。
    Local,
    Remote,
    State,
    Process,
}

/// 排序设置：`key` 为空 = 保持采集顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SortSpec {
    pub key: Option<SortKey>,
    pub descending: bool,
}

impl SortSpec {
    /// 点一次表头：新的一列升序，再点降序，第三次点回采集顺序。
    pub fn toggled(self, key: SortKey) -> Self {
        match (self.key, self.descending) {
            (Some(current), false) if current == key => Self {
                key: Some(key),
                descending: true,
            },
            (Some(current), true) if current == key => Self::default(),
            _ => Self {
                key: Some(key),
                descending: false,
            },
        }
    }
}

/// 搜索：地址、端口、状态、协议、进程名 / PID 里包含这段文字（不分大小写、忽略首尾空白）。
pub fn matches_query(row: &NetConn, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let hay = [
        row.local.display(),
        row.remote.display(),
        row.state.label(),
        row.proto_label().to_owned(),
        row.processes
            .iter()
            .map(|p| format!("{} {}", p.name, p.pid))
            .collect::<Vec<_>>()
            .join(" "),
    ]
    .join(" ")
    .to_lowercase();
    hay.contains(&query)
}

fn cmp_port(a: &Endpoint, b: &Endpoint) -> Ordering {
    // 没有数值端口（`*`）排在后面。
    match (a.port_num(), b.port_num()) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cmp_rows(key: SortKey, a: &NetConn, b: &NetConn) -> Ordering {
    match key {
        SortKey::Proto => a.proto_label().cmp(b.proto_label()),
        SortKey::Local => {
            cmp_port(&a.local, &b.local).then_with(|| a.local.host.cmp(&b.local.host))
        }
        SortKey::Remote => a
            .remote
            .host
            .cmp(&b.remote.host)
            .then_with(|| cmp_port(&a.remote, &b.remote)),
        SortKey::State => a.state.rank().cmp(&b.state.rank()),
        SortKey::Process => match (a.processes.first(), b.processes.first()) {
            (Some(x), Some(y)) => x
                .name
                .to_lowercase()
                .cmp(&y.name.to_lowercase())
                .then(x.pid.cmp(&y.pid)),
            // 没有进程的沉底。
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    }
}

/// 筛选 + 搜索 + 排序之后要显示的行（下标指向 `rows`）。稳定排序：同值保持采集顺序。
pub fn view_indices(
    rows: &[NetConn],
    filter: ConnFilter,
    query: &str,
    sort: SortSpec,
) -> Vec<usize> {
    let mut shown: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| filter.accepts(row) && matches_query(row, query))
        .map(|(index, _)| index)
        .collect();
    if let Some(key) = sort.key {
        shown.sort_by(|&a, &b| {
            let order = cmp_rows(key, &rows[a], &rows[b]);
            if sort.descending {
                order.reverse()
            } else {
                order
            }
        });
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份 ss -tunapH 的输出（文档保留段地址）：监听、已建立、多进程、IPv6、UDP 无状态、`%网卡`、`*:*`。
    const SS_SAMPLE: &str = "\
@@ZSSH uid 0
@@ZSSH source ss
udp   UNCONN 0      0       127.0.0.53%lo:53         0.0.0.0:*     users:((\"systemd-resolve\",pid=620,fd=13))
udp   UNCONN 0      0                   *:5353             *:*     users:((\"avahi-daemon\",pid=701,fd=12))
udp   UNCONN 0      0   [fe80::1]%eth0:546              [::]:*     users:((\"systemd-network\",pid=588,fd=22))
tcp   LISTEN 0      511           0.0.0.0:80         0.0.0.0:*     users:((\"nginx\",pid=123,fd=6),(\"nginx\",pid=124,fd=6))
tcp   LISTEN 0      4096             [::]:22            [::]:*     users:((\"sshd\",pid=800,fd=4))
tcp   ESTAB  0      0         192.0.2.10:22     198.51.100.7:51234 users:((\"sshd\",pid=1200,fd=4),(\"sshd\",pid=1180,fd=4))
tcp   ESTAB  0      0   [2001:db8::10]:443     [2001:db8::77]:40000
tcp   TIME-WAIT 0    0         192.0.2.10:8080   198.51.100.9:33333
tcp   SYN-SENT  0    1         192.0.2.10:45678  203.0.113.5:443
tcp   FIN-WAIT-2 0   0         192.0.2.10:9000   203.0.113.6:5000
";

    #[test]
    fn ss_lines_parse_with_ipv6_scopes_wildcards_and_multi_process_users() {
        let report = parse_connections(SS_SAMPLE).expect("解析");
        assert_eq!(report.source, ConnSource::Ss);
        assert_eq!(report.uid, Some(0));
        assert_eq!(report.rows.len(), 10);
        let by_local = |text: &str| {
            report
                .rows
                .iter()
                .find(|row| row.local.display() == text)
                .unwrap_or_else(|| panic!("没有 {text}"))
        };
        // 地址里带 % 网卡后缀
        let dns = by_local("127.0.0.53%lo:53");
        assert_eq!(dns.local.host, "127.0.0.53%lo");
        assert_eq!(dns.state, ConnState::Unconn);
        assert_eq!(dns.proto_label(), "UDP");
        assert_eq!(dns.process_label(), "systemd-resolve (620)");
        // *:* 和 [::]:*
        let avahi = by_local("*:5353");
        assert_eq!(avahi.local.host, "*");
        assert_eq!(avahi.remote.port, "*");
        assert_eq!(avahi.remote.port_num(), None);
        let v6udp = by_local("[fe80::1]%eth0:546");
        assert_eq!(v6udp.local.host, "fe80::1%eth0");
        assert_eq!(v6udp.proto_label(), "UDP6");
        assert_eq!(v6udp.remote.display(), "[::]:*");
        // 多进程（同名）合并成一个标签
        let nginx = by_local("0.0.0.0:80");
        assert_eq!(nginx.processes.len(), 2);
        assert_eq!(nginx.process_label(), "nginx (123, 124)");
        assert!(nginx.is_listening());
        // IPv6 监听
        let ssh6 = by_local("[::]:22");
        assert_eq!(ssh6.proto_label(), "TCP6");
        // 已建立，两个同名进程
        let est = by_local("192.0.2.10:22");
        assert_eq!(est.state, ConnState::Established);
        assert_eq!(est.remote.display(), "198.51.100.7:51234");
        assert_eq!(est.process_label(), "sshd (1200, 1180)");
        // 没有 users:（没权限 / 没属主）
        let v6est = by_local("[2001:db8::10]:443");
        assert_eq!(v6est.remote.display(), "[2001:db8::77]:40000");
        assert!(v6est.processes.is_empty());
        assert_eq!(v6est.process_label(), "—");
        // 带连字符的状态名
        assert_eq!(by_local("192.0.2.10:8080").state, ConnState::TimeWait);
        assert_eq!(by_local("192.0.2.10:45678").state, ConnState::SynSent);
        assert_eq!(by_local("192.0.2.10:9000").state, ConnState::FinWait2);
    }

    #[test]
    fn ss_without_dash_h_has_a_header_that_is_skipped_and_different_names_are_listed() {
        let out = "\
@@ZSSH uid 1000
@@ZSSH source ss
Netid State      Recv-Q Send-Q Local Address:Port  Peer Address:Port Process
tcp   LISTEN     0      128    127.0.0.1:631        0.0.0.0:*  users:((\"cupsd\",pid=900,fd=7),(\"cups-browsed\",pid=901,fd=3))
";
        let report = parse_connections(out).expect("解析");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(
            report.rows[0].process_label(),
            "cupsd (900)、cups-browsed (901)"
        );
    }

    #[test]
    fn a_process_name_with_parentheses_and_spaces_survives() {
        let out = "@@ZSSH source ss\ntcp LISTEN 0 1 0.0.0.0:99 0.0.0.0:* users:((\"my (odd) name\",pid=7,fd=3))\n";
        let report = parse_connections(out).expect("解析");
        assert_eq!(report.rows[0].processes[0].name, "my (odd) name");
        assert_eq!(report.rows[0].processes[0].pid, 7);
    }

    const NETSTAT_SAMPLE: &str = "\
@@ZSSH uid 0
@@ZSSH source ss
@@ZSSH source netstat
Active Internet connections (servers and established)
Proto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name
tcp        0      0 0.0.0.0:22              0.0.0.0:*               LISTEN      800/sshd
tcp6       0      0 :::80                   :::*                    LISTEN      123/nginx: master p
tcp        0      0 192.0.2.10:22           198.51.100.7:51234      ESTABLISHED 1200/sshd: root@pts
tcp        0      0 192.0.2.10:8080         198.51.100.9:33333      TIME_WAIT   -
tcp6       0      0 2001:db8::10:443        2001:db8::77:40000      ESTABLISHED -
udp        0      0 127.0.0.53:53           0.0.0.0:*                           620/systemd-resolve
udp6       0      0 fe80::1:546             :::*                                -
udp        0      0 192.0.2.10:123          203.0.113.1:123         ESTABLISHED 650/chronyd
";

    #[test]
    fn netstat_lines_parse_with_spaces_in_program_names_and_stateless_udp() {
        let report = parse_connections(NETSTAT_SAMPLE).expect("解析");
        assert_eq!(report.source, ConnSource::Netstat, "取最后一段来源");
        assert_eq!(report.rows.len(), 8);
        let find = |text: &str| {
            report
                .rows
                .iter()
                .find(|row| row.local.display() == text)
                .unwrap_or_else(|| panic!("没有 {text}"))
        };
        assert_eq!(find("[::]:80").process_label(), "nginx: master p (123)");
        assert_eq!(find("[::]:80").state, ConnState::Listen);
        assert_eq!(
            find("192.0.2.10:22").process_label(),
            "sshd: root@pts (1200)"
        );
        assert_eq!(find("192.0.2.10:8080").state, ConnState::TimeWait);
        assert!(find("192.0.2.10:8080").processes.is_empty());
        // UDP 没有状态列：没连对端 = UNCONN；连了的写着 ESTABLISHED。
        let dns = find("127.0.0.53:53");
        assert_eq!(dns.state, ConnState::Unconn);
        assert_eq!(dns.process_label(), "systemd-resolve (620)");
        assert!(dns.is_listening());
        assert_eq!(find("192.0.2.10:123").state, ConnState::Established);
        // IPv6：最后一个冒号是端口分隔
        let v6 = find("[2001:db8::10]:443");
        assert_eq!(v6.remote.display(), "[2001:db8::77]:40000");
        assert_eq!(find("[fe80::1]:546").remote.display(), "[::]:*");
    }

    /// 内核打印的 /proc/net/tcp：127.0.0.1:53（监听）、192.0.2.10:22 ↔ 198.51.100.7:51234（已建立）。
    const PROC_SAMPLE: &str = "\
@@ZSSH uid 1000
@@ZSSH source proc
@@ZSSH file tcp
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   101        0 17583 1 0000000000000000 100 0 0 10 0
   2: 0A0200C0:0016 076433C6:C822 01 00000000:00000000 02:000A6B2B 00000000     0        0 18000 2 0000000000000000 20 4 29 10 -1
@@ZSSH file tcp6
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 11111 1 0000000000000000 100 0 0 10 0
   1: B80D0120000000000000000010000000:01BB B80D0120000000000000000077000000:9C40 01 00000000:00000000 00:00000000 00000000     0        0 22222 1 0000000000000000 20 4 29 10 -1
@@ZSSH file udp
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  10: 0A0200C0:0123 017100CB:007B 01 00000000:00000000 00:00000000 00000000     0        0 33333 2 0000000000000000 0
  11: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 44444 2 0000000000000000 0
@@ZSSH file udp6
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
";

    #[test]
    fn proc_net_is_decoded_little_endian_with_no_process_info() {
        let report = parse_connections(PROC_SAMPLE).expect("解析");
        assert_eq!(report.source, ConnSource::Proc);
        assert!(report.rows.iter().all(|row| row.processes.is_empty()));
        let find = |text: &str| {
            report
                .rows
                .iter()
                .find(|row| row.local.display() == text)
                .unwrap_or_else(|| panic!("没有 {text}"))
        };
        let dns = find("127.0.0.1:53");
        assert_eq!(dns.state, ConnState::Listen);
        assert_eq!(dns.remote.display(), "0.0.0.0:*");
        let est = find("192.0.2.10:22");
        assert_eq!(est.state, ConnState::Established);
        assert_eq!(est.remote.display(), "198.51.100.7:51234");
        // IPv6：四个小端 32 位字
        let ssh6 = find("[::1]:22");
        assert_eq!(ssh6.proto_label(), "TCP6");
        assert_eq!(ssh6.state, ConnState::Listen);
        let v6 = find("[2001:db8::10]:443");
        assert_eq!(v6.remote.display(), "[2001:db8::77]:40000");
        // UDP：01 = 连了，07 = 没连
        let ntp = find("192.0.2.10:291");
        assert_eq!(ntp.state, ConnState::Established);
        assert_eq!(ntp.proto, NetProto::Udp);
        let wild = find("0.0.0.0:5353");
        assert_eq!(wild.state, ConnState::Unconn);
        assert!(wild.is_listening());
        assert_eq!(report.process_hint(), Some(ProcessHint::NoProcessSource));
    }

    #[test]
    fn a_malformed_proc_line_is_skipped_not_fatal() {
        let out = "@@ZSSH source proc\n@@ZSSH file tcp\n  sl local rem st\n   0: ZZZZ:0035 00000000:0000 0A\n   1: 0100007F:0035 00000000:0000 0A\nrandom words\n";
        let report = parse_connections(out).expect("解析");
        assert_eq!(report.rows.len(), 1);
    }

    #[test]
    fn nothing_usable_means_unsupported() {
        // 没有任何来源（连脚本都没跑起来）
        assert_eq!(parse_connections(""), Err(ConnectionsError::Unsupported));
        assert_eq!(
            parse_connections("garbage\n"),
            Err(ConnectionsError::Unsupported)
        );
        // 退到了 /proc 但一个表头都没读到（不是 Linux）
        let out = "@@ZSSH uid 0\n@@ZSSH source proc\n@@ZSSH file tcp\n@@ZSSH file tcp6\n@@ZSSH file udp\n@@ZSSH file udp6\n";
        assert_eq!(parse_connections(out), Err(ConnectionsError::Unsupported));
    }

    #[test]
    fn an_empty_but_real_listing_is_ok() {
        let out = "@@ZSSH uid 0\n@@ZSSH source ss\n";
        let report = parse_connections(out).expect("空列表也是有效结果");
        assert!(report.rows.is_empty());
        assert!(!report.truncated);
    }

    #[test]
    fn too_many_rows_are_cut_and_flagged() {
        let mut out = String::from("@@ZSSH source ss\n");
        for i in 0..(CONNECTIONS_MAX_ROWS + 25) {
            out.push_str(&format!(
                "tcp ESTAB 0 0 192.0.2.1:{} 198.51.100.1:{}\n",
                1000 + (i % 60000),
                2000 + (i % 60000)
            ));
        }
        let report = parse_connections(&out).expect("解析");
        assert_eq!(report.rows.len(), CONNECTIONS_MAX_ROWS);
        assert!(report.truncated);
    }

    #[test]
    fn the_process_hint_follows_uid_and_missing_processes() {
        let non_root = parse_connections(
            "@@ZSSH uid 1000\n@@ZSSH source ss\ntcp LISTEN 0 1 0.0.0.0:80 0.0.0.0:*\ntcp LISTEN 0 1 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=1,fd=3))\n",
        )
        .expect("解析");
        assert_eq!(non_root.process_hint(), Some(ProcessHint::NotRoot));
        // root 的 TIME_WAIT 本来就没有进程，不该提示。
        let root = parse_connections(
            "@@ZSSH uid 0\n@@ZSSH source ss\ntcp TIME-WAIT 0 0 192.0.2.1:80 198.51.100.1:5\n",
        )
        .expect("解析");
        assert_eq!(root.process_hint(), None);
        // 非 root 但每行都拿到了进程：不提示。
        let all = parse_connections(
            "@@ZSSH uid 1000\n@@ZSSH source ss\ntcp LISTEN 0 1 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=1,fd=3))\n",
        )
        .expect("解析");
        assert_eq!(all.process_hint(), None);
    }

    #[test]
    fn the_command_is_one_read_only_exec_with_balanced_quotes_and_c_locale() {
        let command = connections_command();
        assert!(command.starts_with("exec sh -c '"));
        let stripped = command.replace(r"'\''", "");
        assert_eq!(stripped.matches('\'').count(), 2, "{command}");
        assert!(SCRIPT.contains("LC_ALL=C"));
        assert!(SCRIPT.contains("ss -tunapH"));
        assert!(SCRIPT.contains("netstat -tunap"));
        assert!(SCRIPT.contains("/proc/net/$f"));
        // 只读：不出现任何会改东西的命令。
        for forbidden in [
            "kill", "ss -K", "rm ", "iptables", "tee ", " > /", "ip link",
        ] {
            assert!(!SCRIPT.contains(forbidden), "不该出现 {forbidden}");
        }
    }

    fn sample_rows() -> Vec<NetConn> {
        parse_connections(SS_SAMPLE).expect("解析").rows
    }

    #[test]
    fn filters_pick_protocol_listening_and_established() {
        let rows = sample_rows();
        let count = |filter: ConnFilter| view_indices(&rows, filter, "", SortSpec::default()).len();
        assert_eq!(count(ConnFilter::All), 10);
        assert_eq!(count(ConnFilter::Tcp), 7);
        assert_eq!(count(ConnFilter::Udp), 3);
        // 监听 = TCP LISTEN（2）+ 没连对端的 UDP（3）
        assert_eq!(count(ConnFilter::Listen), 5);
        assert_eq!(count(ConnFilter::Established), 2);
    }

    #[test]
    fn search_matches_address_port_process_state_and_protocol_case_insensitively() {
        let rows = sample_rows();
        let hits = |q: &str| view_indices(&rows, ConnFilter::All, q, SortSpec::default()).len();
        assert_eq!(hits(""), 10);
        assert_eq!(hits("  "), 10, "只有空白等于没搜");
        assert_eq!(hits("NGINX"), 1);
        assert_eq!(hits("1200"), 1, "按 PID");
        assert_eq!(hits("51234"), 1, "按对端端口");
        assert_eq!(hits("2001:db8"), 1, "IPv6 地址");
        assert_eq!(hits("time_wait"), 1);
        assert_eq!(hits("udp6"), 1);
        assert_eq!(hits("nothing-matches-this"), 0);
        // 搜索和筛选叠加
        let both = view_indices(&rows, ConnFilter::Listen, "sshd", SortSpec::default());
        assert_eq!(both.len(), 1);
    }

    #[test]
    fn sorting_by_port_state_and_process_is_stable_and_reversible() {
        let rows = sample_rows();
        let order = |key: SortKey, descending: bool| {
            view_indices(
                &rows,
                ConnFilter::All,
                "",
                SortSpec {
                    key: Some(key),
                    descending,
                },
            )
        };
        let ports: Vec<String> = order(SortKey::Local, false)
            .into_iter()
            .map(|i| rows[i].local.port.clone())
            .collect();
        assert_eq!(
            ports,
            ["22", "22", "53", "80", "443", "546", "5353", "8080", "9000", "45678"],
            "端口按数值排，不是按字典序"
        );
        let desc = order(SortKey::Local, true);
        assert_eq!(rows[desc[0]].local.port, "45678");
        // 状态：监听在前
        let by_state = order(SortKey::State, false);
        assert_eq!(rows[by_state[0]].state, ConnState::Listen);
        assert_eq!(rows[by_state[1]].state, ConnState::Listen);
        assert_eq!(
            rows[*by_state.last().unwrap_or(&0)].state,
            ConnState::TimeWait
        );
        // 进程：有名字的按字母序在前，没有进程的沉底
        let by_proc = order(SortKey::Process, false);
        assert_eq!(rows[by_proc[0]].processes[0].name, "avahi-daemon");
        assert!(rows[*by_proc.last().unwrap_or(&0)].processes.is_empty());
        // 不排序 = 采集顺序
        let plain = view_indices(&rows, ConnFilter::All, "", SortSpec::default());
        assert_eq!(plain, (0..rows.len()).collect::<Vec<_>>());
    }

    #[test]
    fn clicking_a_header_cycles_ascending_descending_and_back_to_collection_order() {
        let spec = SortSpec::default().toggled(SortKey::State);
        assert_eq!((spec.key, spec.descending), (Some(SortKey::State), false));
        let spec = spec.toggled(SortKey::State);
        assert_eq!((spec.key, spec.descending), (Some(SortKey::State), true));
        let spec = spec.toggled(SortKey::State);
        assert_eq!(spec, SortSpec::default());
        // 换一列：从升序重新开始
        let spec = SortSpec {
            key: Some(SortKey::State),
            descending: true,
        }
        .toggled(SortKey::Process);
        assert_eq!((spec.key, spec.descending), (Some(SortKey::Process), false));
    }

    #[test]
    fn twenty_thousand_rows_filter_search_and_sort_without_trouble() {
        let mut out = String::from(
            "@@ZSSH uid 0
@@ZSSH source ss
",
        );
        for i in 0..CONNECTIONS_MAX_ROWS {
            out.push_str(&format!(
                "tcp ESTAB 0 0 192.0.2.10:{} 198.51.100.{}:{} users:((\"app-worker\",pid={},fd=9))
",
                1024 + (i % 60_000),
                1 + (i % 250),
                2000 + (i % 50_000),
                800 + (i % 400)
            ));
        }
        let report = parse_connections(&out).expect("解析");
        assert_eq!(report.rows.len(), CONNECTIONS_MAX_ROWS);
        let started = std::time::Instant::now();
        let hits = view_indices(
            &report.rows,
            ConnFilter::Established,
            "198.51.100.7:",
            SortSpec::default().toggled(SortKey::Process),
        );
        assert!(!hits.is_empty());
        // 调试构建也该在一两秒内（每次键入都会重算一遍）。
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn endpoint_display_is_complete_for_ipv6_and_keeps_the_scope_outside_the_brackets() {
        let e = parse_endpoint("[2001:db8:0:0:1:2:3:4]:8443").expect("解析");
        assert_eq!(e.display(), "[2001:db8:0:0:1:2:3:4]:8443");
        assert_eq!(
            parse_endpoint("[fe80::1]%eth0:546")
                .expect("解析")
                .display(),
            "[fe80::1]%eth0:546"
        );
        assert!(parse_endpoint("").is_none());
        assert!(parse_endpoint("no-port").is_none());
    }
}
