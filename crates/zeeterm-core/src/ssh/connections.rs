//! 服务器上的网络连接列表（概览面板「网卡」一节的"所有连接"）。
//!
//! 在已有连接上开一个 exec 通道（不新建 SSH 连接、不往终端里打字），跑一段只读脚本：
//! 优先 `ss -tunapH`，没有 `ss`（或它整个失败）退到 `netstat -tunap`，再不行读
//! `/proc/net/{tcp,tcp6,udp,udp6}`。脚本用 `@@ZSSH …` 行标出来源，解析全在这里（纯函数，
//! 三种格式各有夹具测试），界面只做展示、筛选、排序。
//!
//! 只读：不结束进程、不断开连接。命令里强制 `LC_ALL=C`，这样状态名、表头不会被翻译。
//!
//! `ss` 来源还会在主列表之后追加 `ss -tin` 的一段：内核给每条 TCP 连接记的累计收发字节数，不需要 root。
//! 两次采集之间字节数的差除以时间差就是每条连接的实时速率（[`compute_rates`]）。netstat / `/proc/net`
//! 没有这个数据，速率不可用。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

use super::connection::Connection;
use super::cwd::shell_quote;

/// 单次采集的时限：超时就放弃这一次，终端照常。
pub const CONNECTIONS_TIMEOUT: Duration = Duration::from_secs(5);
/// 脚本输出的字节上限（超过只取前面的整行）。
///
/// 主列表每条约 100~150 字节，`ss -tin` 的每条连接还要多两行（状态行加几百字节的信息行）。两万条连接时
/// 主列表约 2~3 MB、tcpinfo 段远超 4 MB，所以 tcpinfo 段一定放在主列表**后面**：撞到上限只会截掉它的尾巴
/// （没对上的行就没有速率），主列表不会因此变少。
pub const CONNECTIONS_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 最多解析这么多行连接，超过的丢掉并标记 `truncated`。
pub const CONNECTIONS_MAX_ROWS: usize = 20_000;

/// 服务器上跑的脚本。
///
/// 用 `if/&&` 而不是 `||` 串起三个来源：前一个整个失败（没有命令、netlink 被容器拦了）才退到下一个；
/// `ss -tunapH` 不认 `-H`（老 iproute2）时再试一次不带 `-H` 的，表头由解析器跳过。
///
/// 主 `ss` 的退出码先记进 `rc`，之后才跑 `date` 和 `ss -tin`：tcpinfo 这一段失败（老内核、被拦）
/// 不能影响 netstat / `/proc` 的回退判断，所以 `rc` 只认主列表那条。
///
/// `ss -tin` 的信息行每条几百字节、只用得上其中的字节计数：有 awk 就在服务器上把信息行压成
/// `\t- bytes_…` （`-` 留着表示"信息行有内容"，计数全缺时据此判成 0，见 `feed_tcpinfo`），
/// 两万条连接从约 9 MB 降到 1 MB 上下，慢线路上不容易撞到采集时限；没有 awk 就原样输出。
const SCRIPT: &str = r#"LC_ALL=C
export LC_ALL
echo "@@ZSSH uid $(id -u 2>/dev/null)"
echo "@@ZSSH ssh_connection $SSH_CONNECTION"
rc=127
if command -v ss >/dev/null 2>&1; then
  echo "@@ZSSH source ss"
  ss -tunapH 2>/dev/null || ss -tunap 2>/dev/null
  rc=$?
  echo "@@ZSSH now $(date +%s%N 2>/dev/null)"
  echo "@@ZSSH tcpinfo"
  if command -v awk >/dev/null 2>&1; then
    { ss -tinH state connected 2>/dev/null || ss -tin state connected 2>/dev/null; } | awk '/^[ \t]/ { if (NF == 0) { print "\t"; next } o = "\t-"; for (i = 1; i <= NF; i++) if ($i ~ /^bytes_(acked|sent|received):/) o = o " " $i; print o; next } { print }'
  else
    ss -tinH state connected 2>/dev/null || ss -tin state connected 2>/dev/null
  fi
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

/// 一条 TCP 连接从建立起的累计字节数（内核 `tcp_info`，`ss -tin` 读出来）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpBytes {
    /// 已发送：优先 `bytes_acked`（对端确认收到的，不含重传），没有就退 `bytes_sent`（含重传，内核 >= 4.19）。
    pub sent: u64,
    /// 已接收：`bytes_received`。
    pub received: u64,
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
    /// 累计收发字节数；UDP、非 `ss` 来源、内核不提供（< 4.1）或没对上 tcpinfo 里的那一行时是 `None`。
    pub bytes: Option<TcpBytes>,
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

    /// 占着这条连接的进程，同一个 PID 只留一项（`ss` 对每个文件描述符各给一项）。
    pub fn unique_processes(&self) -> Vec<ProcRef> {
        let mut seen: Vec<ProcRef> = Vec::new();
        for proc in &self.processes {
            if !seen.iter().any(|known| known.pid == proc.pid) {
                seen.push(proc.clone());
            }
        }
        seen
    }

    /// 「复制整行」：协议、本地、远端、状态、进程，制表符分隔（和表格上看到的一样）。
    pub fn tsv_line(&self) -> String {
        [
            self.proto_label().to_owned(),
            self.local.display(),
            self.remote.display(),
            self.state.label(),
            self.process_label(),
        ]
        .join("	")
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
    /// 采集用的这条 SSH 连接自己（服务器 sshd 设置的 `$SSH_CONNECTION`）：ZeeTerm 正在用的那条连接，
    /// 不能被断开（V2-U21）。没有这个环境变量（非 OpenSSH、被登录 shell 清掉）时是 `None`。
    pub ssh_connection: Option<SshConnection>,
    /// 服务器上采集 tcpinfo 时的时间（自 1970 年起的纳秒）。算速率的时间差优先用它（不受网络抖动影响）；
    /// 没有（busybox 的 `date` 不认 `%N`、非 `ss` 来源）时是 `None`，调用方改用本地收到结果的时间。
    pub server_time_ns: Option<u64>,
    /// 输出被字节上限截断时落在 tcpinfo 段里：主列表是完整的，只是靠后的连接没有字节数。
    pub counters_truncated: bool,
}

/// `$SSH_CONNECTION`：`客户端 IP 客户端端口 服务端 IP 服务端端口`，端点都是 sshd 看到的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshConnection {
    pub client: Endpoint,
    pub server: Endpoint,
}

/// 解析 `$SSH_CONNECTION` 的值。格式不对（不是四段、地址不是 IP、端口不是 1..=65535 的数字）一律是 `None`：
/// 认不出就不上锁，但也绝不把半截数据当成"自己的连接"去匹配别的行。
pub fn parse_ssh_connection(value: &str) -> Option<SshConnection> {
    let mut words = value.split_whitespace();
    let (client_ip, client_port, server_ip, server_port) =
        (words.next()?, words.next()?, words.next()?, words.next()?);
    if words.next().is_some() {
        return None;
    }
    let endpoint = |ip: &str, port: &str| -> Option<Endpoint> {
        let ip: IpAddr = ip.parse().ok()?;
        let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
        Some(Endpoint {
            host: ip.to_string(),
            port: port.to_string(),
        })
    };
    Some(SshConnection {
        client: endpoint(client_ip, client_port)?,
        server: endpoint(server_ip, server_port)?,
    })
}

/// 两个端点是不是同一个：端口相同；地址去掉 `%网卡`、按 IP 比（IPv4-mapped IPv6 当 IPv4），不是 IP 时按字面比。
pub fn same_endpoint(a: &Endpoint, b: &Endpoint) -> bool {
    a.port == b.port && canon_host(&a.host) == canon_host(&b.host)
}

/// `same_endpoint` 比较用的规范写法：去掉 `%网卡`，IPv4-mapped IPv6 写成 IPv4，其余按 IP 规范化。
fn canon_host(host: &str) -> String {
    let bare = host.split_once('%').map_or(host, |(addr, _)| addr);
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        },
        Ok(ip) => ip.to_string(),
        Err(_) => bare.to_ascii_lowercase(),
    }
}

/// 一条连接的（本地, 远端）身份，写法差异（`%网卡`、IPv4-mapped）不影响：把 tcpinfo 的字节数挂到主列表、
/// 以及前后两次快照对账都用它。
type ConnKey = (String, String);

fn conn_key(local: &Endpoint, remote: &Endpoint) -> ConnKey {
    (
        format!("{}|{}", canon_host(&local.host), local.port),
        format!("{}|{}", canon_host(&remote.host), remote.port),
    )
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
    /// 这一行是不是 ZeeTerm 正在使用的那条 SSH 连接（TCP，本地 = sshd 的地址，对端 = 客户端）。
    pub fn is_own_connection(&self, row: &NetConn) -> bool {
        let Some(own) = &self.ssh_connection else {
            return false;
        };
        row.proto == NetProto::Tcp
            && same_endpoint(&row.local, &own.server)
            && same_endpoint(&row.remote, &own.client)
    }

    /// 速率列为什么会是 `—`：来源不是 `ss`（netstat / `/proc/net` 没有字节计数），或者 `ss` 来源下有已建立的
    /// TCP 连接却一条都没拿到字节数（内核 < 4.1、`ss -tin` 被拦）。只有监听 / UDP 时本来就没东西可算，不提示。
    pub fn counters_unavailable(&self) -> bool {
        if self.source != ConnSource::Ss {
            return true;
        }
        let has_connected = self
            .rows
            .iter()
            .any(|row| row.proto == NetProto::Tcp && row.state == ConnState::Established);
        has_connected && self.rows.iter().all(|row| row.bytes.is_none())
    }

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
    parse_output(output).map(|(report, _)| report)
}

/// [`parse_connections`] 加一个"输出里有没有读到最后一个来源的 tcpinfo 段"：输出被字节上限截断时，
/// 靠它分清截到的是主列表还是只截了 tcpinfo 的尾巴。
fn parse_output(output: &str) -> Result<(ConnectionsReport, bool), ConnectionsError> {
    let mut uid: Option<u32> = None;
    let mut ssh_connection: Option<SshConnection> = None;
    let mut source: Option<ConnSource> = None;
    let mut file: Option<(NetProto, bool)> = None;
    let mut rows: Vec<NetConn> = Vec::new();
    let mut truncated = false;
    // tcpinfo 段：读到 `@@ZSSH tcpinfo` 之后的行都归它，直到下一个来源段开始。
    let mut in_tcpinfo = false;
    let mut tcpinfo_seen = false;
    let mut server_time_ns: Option<u64> = None;
    let mut pending: Option<ConnKey> = None;
    let mut counters: HashMap<ConnKey, TcpBytes> = HashMap::new();
    // 信息行有内容但没有任何字节计数的连接（见 `feed_tcpinfo`）。
    let mut quiet: HashSet<ConnKey> = HashSet::new();
    // /proc 来源下，有没有见到过任何一个文件的表头（见到 = 至少这台机器有 /proc/net）。
    let mut proc_header_seen = false;

    for raw in output.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(directive) = line.strip_prefix("@@ZSSH ") {
            let mut words = directive.split_whitespace();
            match (words.next(), words.next()) {
                (Some("uid"), value) => uid = value.and_then(|v| v.parse().ok()),
                (Some("ssh_connection"), _) => {
                    let value = directive.trim_start_matches("ssh_connection");
                    ssh_connection = parse_ssh_connection(value);
                }
                (Some("source"), Some(name)) => {
                    source = match name {
                        "ss" => Some(ConnSource::Ss),
                        "netstat" => Some(ConnSource::Netstat),
                        "proc" => Some(ConnSource::Proc),
                        _ => source,
                    };
                    // 退到下一个来源：前一个来源的残留不要（字节数和服务器时间也只属于 ss 那一段）。
                    rows.clear();
                    truncated = false;
                    file = None;
                    in_tcpinfo = false;
                    tcpinfo_seen = false;
                    server_time_ns = None;
                    pending = None;
                    counters.clear();
                    quiet.clear();
                }
                (Some("now"), value) => server_time_ns = value.and_then(parse_server_time),
                (Some("tcpinfo"), _) => {
                    in_tcpinfo = true;
                    tcpinfo_seen = true;
                    pending = None;
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
        if in_tcpinfo {
            feed_tcpinfo(line, &mut pending, &mut counters, &mut quiet);
            continue;
        }
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
    // 字节数只挂在 TCP 行上；`counters` 只有 ss 来源才可能非空。
    if !counters.is_empty() {
        // 别的连接有计数，说明这台机器的内核给计数：没打印计数的就是收发都还是 0，不是"不知道"。
        // 否则刚建立的连接第一次出现字节的那个间隔会显示成"—"而不是速率。
        for key in quiet.drain() {
            counters.entry(key).or_insert(TcpBytes {
                sent: 0,
                received: 0,
            });
        }
        for row in rows.iter_mut().filter(|row| row.proto == NetProto::Tcp) {
            row.bytes = counters.get(&conn_key(&row.local, &row.remote)).copied();
        }
    }
    let report = ConnectionsReport {
        source,
        rows,
        truncated,
        uid,
        ssh_connection,
        server_time_ns,
        counters_truncated: false,
    };
    Ok((report, tcpinfo_seen))
}

/// `date +%s%N` 的输出：纯数字且位数够（19 位，自 1970 年的纳秒）。busybox 等不认 `%N` 时会输出
/// `1700000000N` 或只有秒（10 位），那样算出的时间差是错的，一律当作没有。
fn parse_server_time(text: &str) -> Option<u64> {
    if text.len() < 18 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// 吃 tcpinfo 段的一行。`ss -tin` 每条连接两行：状态行（`State Recv-Q Send-Q 本地 远端`，**没有** netid 那一列）
/// 和以空白开头的信息行（`key:value` 串，可能只有空白）。老 iproute2 不认 `-H` 时第一行是表头，
/// 它的 Recv-Q 不是数字，状态行解析不过，自然跳过。
fn feed_tcpinfo(
    line: &str,
    pending: &mut Option<ConnKey>,
    counters: &mut HashMap<ConnKey, TcpBytes>,
    quiet: &mut HashSet<ConnKey>,
) {
    if line.starts_with(char::is_whitespace) {
        // 信息行也可能只有空白（TIME-WAIT 之类没有信息的连接）：照样消耗掉前面那条状态行，解析不出字节数就算了。
        let Some(key) = pending.take() else {
            return;
        };
        match parse_tcp_bytes(line) {
            Some(bytes) => {
                counters.entry(key).or_insert(bytes);
            }
            // 信息行有内容、却一个字节计数都没有：要么内核太老不给，要么收发都还是 0（iproute2 不打印 0）。
            // 先记下，整段读完再按"这台机器给不给计数"定。
            None if !line.trim().is_empty() => {
                quiet.insert(key);
            }
            None => {}
        }
    } else {
        *pending = parse_tcpinfo_head(line);
    }
}

fn parse_tcpinfo_head(line: &str) -> Option<ConnKey> {
    let mut words = line.split_whitespace().peekable();
    // 个别版本会带 netid 一列：有就跳过。
    if matches!(words.peek(), Some(&"tcp" | &"tcp6")) {
        words.next();
    }
    let _state = words.next()?;
    words.next()?.parse::<u64>().ok()?;
    words.next()?.parse::<u64>().ok()?;
    let local = parse_endpoint(words.next()?)?;
    let remote = parse_endpoint(words.next()?)?;
    Some(conn_key(&local, &remote))
}

/// 从信息行取累计字节数。iproute2 只打印非 0 的计数，所以只要见到其中一个，缺的那个就是 0；
/// 一个都没有（内核 < 4.1、信息行为空）才算这条没有字节数。
fn parse_tcp_bytes(info: &str) -> Option<TcpBytes> {
    let (mut acked, mut sent, mut received) = (None, None, None);
    for word in info.split_whitespace() {
        let Some((key, value)) = word.split_once(':') else {
            continue;
        };
        let slot = match key {
            "bytes_acked" => &mut acked,
            "bytes_sent" => &mut sent,
            "bytes_received" => &mut received,
            _ => continue,
        };
        if let Ok(value) = value.parse::<u64>() {
            *slot = Some(value);
        }
    }
    let sent = acked.or(sent);
    if sent.is_none() && received.is_none() {
        return None;
    }
    Some(TcpBytes {
        sent: sent.unwrap_or(0),
        received: received.unwrap_or(0),
    })
}

/// 解析 `host:port`、`[v6]:port`、`[fe80::1]%eth0:546`、`127.0.0.53%lo:53`、`*:*`、`:::80`。
pub(crate) fn parse_endpoint(text: &str) -> Option<Endpoint> {
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
        bytes: None,
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
        bytes: None,
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
        bytes: None,
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
    report_from_output(capture.stdout, capture.stdout_truncated_bytes > 0)
}

/// 把采集到的输出解析成报告；`cut` = 输出被字节上限截断过。
fn report_from_output(
    mut stdout: String,
    cut: bool,
) -> Result<ConnectionsReport, ConnectionsError> {
    if cut {
        // 被字节上限截断：最后一行可能只有半截，丢掉。
        if let Some(end) = stdout.rfind('\n') {
            stdout.truncate(end);
        }
    }
    let (mut report, tcpinfo_seen) = parse_output(&stdout)?;
    if cut {
        // tcpinfo 段在主列表后面：截断落在里面时，主列表是完整的（不能报"连接太多"），
        // 只是靠后的连接没有字节数。
        if tcpinfo_seen {
            report.counters_truncated = true;
        } else {
            report.truncated = true;
        }
    }
    Ok(report)
}

// ---- 速率（纯函数）----

/// 两次快照的时间差小于它就不算速率：计数是整字节、时间有抖动，太短的区间算出来的数没意义。
pub const RATE_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// 一条 TCP 连接两次采集之间的平均速率，字节 / 秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnRate {
    /// 上行（本机发出）。
    pub up: u64,
    /// 下行（本机收到）。
    pub down: u64,
}

/// 用前后两次快照算每条连接的速率；结果与 `cur.rows` 一一对应，`None` = 算不出（界面写 `—`）。
///
/// 时间差：两次都有服务器时间就用它（不含网络往返的抖动），否则用 `local_elapsed`（界面两次收到结果的间隔）。
/// 时间差 <= 0 或小于 [`RATE_MIN_INTERVAL`] 整批 `None`。单条连接在下列情况是 `None`：上次没有这条连接
/// （第一次出现）、上次或这次没有字节数、任何一个计数变小（连接被换掉、端口复用）。
pub fn compute_rates(
    prev: &ConnectionsReport,
    cur: &ConnectionsReport,
    local_elapsed: Duration,
) -> Vec<Option<ConnRate>> {
    let nothing = || vec![None; cur.rows.len()];
    let elapsed_ns = match (prev.server_time_ns, cur.server_time_ns) {
        (Some(before), Some(after)) => {
            if after <= before {
                return nothing();
            }
            u128::from(after - before)
        }
        _ => local_elapsed.as_nanos(),
    };
    if elapsed_ns < RATE_MIN_INTERVAL.as_nanos() {
        return nothing();
    }
    let before: HashMap<ConnKey, TcpBytes> = prev
        .rows
        .iter()
        .filter_map(|row| Some((conn_key(&row.local, &row.remote), row.bytes?)))
        .collect();
    let per_second = |delta: u64| -> u64 {
        u64::try_from(u128::from(delta) * 1_000_000_000 / elapsed_ns).unwrap_or(u64::MAX)
    };
    cur.rows
        .iter()
        .map(|row| {
            let now = row.bytes?;
            let was = before.get(&conn_key(&row.local, &row.remote))?;
            if now.sent < was.sent || now.received < was.received {
                return None;
            }
            Some(ConnRate {
                up: per_second(now.sent - was.sent),
                down: per_second(now.received - was.received),
            })
        })
        .collect()
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
    /// 上行速率（字节 / 秒）：没有速率的永远排最后。
    Up,
    /// 下行速率，规则同上。
    Down,
    Process,
}

impl SortKey {
    /// 按速率排的两列：第一次点是从大到小（要找的是最忙的连接），没有速率的行不论升降都排最后。
    pub fn is_rate(self) -> bool {
        matches!(self, Self::Up | Self::Down)
    }
}

/// 排序设置：`key` 为空 = 保持采集顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SortSpec {
    pub key: Option<SortKey>,
    pub descending: bool,
}

impl SortSpec {
    /// 点一次表头：新的一列升序（速率列先降序），再点反向，第三次点回采集顺序。
    pub fn toggled(self, key: SortKey) -> Self {
        let first_descending = key.is_rate();
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
        // 速率列在 `view_indices` 里单独比（要用速率表，且没有速率的要沉底）。
        SortKey::Up | SortKey::Down => Ordering::Equal,
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

/// 速率列的比较：没有速率的永远排最后（不论升降序）。
fn cmp_rate(key: SortKey, a: Option<ConnRate>, b: Option<ConnRate>, descending: bool) -> Ordering {
    let pick = |rate: ConnRate| {
        if key == SortKey::Up {
            rate.up
        } else {
            rate.down
        }
    };
    match (a.map(pick), b.map(pick)) {
        (Some(x), Some(y)) if descending => y.cmp(&x),
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// 筛选 + 搜索 + 排序之后要显示的行（下标指向 `rows`）。稳定排序：同值保持采集顺序。
///
/// `rates` 与 `rows` 一一对应（[`compute_rates`] 的结果）；比 `rows` 短或为空时缺的当作没有速率。
pub fn view_indices(
    rows: &[NetConn],
    rates: &[Option<ConnRate>],
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
        let rate_of = |index: usize| rates.get(index).copied().flatten();
        shown.sort_by(|&a, &b| {
            if key.is_rate() {
                return cmp_rate(key, rate_of(a), rate_of(b), sort.descending);
            }
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
    fn ssh_connection_parses_four_fields_and_rejects_anything_odd() {
        let own = parse_ssh_connection("198.51.100.7 51234 192.0.2.10 22").expect("解析");
        assert_eq!(own.client.display(), "198.51.100.7:51234");
        assert_eq!(own.server.display(), "192.0.2.10:22");
        let v6 = parse_ssh_connection(
            "2001:db8::77 40000 2001:db8::10 22
",
        )
        .expect("IPv6");
        assert_eq!(v6.client.display(), "[2001:db8::77]:40000");
        // 没有 / 空 / 三段 / 五段 / 不是 IP / 端口越界或不是数字：都不认。
        for odd in [
            "",
            "   ",
            "198.51.100.7 51234 192.0.2.10",
            "198.51.100.7 51234 192.0.2.10 22 extra",
            "not-an-ip 51234 192.0.2.10 22",
            "198.51.100.7 0 192.0.2.10 22",
            "198.51.100.7 65536 192.0.2.10 22",
            "198.51.100.7 http 192.0.2.10 22",
            "198.51.100.7 -1 192.0.2.10 22",
        ] {
            assert_eq!(parse_ssh_connection(odd), None, "{odd:?}");
        }
    }

    #[test]
    fn the_report_carries_ssh_connection_and_flags_only_the_matching_row_as_own() {
        let out = "@@ZSSH uid 0
@@ZSSH ssh_connection 198.51.100.7 51234 192.0.2.10 22
@@ZSSH source ss
tcp   ESTAB  0 0 192.0.2.10:22     198.51.100.7:51234 users:((\"sshd\",pid=1200,fd=4))
tcp   ESTAB  0 0 192.0.2.10:22     198.51.100.8:51234 users:((\"sshd\",pid=1300,fd=4))
tcp   ESTAB  0 0 192.0.2.10:22     198.51.100.7:51235 users:((\"sshd\",pid=1400,fd=4))
tcp   ESTAB  0 0 192.0.2.10:2222   198.51.100.7:51234
tcp   LISTEN 0 0 0.0.0.0:22       0.0.0.0:*
";
        let report = parse_connections(out).expect("解析");
        assert!(report.ssh_connection.is_some());
        let own: Vec<bool> = report
            .rows
            .iter()
            .map(|row| report.is_own_connection(row))
            .collect();
        // 只有四元组完全相同的那一行；别的客户端 / 别的端口 / 别的服务端口 / 监听都不是。
        assert_eq!(own, [true, false, false, false, false]);
    }

    #[test]
    fn own_connection_matching_ignores_scope_and_v4_mapped_forms_but_never_matches_without_the_variable(
    ) {
        let out = "@@ZSSH uid 0
@@ZSSH ssh_connection 198.51.100.7 51234 192.0.2.10 22
@@ZSSH source ss
tcp   ESTAB  0 0 [::ffff:192.0.2.10]:22   [::ffff:198.51.100.7]:51234
";
        let report = parse_connections(out).expect("解析");
        assert!(
            report.is_own_connection(&report.rows[0]),
            "IPv4-mapped 写法"
        );
        let v6 = "@@ZSSH uid 0
@@ZSSH ssh_connection 2001:db8::77 40000 2001:db8::10 22
@@ZSSH source ss
tcp   ESTAB  0 0 [2001:db8::10]%eth0:22   [2001:db8::77]:40000
";
        let report = parse_connections(v6).expect("解析");
        assert!(
            report.is_own_connection(&report.rows[0]),
            "scope 后缀不影响"
        );
        // 没有变量 / 格式怪：没有任何一行算自己的。
        for header in [
            "",
            "@@ZSSH ssh_connection 
",
            "@@ZSSH ssh_connection a b c d
",
        ] {
            let out = format!(
                "@@ZSSH uid 0
{header}@@ZSSH source ss
tcp ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234
"
            );
            let report = parse_connections(&out).expect("解析");
            assert_eq!(report.ssh_connection, None);
            assert!(!report.is_own_connection(&report.rows[0]));
        }
    }

    #[test]
    fn unique_processes_and_the_copied_line_follow_the_table() {
        let report = parse_connections(SS_SAMPLE).expect("解析");
        let est = report
            .rows
            .iter()
            .find(|row| row.local.display() == "192.0.2.10:22")
            .expect("行");
        let pids: Vec<u32> = est.unique_processes().iter().map(|p| p.pid).collect();
        assert_eq!(pids, [1200, 1180]);
        assert_eq!(
            est.tsv_line(),
            "TCP	192.0.2.10:22	198.51.100.7:51234	ESTABLISHED	sshd (1200, 1180)"
        );
        // 同一个 PID 出现在多个文件描述符里只算一项。
        let dup = NetConn {
            processes: vec![
                ProcRef {
                    pid: 7,
                    name: "a".into(),
                },
                ProcRef {
                    pid: 7,
                    name: "a".into(),
                },
            ],
            ..est.clone()
        };
        assert_eq!(dup.unique_processes().len(), 1);
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
        assert!(SCRIPT.contains("@@ZSSH ssh_connection $SSH_CONNECTION"));
        // tcpinfo 段：在主 ss 的退出码记下之后、netstat 回退之前；退出码只认主列表那条。
        let at = |needle: &str| {
            SCRIPT
                .find(needle)
                .unwrap_or_else(|| panic!("没有 {needle}"))
        };
        assert!(at("rc=$?") < at("@@ZSSH now $(date +%s%N"));
        assert!(at("@@ZSSH now") < at("@@ZSSH tcpinfo"));
        assert!(at("@@ZSSH tcpinfo") < at("ss -tinH state connected"));
        assert!(at("ss -tin state connected") < at("@@ZSSH source netstat"));
        assert_eq!(SCRIPT.matches("rc=$?").count(), 2, "tcpinfo 不能再改 rc");
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
        let count =
            |filter: ConnFilter| view_indices(&rows, &[], filter, "", SortSpec::default()).len();
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
        let hits =
            |q: &str| view_indices(&rows, &[], ConnFilter::All, q, SortSpec::default()).len();
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
        let both = view_indices(&rows, &[], ConnFilter::Listen, "sshd", SortSpec::default());
        assert_eq!(both.len(), 1);
    }

    #[test]
    fn sorting_by_port_state_and_process_is_stable_and_reversible() {
        let rows = sample_rows();
        let order = |key: SortKey, descending: bool| {
            view_indices(
                &rows,
                &[],
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
        let plain = view_indices(&rows, &[], ConnFilter::All, "", SortSpec::default());
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
            &[],
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

    // ---- 字节计数与速率 ----

    /// 把若干行用换行连起来（信息行以制表符开头，不能靠字符串续行写）。
    fn out(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    const INFO: &str = "\t bbr wscale:7,7 rto:216 rtt:12.249/0.373 ato:40 mss:1448 pmtu:1500 rcvmss:1448 advmss:1448 cwnd:10 bytes_sent:148915 bytes_retrans:4008 bytes_acked:144907 bytes_received:6664002 segs_out:84867 segs_in:90000 data_segs_out:80000 send 9.46Mbps lastsnd:8 lastrcv:8 lastack:8 pacing_rate 18.9Mbps delivery_rate 9.9Mbps delivered:84000 busy:1000ms rcv_rtt:12 rcv_space:64000 minrtt:12.1";

    fn tcpinfo_sample() -> String {
        out(&[
            "@@ZSSH uid 0",
            "@@ZSSH source ss",
            "tcp   ESTAB  0      0         192.0.2.10:443     198.51.100.7:54532 users:((\"web-server\",pid=1,fd=3))",
            "tcp   ESTAB  0      0     [2001:db8::10]:443     [2001:db8::77]:40000",
            "tcp   ESTAB  0      0 [::ffff:192.0.2.10]:8443 [::ffff:198.51.100.9]:41000",
            "tcp   TIME-WAIT 0   0         192.0.2.10:60152   198.51.100.8:443",
            "tcp   ESTAB  0      0    127.0.0.53%lo:53         127.0.0.1:41234",
            "udp   UNCONN 0      0               *:5353             *:*",
            "@@ZSSH now 1700000000123456789",
            "@@ZSSH tcpinfo",
            "ESTAB     0      0      192.0.2.10:443     198.51.100.7:54532",
            INFO,
            "ESTAB     0      0      [2001:db8::10]:443     [2001:db8::77]:40000",
            "\t cubic wscale:7,7 bytes_sent:500 bytes_acked:400 bytes_received:300 segs_out:3",
            // 同一条连接 main 里写 IPv4-mapped、tcpinfo 里写成 IPv4（写法不同也要对上）
            "ESTAB     0      0      192.0.2.10:8443     198.51.100.9:41000",
            "\t cubic bytes_acked:10 bytes_received:20",
            "TIME-WAIT 0      0      192.0.2.10:60152    198.51.100.8:443",
            "\t          ",
            "ESTAB     0      0      127.0.0.53%lo:53    127.0.0.1:41234",
            "\t cubic bytes_acked:7 bytes_received:9",
        ])
    }

    fn row_of<'a>(report: &'a ConnectionsReport, local: &str) -> &'a NetConn {
        report
            .rows
            .iter()
            .find(|row| row.local.display() == local)
            .unwrap_or_else(|| panic!("没有 {local}"))
    }

    #[test]
    fn tcpinfo_is_paired_by_local_and_remote_and_attached_to_the_tcp_rows() {
        let report = parse_connections(&tcpinfo_sample()).expect("解析");
        assert_eq!(report.rows.len(), 6, "tcpinfo 的行不能混进主列表");
        assert_eq!(report.server_time_ns, Some(1_700_000_000_123_456_789));
        // 真实格式：bytes_acked 当已发送（不含重传），bytes_received 当已接收
        assert_eq!(
            row_of(&report, "192.0.2.10:443").bytes,
            Some(TcpBytes {
                sent: 144_907,
                received: 6_664_002
            })
        );
        assert_eq!(
            row_of(&report, "[2001:db8::10]:443").bytes,
            Some(TcpBytes {
                sent: 400,
                received: 300
            })
        );
        // ::ffff: 映射写法不同也对得上
        assert_eq!(
            row_of(&report, "[::ffff:192.0.2.10]:8443").bytes,
            Some(TcpBytes {
                sent: 10,
                received: 20
            })
        );
        // 带接口名的本地地址
        assert_eq!(
            row_of(&report, "127.0.0.53%lo:53").bytes,
            Some(TcpBytes {
                sent: 7,
                received: 9
            })
        );
        // 信息行只有空白的（TIME-WAIT）没有字节数；UDP 永远没有
        assert_eq!(row_of(&report, "192.0.2.10:60152").bytes, None);
        assert_eq!(row_of(&report, "*:5353").bytes, None);
        // 主列表本身不受影响
        assert_eq!(report.rows[0].process_label(), "web-server (1)");
        assert!(!report.counters_truncated);
    }

    #[test]
    fn an_old_iproute2_header_line_in_the_tcpinfo_section_is_skipped() {
        let text = out(&[
            "@@ZSSH source ss",
            "tcp ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "@@ZSSH now 1700000000123456789",
            "@@ZSSH tcpinfo",
            "State      Recv-Q Send-Q Local Address:Port  Peer Address:Port",
            "ESTAB      0      0      192.0.2.10:22       198.51.100.7:51234",
            "\t cubic bytes_acked:11 bytes_received:22",
        ]);
        let report = parse_connections(&text).expect("解析");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(
            report.rows[0].bytes,
            Some(TcpBytes {
                sent: 11,
                received: 22
            })
        );
    }

    #[test]
    fn a_line_that_only_looks_like_a_state_line_is_not_a_connection() {
        // Recv-Q / Send-Q 不是数字：不当成连接（表头、别的输出）
        assert_eq!(
            parse_tcpinfo_head("State Recv-Q Send-Q Local:Port Peer:Port"),
            None
        );
        assert_eq!(parse_tcpinfo_head(""), None);
        assert!(parse_tcpinfo_head("ESTAB 0 0 192.0.2.1:22 198.51.100.2:5000").is_some());
        // 带 netid 的写法也认
        assert_eq!(
            parse_tcpinfo_head("tcp ESTAB 0 0 192.0.2.1:22 198.51.100.2:5000"),
            parse_tcpinfo_head("ESTAB 0 0 192.0.2.1:22 198.51.100.2:5000")
        );
    }

    #[test]
    fn bytes_sent_is_the_fallback_for_bytes_acked_and_acked_wins_when_both_exist() {
        let only_sent =
            parse_tcp_bytes("\t cubic bytes_sent:900 bytes_retrans:100 bytes_received:5");
        assert_eq!(
            only_sent,
            Some(TcpBytes {
                sent: 900,
                received: 5
            })
        );
        let both =
            parse_tcp_bytes("\t bytes_sent:900 bytes_retrans:100 bytes_acked:800 bytes_received:5");
        assert_eq!(both.map(|b| b.sent), Some(800));
    }

    #[test]
    fn a_counter_that_ss_leaves_out_is_zero_but_no_counter_at_all_means_no_bytes() {
        // iproute2 只打印非 0 的计数：只收不发的连接没有 bytes_acked。
        assert_eq!(
            parse_tcp_bytes("\t cubic bytes_received:42"),
            Some(TcpBytes {
                sent: 0,
                received: 42
            })
        );
        assert_eq!(
            parse_tcp_bytes("\t cubic bytes_acked:42"),
            Some(TcpBytes {
                sent: 42,
                received: 0
            })
        );
        // 老内核（< 4.1）：信息行有内容，但没有任何字节字段
        let text = out(&[
            "@@ZSSH source ss",
            "tcp ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "@@ZSSH now 1700000000123456789",
            "@@ZSSH tcpinfo",
            "ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "\t cubic wscale:7,7 rto:204 rtt:1/0.5 mss:1448 cwnd:10 send 100Mbps",
        ]);
        let report = parse_connections(&text).expect("解析");
        assert_eq!(report.rows[0].bytes, None);
        assert!(
            report.counters_unavailable(),
            "有已建立连接却一条字节数都没有"
        );
    }

    #[test]
    fn a_quiet_connection_counts_as_zero_once_the_kernel_is_known_to_give_counters() {
        // 刚建立、还没收发过数据的连接：iproute2 不打印 0，信息行里一个计数都没有。同一份输出里
        // 别的连接有计数，说明内核给计数，这条就是 0 而不是"不知道"。TIME-WAIT 的空白信息行不算。
        let text = out(&[
            "@@ZSSH source ss",
            "tcp ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "tcp ESTAB 0 0 192.0.2.10:443 198.51.100.8:40000",
            "tcp TIME-WAIT 0 0 192.0.2.10:8080 198.51.100.9:33333",
            "@@ZSSH now 1700000000123456789",
            "@@ZSSH tcpinfo",
            "ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "\t cubic bytes_acked:10 bytes_received:20",
            "ESTAB 0 0 192.0.2.10:443 198.51.100.8:40000",
            "\t cubic wscale:7,7 rto:204 mss:1448 cwnd:10",
            "TIME-WAIT 0 0 192.0.2.10:8080 198.51.100.9:33333",
            "\t      ",
        ]);
        let report = parse_connections(&text).expect("解析");
        assert_eq!(
            report.rows[1].bytes,
            Some(TcpBytes {
                sent: 0,
                received: 0
            })
        );
        assert_eq!(report.rows[2].bytes, None, "空白信息行不当成 0");
    }

    #[test]
    fn the_awk_trimmed_info_lines_parse_like_the_full_ones() {
        // 服务器上 awk 把信息行压成 `\t- bytes_…`；计数全缺时只剩 `\t-`（有内容），空白行原样。
        let text = out(&[
            "@@ZSSH source ss",
            "tcp ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "tcp ESTAB 0 0 [2001:db8::10]:443 [2001:db8::7]:40000",
            "tcp ESTAB 0 0 192.0.2.10:8443 198.51.100.8:40001",
            "@@ZSSH now 1700000000123456789",
            "@@ZSSH tcpinfo",
            "ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "\t- bytes_sent:500 bytes_acked:400 bytes_received:300",
            "ESTAB 0 0 [2001:db8::10]:443 [2001:db8::7]:40000",
            "\t-",
            "ESTAB 0 0 192.0.2.10:8443 198.51.100.8:40001",
            "\t- bytes_received:7",
        ]);
        let report = parse_connections(&text).expect("解析");
        let bytes: Vec<_> = report.rows.iter().map(|row| row.bytes).collect();
        assert_eq!(
            bytes,
            vec![
                Some(TcpBytes {
                    sent: 400,
                    received: 300
                }),
                Some(TcpBytes {
                    sent: 0,
                    received: 0
                }),
                Some(TcpBytes {
                    sent: 0,
                    received: 7
                }),
            ]
        );
    }

    #[test]
    fn the_server_time_must_be_a_full_nanosecond_number_or_it_is_ignored() {
        assert_eq!(
            parse_server_time("1700000000123456789"),
            Some(1_700_000_000_123_456_789)
        );
        // busybox 等不认 %N：输出带字母 / 只有秒 / 空
        assert_eq!(parse_server_time("1700000000N"), None);
        assert_eq!(parse_server_time("1700000000%N"), None);
        assert_eq!(parse_server_time("1700000000"), None);
        assert_eq!(parse_server_time(""), None);
        let report =
            parse_connections("@@ZSSH source ss\n@@ZSSH now 1700000000N\n@@ZSSH tcpinfo\n")
                .expect("解析");
        assert_eq!(report.server_time_ns, None);
        let report =
            parse_connections("@@ZSSH source ss\n@@ZSSH now \n@@ZSSH tcpinfo\n").expect("解析");
        assert_eq!(
            report.server_time_ns, None,
            "date 整个失败时 now 后面是空的"
        );
    }

    #[test]
    fn falling_back_to_netstat_throws_away_the_ss_counters_and_time() {
        let text = out(&[
            "@@ZSSH source ss",
            "@@ZSSH now 1700000000123456789",
            "@@ZSSH tcpinfo",
            "ESTAB 0 0 192.0.2.10:22 198.51.100.7:51234",
            "\t bytes_acked:11 bytes_received:22",
            "@@ZSSH source netstat",
            "tcp 0 0 192.0.2.10:22 198.51.100.7:51234 ESTABLISHED 1200/sshd",
        ]);
        let report = parse_connections(&text).expect("解析");
        assert_eq!(report.source, ConnSource::Netstat);
        assert_eq!(report.server_time_ns, None);
        assert_eq!(report.rows[0].bytes, None);
        assert!(report.counters_unavailable(), "netstat 来源没有流量计数");
    }

    #[test]
    fn counters_are_only_unavailable_when_there_is_something_to_count() {
        // ss 来源，只有监听和 UDP：没东西可算，不提示
        let idle = parse_connections(
            "@@ZSSH source ss\ntcp LISTEN 0 1 0.0.0.0:80 0.0.0.0:*\nudp UNCONN 0 0 *:53 *:*\n",
        )
        .expect("解析");
        assert!(!idle.counters_unavailable());
        // /proc 来源总是没有
        let proc_report = parse_connections(PROC_SAMPLE).expect("解析");
        assert!(proc_report.counters_unavailable());
        // 正常的 ss 样本有字节数：不提示
        assert!(!parse_connections(&tcpinfo_sample())
            .expect("解析")
            .counters_unavailable());
    }

    /// 两次快照里的一条连接：（本地, 远端, 已发送与已接收）；`None` = 这行没有字节数。
    type Conn<'a> = (&'a str, &'a str, Option<(u64, u64)>);

    fn snapshot(now_ns: Option<u64>, conns: &[Conn<'_>]) -> ConnectionsReport {
        let mut lines = vec!["@@ZSSH source ss".to_owned()];
        for (local, remote, _) in conns {
            lines.push(format!("tcp ESTAB 0 0 {local} {remote}"));
        }
        if let Some(ns) = now_ns {
            lines.push(format!("@@ZSSH now {ns}"));
        }
        lines.push("@@ZSSH tcpinfo".to_owned());
        for (local, remote, bytes) in conns {
            lines.push(format!("ESTAB 0 0 {local} {remote}"));
            lines.push(match bytes {
                Some((sent, received)) => {
                    format!("\t cubic bytes_acked:{sent} bytes_received:{received}")
                }
                None => "\t ".to_owned(),
            });
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        parse_connections(&out(&refs)).expect("解析")
    }

    const T0: u64 = 1_700_000_000_000_000_000;
    const SEC: u64 = 1_000_000_000;

    /// 同一批连接换个服务器时间（测"没有变化"用）。
    fn cur_at(report: &ConnectionsReport, ns: u64) -> ConnectionsReport {
        ConnectionsReport {
            server_time_ns: Some(ns),
            ..report.clone()
        }
    }

    #[test]
    fn rates_are_the_byte_difference_over_the_server_time_difference() {
        let prev = snapshot(
            Some(T0),
            &[("192.0.2.1:22", "198.51.100.1:5000", Some((1000, 5000)))],
        );
        let cur = snapshot(
            Some(T0 + 2 * SEC),
            &[("192.0.2.1:22", "198.51.100.1:5000", Some((3000, 13000)))],
        );
        // 本地时间差故意给一个错的值：两次都有服务器时间时必须用服务器的
        let rates = compute_rates(&prev, &cur, Duration::from_secs(10));
        assert_eq!(
            rates,
            [Some(ConnRate {
                up: 1000,
                down: 4000
            })]
        );
        // 没有变化是 0 而不是 None
        let idle = compute_rates(&cur, &cur_at(&cur, T0 + 4 * SEC), Duration::ZERO);
        assert_eq!(idle, [Some(ConnRate { up: 0, down: 0 })]);
    }

    #[test]
    fn without_server_time_on_both_sides_the_local_elapsed_time_is_used() {
        let conn = ("192.0.2.1:22", "198.51.100.1:5000");
        let prev = snapshot(None, &[(conn.0, conn.1, Some((0, 0)))]);
        let cur = snapshot(None, &[(conn.0, conn.1, Some((4000, 8000)))]);
        let rates = compute_rates(&prev, &cur, Duration::from_secs(4));
        assert_eq!(
            rates,
            [Some(ConnRate {
                up: 1000,
                down: 2000
            })]
        );
        // 只有一边有服务器时间：也退本地时间
        let one_sided = snapshot(Some(T0), &[(conn.0, conn.1, Some((4000, 8000)))]);
        assert_eq!(
            compute_rates(&prev, &one_sided, Duration::from_secs(4)),
            rates
        );
    }

    #[test]
    fn a_non_positive_or_tiny_interval_gives_no_rates() {
        let conn = ("192.0.2.1:22", "198.51.100.1:5000");
        let prev = snapshot(Some(T0), &[(conn.0, conn.1, Some((0, 0)))]);
        let at = |ns: u64| snapshot(Some(ns), &[(conn.0, conn.1, Some((1000, 1000)))]);
        let local = Duration::from_secs(5);
        // 服务器时间没前进 / 倒退（对时）：不退回本地时间，直接没有
        assert_eq!(compute_rates(&prev, &at(T0), local), [None]);
        assert_eq!(compute_rates(&prev, &at(T0 - SEC), local), [None]);
        // 小于 200ms
        assert_eq!(compute_rates(&prev, &at(T0 + 199_000_000), local), [None]);
        assert!(compute_rates(&prev, &at(T0 + 200_000_000), local)[0].is_some());
        // 本地时间同理
        let no_time_prev = snapshot(None, &[(conn.0, conn.1, Some((0, 0)))]);
        let no_time_cur = snapshot(None, &[(conn.0, conn.1, Some((1000, 1000)))]);
        assert_eq!(
            compute_rates(&no_time_prev, &no_time_cur, Duration::ZERO),
            [None]
        );
        assert_eq!(
            compute_rates(&no_time_prev, &no_time_cur, Duration::from_millis(100)),
            [None]
        );
        assert!(compute_rates(&no_time_prev, &no_time_cur, RATE_MIN_INTERVAL)[0].is_some());
    }

    #[test]
    fn a_new_connection_a_reused_port_or_missing_bytes_have_no_rate() {
        let a = ("192.0.2.1:22", "198.51.100.1:5000");
        let b = ("192.0.2.1:22", "198.51.100.2:5001");
        let c = ("192.0.2.1:22", "198.51.100.3:5002");
        let d = ("192.0.2.1:22", "198.51.100.4:5003");
        let e = ("192.0.2.1:22", "198.51.100.5:5004");
        let prev = snapshot(
            Some(T0),
            &[
                (a.0, a.1, Some((1000, 1000))),
                (b.0, b.1, Some((1000, 1000))),
                (c.0, c.1, None),
                (d.0, d.1, Some((1000, 1000))),
            ],
        );
        let cur = snapshot(
            Some(T0 + SEC),
            &[
                (a.0, a.1, Some((1500, 3000))),
                // 已发送回退：连接被换掉（端口复用）
                (b.0, b.1, Some((10, 5000))),
                // 上次没有字节数
                (c.0, c.1, Some((10, 10))),
                // 这次没有字节数
                (d.0, d.1, None),
                // 上次没有这条连接
                (e.0, e.1, Some((10, 10))),
            ],
        );
        let rates = compute_rates(&prev, &cur, Duration::from_secs(1));
        assert_eq!(
            rates,
            [
                Some(ConnRate {
                    up: 500,
                    down: 2000
                }),
                None,
                None,
                None,
                None
            ]
        );
        // 已接收回退同理
        let cur = snapshot(Some(T0 + SEC), &[(a.0, a.1, Some((5000, 10)))]);
        assert_eq!(compute_rates(&prev, &cur, Duration::from_secs(1)), [None]);
    }

    #[test]
    fn rates_survive_a_different_spelling_of_the_same_connection() {
        let prev = snapshot(
            Some(T0),
            &[(
                "[::ffff:192.0.2.1]:22",
                "[::ffff:198.51.100.1]:5000",
                Some((0, 0)),
            )],
        );
        let cur = snapshot(
            Some(T0 + SEC),
            &[("192.0.2.1:22", "198.51.100.1:5000", Some((100, 200)))],
        );
        assert_eq!(
            compute_rates(&prev, &cur, Duration::from_secs(1)),
            [Some(ConnRate { up: 100, down: 200 })]
        );
    }

    fn view_rates(
        rows: &[NetConn],
        rates: &[Option<ConnRate>],
        key: SortKey,
        descending: bool,
    ) -> Vec<usize> {
        view_indices(
            rows,
            rates,
            ConnFilter::All,
            "",
            SortSpec {
                key: Some(key),
                descending,
            },
        )
    }

    #[test]
    fn rate_columns_sort_big_first_and_rows_without_a_rate_always_sink() {
        let report = snapshot(
            Some(T0),
            &[
                ("192.0.2.1:1", "198.51.100.1:1", None),
                ("192.0.2.1:2", "198.51.100.1:2", None),
                ("192.0.2.1:3", "198.51.100.1:3", None),
                ("192.0.2.1:4", "198.51.100.1:4", None),
            ],
        );
        let rate = |up: u64, down: u64| Some(ConnRate { up, down });
        let rates = [rate(5, 900), None, rate(50, 100), rate(5, 100)];
        let rows = &report.rows;
        // 降序：大的在上；平手保持采集顺序；没有速率的最后
        assert_eq!(view_rates(rows, &rates, SortKey::Up, true), [2, 0, 3, 1]);
        assert_eq!(view_rates(rows, &rates, SortKey::Down, true), [0, 2, 3, 1]);
        // 升序：没有速率的还是最后
        assert_eq!(view_rates(rows, &rates, SortKey::Up, false), [0, 3, 2, 1]);
        assert_eq!(view_rates(rows, &rates, SortKey::Down, false), [2, 3, 0, 1]);
        // 速率表缺失（比行数短 / 为空）当作都没有速率，顺序不变
        assert_eq!(view_rates(rows, &[], SortKey::Up, true), [0, 1, 2, 3]);
        assert_eq!(
            view_rates(rows, &rates[..2], SortKey::Up, true),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn the_first_click_on_a_rate_header_is_descending_then_ascending_then_the_collection_order() {
        for key in [SortKey::Up, SortKey::Down] {
            let spec = SortSpec::default().toggled(key);
            assert_eq!((spec.key, spec.descending), (Some(key), true), "{key:?}");
            let spec = spec.toggled(key);
            assert_eq!((spec.key, spec.descending), (Some(key), false), "{key:?}");
            assert_eq!(spec.toggled(key), SortSpec::default(), "{key:?}");
        }
        // 从文字列换到速率列：速率列先降序
        let spec = SortSpec::default()
            .toggled(SortKey::State)
            .toggled(SortKey::Up);
        assert_eq!((spec.key, spec.descending), (Some(SortKey::Up), true));
        // 其他列的老规则没变：先升序
        let spec = SortSpec::default().toggled(SortKey::Local);
        assert!(!spec.descending);
    }

    /// 一份像两万条连接那么大的输出：每条主列表一行、tcpinfo 两行（信息行用真实长度）。
    fn big_output(count: usize) -> String {
        let mut text = String::from("@@ZSSH uid 0\n@@ZSSH source ss\n");
        let tuple = |i: usize| {
            format!(
                "192.0.2.10:{} 198.51.100.{}:{}",
                1024 + i % 60_000,
                1 + i % 250,
                2000 + i % 50_000
            )
        };
        for i in 0..count {
            text.push_str(&format!(
                "tcp   ESTAB  0      0      {} users:((\"app-worker\",pid={},fd=9))\n",
                tuple(i),
                800 + i % 400
            ));
        }
        text.push_str("@@ZSSH now 1700000000123456789\n@@ZSSH tcpinfo\n");
        for i in 0..count {
            text.push_str(&format!("ESTAB     0      0      {}\n{INFO}\n", tuple(i)));
        }
        text
    }

    #[test]
    fn a_cut_inside_the_tcpinfo_section_keeps_the_whole_main_list_and_only_loses_tail_counters() {
        let full = big_output(CONNECTIONS_MAX_ROWS);
        // 评估：两万条连接的完整输出远超 4 MiB，主列表本身远低于
        assert!(full.len() > CONNECTIONS_MAX_BYTES, "{}", full.len());
        let main_end = full.find("@@ZSSH tcpinfo").expect("tcpinfo 段");
        assert!(
            main_end < CONNECTIONS_MAX_BYTES * 3 / 4,
            "主列表 {main_end} 字节"
        );
        // 模拟 exec_capture：只收前 CONNECTIONS_MAX_BYTES 字节（可能截在一行中间）
        let mut cut_at = CONNECTIONS_MAX_BYTES;
        while !full.is_char_boundary(cut_at) {
            cut_at -= 1;
        }
        let report = report_from_output(full[..cut_at].to_owned(), true).expect("解析");
        assert_eq!(report.rows.len(), CONNECTIONS_MAX_ROWS, "主列表不能变少");
        assert!(!report.truncated, "没有丢连接，不能报\"连接太多\"");
        assert!(report.counters_truncated);
        let with = report.rows.iter().filter(|row| row.bytes.is_some()).count();
        assert!(with > 0 && with < CONNECTIONS_MAX_ROWS, "{with}");
        // 有字节数的是靠前的那些
        assert!(report.rows[0].bytes.is_some());
        assert!(report.rows[CONNECTIONS_MAX_ROWS - 1].bytes.is_none());
    }

    #[test]
    fn a_cut_inside_the_main_list_still_flags_too_many_connections() {
        let full = big_output(200);
        let main_end = full.find("@@ZSSH now").expect("主列表结束");
        let report = report_from_output(full[..main_end / 2].to_owned(), true).expect("解析");
        assert!(report.truncated);
        assert!(!report.counters_truncated);
        assert!(report.rows.len() < 200);
        // 没截断：两个标记都不立
        let whole = report_from_output(full, false).expect("解析");
        assert!(!whole.truncated && !whole.counters_truncated);
        assert!(whole.rows.iter().all(|row| row.bytes.is_some()));
    }
}
