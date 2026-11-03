//! 断开服务器上的一条网络连接、以及"所有网络连接"对话框右键菜单的可用性规则（V2-U21）。
//!
//! 这是**写操作**，只能由用户在界面上点、确认之后才执行，没有接进 AI 工具桥 / zssh-mcp：
//! - 命令是一条固定形状的 `ss -K`（`exec sh -c 'ss -K -t state <状态> src <IP> sport = :<端口> dst <IP> dport = :<端口>'`），
//!   按四元组精确匹配；IP 和端口严格校验后才拼进去（通配、`%scope`、空、任何别的字符一律拒绝）；
//! - 执行前**重新采一次连接表**，这一条还在才发命令（列表过时了，四元组可能已经属于别的连接）；
//! - 不用 sudo：没有 root 就如实说"需要 root 才能断开连接"；
//! - ZeeTerm 自己正在用的那条 SSH 连接（`$SSH_CONNECTION`）在核心里再拦一次，界面上本来就置灰。
//!
//! "结束进程"不在这里：菜单里的结束进程走 [`super::processes::terminate_process`]，同一套函数、同一套受保护规则，
//! 这里只算"这个菜单项能不能点"（[`kill_check`]）。

use std::net::IpAddr;
use std::time::Duration;

use super::connection::Connection;
use super::connections::{
    collect_connections, parse_endpoint, same_endpoint, ConnSource, ConnState, ConnectionsReport,
    Endpoint, NetConn, NetProto, ProcRef,
};
use super::cwd::shell_quote;
use super::processes::{ProcessReport, Protection};

/// `ss -K` 的时限。
pub const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 一条连接为什么不能断开（右键菜单置灰并写原因）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectBlock {
    /// ZeeTerm 正在使用这条连接：断了就把自己踢下线。
    OwnConnection,
    /// UDP 没有连接。
    Udp,
    /// 监听端口没有对端。
    Listening,
    /// TIME-WAIT 已经在关闭。
    TimeWait,
    /// CLOSE 或认不出的状态：没有对端可断。
    NotConnected,
    /// 来源不是 `ss`（netstat / `/proc`），服务器上没有可用的 `ss`。
    NoSs,
}

impl DisconnectBlock {
    pub fn reason(self) -> &'static str {
        match self {
            Self::OwnConnection => "ZeeTerm 正在使用这条连接",
            Self::Udp => "UDP 没有连接可断",
            Self::Listening => "监听端口没有对端，要停掉请结束进程",
            Self::TimeWait => "TIME-WAIT 已经在关闭",
            Self::NotConnected => "这条连接没有对端，不需要断开",
            Self::NoSs => "服务器上没有 ss 命令，无法单独断开连接",
        }
    }
}

/// 状态是不是"有对端"、`ss -K` 能处理：返回 `ss` 的 `state` 关键字。
fn ss_state_keyword(state: &ConnState) -> Option<&'static str> {
    Some(match state {
        ConnState::Established => "established",
        ConnState::SynSent => "syn-sent",
        ConnState::SynRecv => "syn-recv",
        ConnState::FinWait1 => "fin-wait-1",
        ConnState::FinWait2 => "fin-wait-2",
        ConnState::CloseWait => "close-wait",
        ConnState::LastAck => "last-ack",
        ConnState::Closing => "closing",
        _ => return None,
    })
}

/// 协议和状态上能不能断开（不看来源和是不是自己的连接）。
fn kind_check(row: &NetConn) -> Result<(), DisconnectBlock> {
    if row.proto == NetProto::Udp {
        return Err(DisconnectBlock::Udp);
    }
    match row.state {
        ConnState::Listen => Err(DisconnectBlock::Listening),
        ConnState::TimeWait => Err(DisconnectBlock::TimeWait),
        _ if ss_state_keyword(&row.state).is_none() => Err(DisconnectBlock::NotConnected),
        _ => Ok(()),
    }
}

/// 这一行能不能断开。顺序：自己的连接 → 协议 / 状态 → 来源。
pub fn disconnect_check(report: &ConnectionsReport, row: &NetConn) -> Result<(), DisconnectBlock> {
    if report.is_own_connection(row) {
        return Err(DisconnectBlock::OwnConnection);
    }
    kind_check(row)?;
    if report.source != ConnSource::Ss {
        return Err(DisconnectBlock::NoSs);
    }
    Ok(())
}

/// 这一行的某个进程为什么不能结束（菜单项置灰并写原因）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillBlock {
    /// ZeeTerm 正在使用这条连接：它的进程是承载本次会话的 sshd。
    OwnConnection,
    /// 进程列是空的：没权限看到这条连接属于哪个进程。
    NoProcessInfo,
    /// 还没读到进程列表：没法核对这个进程是不是受保护的。
    ProcessesUnavailable,
    /// 进程列表里已经没有这个 PID。
    NotRunning,
    /// 受保护的进程（U18 的同一套判定）。
    Protected(Protection),
}

impl KillBlock {
    pub fn reason(self) -> String {
        match self {
            Self::OwnConnection => "ZeeTerm 正在使用这条连接".to_owned(),
            Self::NoProcessInfo => {
                "看不到这条连接属于哪个进程（非 root 登录时看不到其他用户的进程）".to_owned()
            }
            Self::ProcessesUnavailable => "还没读到进程列表，无法确认这个进程能不能结束".to_owned(),
            Self::NotRunning => "这个进程已经不在了".to_owned(),
            Self::Protected(protection) => {
                format!("受保护的系统进程不能结束：{}", protection.reason())
            }
        }
    }

    /// 菜单右侧的简短原因。
    pub fn short(self) -> &'static str {
        match self {
            Self::OwnConnection => "ZeeTerm 正在使用",
            Self::NoProcessInfo => "看不到进程",
            Self::ProcessesUnavailable => "没读到进程列表",
            Self::NotRunning => "进程已不在",
            Self::Protected(protection) => protection.reason(),
        }
    }
}

/// 这一行里的 `pid` 能不能结束。判定完全用 U18 的 [`ProcessReport::protection`]（不另写一套规则）。
pub fn kill_check(
    report: &ConnectionsReport,
    row: &NetConn,
    procs: Option<&ProcessReport>,
    pid: u32,
) -> Result<(), KillBlock> {
    if report.is_own_connection(row) {
        return Err(KillBlock::OwnConnection);
    }
    if row.processes.is_empty() {
        return Err(KillBlock::NoProcessInfo);
    }
    let Some(procs) = procs else {
        return Err(KillBlock::ProcessesUnavailable);
    };
    if procs.row(pid).is_none() {
        return Err(KillBlock::NotRunning);
    }
    match procs.protection(pid) {
        Some(protection) => Err(KillBlock::Protected(protection)),
        None => Ok(()),
    }
}

/// 一行菜单里最多列出的进程个数，超过的改成"在进程管理里查看…"。
pub const MENU_MAX_PROCESSES: usize = 6;

/// 菜单里要列出的进程：去重后的前 [`MENU_MAX_PROCESSES`] 个，以及被省略的个数。
pub fn menu_processes(row: &NetConn) -> (Vec<ProcRef>, usize) {
    let all = row.unique_processes();
    let hidden = all.len().saturating_sub(MENU_MAX_PROCESSES);
    (all.into_iter().take(MENU_MAX_PROCESSES).collect(), hidden)
}

// ---- 命令构造与校验 ----

/// 断开连接失败的原因（文字直接给用户看）。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DisconnectError {
    #[error("地址或端口不合法，没有发出任何命令：{0}")]
    InvalidAddress(String),
    #[error("不能断开这条连接：{}。", .0.reason())]
    Blocked(DisconnectBlock),
    #[error("这条连接已经不在了，列表已刷新。")]
    Gone,
    #[error("这台服务器的内核不支持单独断开连接，可以改为结束占用它的进程。")]
    NotSupported,
    #[error("需要 root 才能断开连接。")]
    NoPermission,
    #[error("服务器上没有 ss 命令，无法单独断开连接。")]
    NoSs,
    #[error(
        "没有找到这条连接（可能已经关闭）；如果刷新后它还在，多半是内核不支持单独断开，可以改为结束占用它的进程。"
    )]
    NoMatch,
    #[error("等了 5 秒没有结果。")]
    Timeout,
    #[error("断开失败：{0}")]
    Failed(String),
}

/// 校验并写成 `ss` 的地址：IPv4 点分原样，IPv6 加方括号（`ss` 按最后一个冒号切端口，不加括号会把地址吃掉一截）。
///
/// 只接受**一个具体的 IP**：带 `%网卡` 的、`*`、空、`0.0.0.0` / `::`（通配）、含任何别的字符的都拒绝。
/// 输出是 `IpAddr` 重新格式化的结果，不会带进输入里的任何原文。
fn ss_address(host: &str) -> Result<String, DisconnectError> {
    if host.is_empty() {
        return Err(DisconnectError::InvalidAddress("地址是空的".to_owned()));
    }
    if host.contains('%') {
        return Err(DisconnectError::InvalidAddress(
            "带网卡范围（%）的地址暂不支持单独断开".to_owned(),
        ));
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    {
        return Err(DisconnectError::InvalidAddress(
            "地址里只能有数字、a-f、冒号和点".to_owned(),
        ));
    }
    let ip: IpAddr = host
        .parse()
        .map_err(|_| DisconnectError::InvalidAddress("不是一个完整的 IP 地址".to_owned()))?;
    if ip.is_unspecified() {
        return Err(DisconnectError::InvalidAddress(
            "通配地址（0.0.0.0 / ::）不能用来指定一条连接".to_owned(),
        ));
    }
    Ok(match ip {
        IpAddr::V4(v4) => v4.to_string(),
        // 方括号在 `sh` 里是通配符，命令里再加双引号（见 `disconnect_command`）。
        IpAddr::V6(v6) => format!("[{v6}]"),
    })
}

/// 端口：纯 ASCII 数字，1..=65535（`*`、空、符号、空白都不行）。
fn ss_port(port: &str) -> Result<u16, DisconnectError> {
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DisconnectError::InvalidAddress(
            "端口必须是 1 到 65535 的纯数字".to_owned(),
        ));
    }
    port.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| DisconnectError::InvalidAddress("端口必须是 1 到 65535 的纯数字".to_owned()))
}

/// 断开连接的命令行：
/// `exec sh -c 'ss -K -t state established src 192.0.2.10 sport = :22 dst 198.51.100.7 dport = :51234'`。
///
/// IPv6 的方括号地址加双引号，免得 `sh` 把它当通配符展开。命令里只有常量、`ss` 的状态关键字、和重新格式化过的
/// IP / 端口数字。
pub fn disconnect_command(
    state: &ConnState,
    local: &Endpoint,
    remote: &Endpoint,
) -> Result<String, DisconnectError> {
    let keyword = ss_state_keyword(state).ok_or(DisconnectError::Blocked(match state {
        ConnState::Listen => DisconnectBlock::Listening,
        ConnState::TimeWait => DisconnectBlock::TimeWait,
        _ => DisconnectBlock::NotConnected,
    }))?;
    let quote = |address: String| {
        if address.starts_with('[') {
            format!("\"{address}\"")
        } else {
            address
        }
    };
    let (src, dst) = (ss_address(&local.host)?, ss_address(&remote.host)?);
    let (sport, dport) = (ss_port(&local.port)?, ss_port(&remote.port)?);
    Ok(format!(
        "exec sh -c {}",
        shell_quote(&format!(
            "ss -K -t state {keyword} src {} sport = :{sport} dst {} dport = :{dport}",
            quote(src),
            quote(dst)
        ))
    ))
}

/// `ss -K` 的输出里有没有这一条连接（`ss -K` 只打印**成功关掉**的 socket）。
///
/// 一行里按顺序找到"本地端点、对端端点"两个相邻的词（行首的 `State Recv-Q Send-Q` 和行尾的 `users:(...)` 不影响）。
pub fn kill_output_mentions(stdout: &str, local: &Endpoint, remote: &Endpoint) -> bool {
    stdout.lines().any(|line| {
        let ends: Vec<Option<Endpoint>> = line.split_whitespace().map(parse_endpoint).collect();
        ends.windows(2).any(|pair| match (&pair[0], &pair[1]) {
            (Some(a), Some(b)) => same_endpoint(a, local) && same_endpoint(b, remote),
            _ => false,
        })
    })
}

/// 把 `ss -K` 的结果（退出码、stdout、stderr）归类。按关键词认，不看退出码以外的格式：
/// 权限 / 不支持的措辞优先（`ss` 有时报错了退出码仍是 0）。
pub fn classify_disconnect(
    exit_code: Option<u32>,
    stdout: &str,
    stderr: &str,
    local: &Endpoint,
    remote: &Endpoint,
) -> Result<(), DisconnectError> {
    let text = format!("{stderr}\n{stdout}").to_lowercase();
    if text.contains("not permitted") || text.contains("permission denied") {
        return Err(DisconnectError::NoPermission);
    }
    if text.contains("not supported")
        || text.contains("sock_destroy")
        || text.contains("invalid option")
        || text.contains("unrecognized option")
        || text.contains("unknown option")
    {
        return Err(DisconnectError::NotSupported);
    }
    if exit_code == Some(127)
        || text.contains("ss: not found")
        || text.contains("command not found")
    {
        return Err(DisconnectError::NoSs);
    }
    match exit_code {
        Some(0) => {
            if kill_output_mentions(stdout, local, remote) {
                Ok(())
            } else {
                Err(DisconnectError::NoMatch)
            }
        }
        _ => {
            let first = stderr
                .lines()
                .chain(stdout.lines())
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("ss 没有成功，也没有给出原因");
            Err(DisconnectError::Failed(first.to_owned()))
        }
    }
}

// ---- 执行 ----

/// 断开这一条连接（写操作，只在用户确认之后调用）。成功 = `ss -K` 报告关掉了它。
///
/// 先重新采一次连接表：来源必须是 `ss`、这一条（同协议、同四元组）还在、不是 ZeeTerm 自己的连接、状态仍然有对端，
/// 然后**用这次新采的状态**构造命令。没权限返回 [`DisconnectError::NoPermission`]，不会换 sudo 重试。
pub async fn disconnect_connection(
    connection: &Connection,
    target: &NetConn,
) -> Result<(), DisconnectError> {
    // 先校验再联网：UDP / 监听 / 不合法的地址端口，一条命令都不发。
    kind_check(target).map_err(DisconnectError::Blocked)?;
    disconnect_command(&target.state, &target.local, &target.remote)?;
    let fresh = collect_connections(connection)
        .await
        .map_err(|err| DisconnectError::Failed(err.to_string()))?;
    if fresh.source != ConnSource::Ss {
        return Err(DisconnectError::NoSs);
    }
    let Some(now) = fresh.rows.iter().find(|row| {
        row.proto == target.proto
            && same_endpoint(&row.local, &target.local)
            && same_endpoint(&row.remote, &target.remote)
    }) else {
        return Err(DisconnectError::Gone);
    };
    disconnect_check(&fresh, now).map_err(DisconnectError::Blocked)?;
    let command = disconnect_command(&now.state, &now.local, &now.remote)?;
    let capture = connection
        .exec_capture(&command, DISCONNECT_TIMEOUT, 64 * 1024)
        .await
        .map_err(|err| DisconnectError::Failed(err.to_string()))?;
    if capture.timed_out {
        return Err(DisconnectError::Timeout);
    }
    classify_disconnect(
        capture.exit_code,
        &capture.stdout,
        &capture.stderr,
        &now.local,
        &now.remote,
    )
}

#[cfg(test)]
mod tests {
    use super::super::connections::parse_connections;
    use super::super::processes::parse_processes;
    use super::*;

    fn ep(host: &str, port: &str) -> Endpoint {
        Endpoint {
            host: host.to_owned(),
            port: port.to_owned(),
        }
    }

    const SS: &str = "\
@@ZSSH uid 0
@@ZSSH ssh_connection 198.51.100.7 51234 192.0.2.10 22
@@ZSSH source ss
udp   UNCONN 0 0 127.0.0.53%lo:53 0.0.0.0:* users:((\"resolver\",pid=620,fd=13))
udp   ESTAB  0 0 192.0.2.10:123 203.0.113.1:123 users:((\"timesync\",pid=650,fd=4))
tcp   LISTEN 0 511 0.0.0.0:80 0.0.0.0:* users:((\"web-server\",pid=123,fd=6),(\"web-server\",pid=124,fd=6))
tcp   ESTAB  0 0 192.0.2.10:22 198.51.100.7:51234 users:((\"sshd\",pid=3090,fd=4),(\"sshd\",pid=3100,fd=4))
tcp   ESTAB  0 0 192.0.2.10:22 198.51.100.8:40000 users:((\"sshd\",pid=1200,fd=4))
tcp   ESTAB  0 0 192.0.2.10:443 198.51.100.21:40112
tcp   ESTAB  0 0 [2001:db8::10]:443 [2001:db8::77]:40000 users:((\"web-server\",pid=124,fd=13))
tcp   TIME-WAIT 0 0 192.0.2.10:8080 198.51.100.9:33333
tcp   SYN-SENT  0 1 192.0.2.10:45678 203.0.113.5:443 users:((\"updater\",pid=1500,fd=8))
tcp   FIN-WAIT-1 0 0 192.0.2.10:9001 203.0.113.6:5000
tcp   FIN-WAIT-2 0 0 192.0.2.10:9002 203.0.113.6:5001
tcp   CLOSE-WAIT 1 0 192.0.2.10:8081 198.51.100.30:50000 users:((\"app-worker\",pid=812,fd=14))
tcp   SYN-RECV 0 0 192.0.2.10:8082 198.51.100.31:50001
tcp   LAST-ACK 0 0 192.0.2.10:8083 198.51.100.32:50002
tcp   CLOSING 0 0 192.0.2.10:8084 198.51.100.33:50003
tcp   CLOSE 0 0 192.0.2.10:8085 198.51.100.34:50004
";

    fn report() -> ConnectionsReport {
        parse_connections(SS).expect("解析")
    }

    fn row_with<'a>(report: &'a ConnectionsReport, local: &str, remote: &str) -> &'a NetConn {
        report
            .rows
            .iter()
            .find(|r| r.local.display() == local && r.remote.display() == remote)
            .unwrap_or_else(|| panic!("没有 {local} {remote}"))
    }

    #[test]
    fn only_connected_tcp_rows_with_the_ss_source_can_be_disconnected() {
        let report = report();
        let check =
            |local: &str, remote: &str| disconnect_check(&report, row_with(&report, local, remote));
        // 可以断：已建立、各种半开 / 半关状态。
        for (local, remote) in [
            ("192.0.2.10:22", "198.51.100.8:40000"),
            ("192.0.2.10:443", "198.51.100.21:40112"),
            ("[2001:db8::10]:443", "[2001:db8::77]:40000"),
            ("192.0.2.10:45678", "203.0.113.5:443"),
            ("192.0.2.10:9001", "203.0.113.6:5000"),
            ("192.0.2.10:9002", "203.0.113.6:5001"),
            ("192.0.2.10:8081", "198.51.100.30:50000"),
            ("192.0.2.10:8082", "198.51.100.31:50001"),
            ("192.0.2.10:8083", "198.51.100.32:50002"),
            ("192.0.2.10:8084", "198.51.100.33:50003"),
        ] {
            assert_eq!(check(local, remote), Ok(()), "{local} {remote}");
        }
        // 置灰，原因各不相同。
        assert_eq!(
            check("0.0.0.0:80", "0.0.0.0:*"),
            Err(DisconnectBlock::Listening)
        );
        assert_eq!(
            check("127.0.0.53%lo:53", "0.0.0.0:*"),
            Err(DisconnectBlock::Udp)
        );
        assert_eq!(
            check("192.0.2.10:123", "203.0.113.1:123"),
            Err(DisconnectBlock::Udp),
            "连了对端的 UDP 也没有连接可断"
        );
        assert_eq!(
            check("192.0.2.10:8080", "198.51.100.9:33333"),
            Err(DisconnectBlock::TimeWait)
        );
        assert_eq!(
            check("192.0.2.10:8085", "198.51.100.34:50004"),
            Err(DisconnectBlock::NotConnected)
        );
        // 自己的连接：压过别的原因。
        assert_eq!(
            check("192.0.2.10:22", "198.51.100.7:51234"),
            Err(DisconnectBlock::OwnConnection)
        );
    }

    #[test]
    fn the_reasons_use_the_wording_from_the_card() {
        assert_eq!(
            DisconnectBlock::Listening.reason(),
            "监听端口没有对端，要停掉请结束进程"
        );
        assert_eq!(DisconnectBlock::Udp.reason(), "UDP 没有连接可断");
        assert_eq!(DisconnectBlock::TimeWait.reason(), "TIME-WAIT 已经在关闭");
        assert_eq!(
            DisconnectBlock::NoSs.reason(),
            "服务器上没有 ss 命令，无法单独断开连接"
        );
        assert_eq!(
            DisconnectBlock::OwnConnection.reason(),
            "ZeeTerm 正在使用这条连接"
        );
    }

    #[test]
    fn netstat_and_proc_sources_cannot_disconnect_anything() {
        for source in [ConnSource::Netstat, ConnSource::Proc] {
            let mut report = report();
            report.source = source;
            // 自己的连接仍是"自己的"（优先），别的 ESTAB 行是"没有 ss"。
            let other = row_with(&report, "192.0.2.10:443", "198.51.100.21:40112").clone();
            assert_eq!(
                disconnect_check(&report, &other),
                Err(DisconnectBlock::NoSs)
            );
        }
    }

    #[test]
    fn the_command_has_one_fixed_shape_for_ipv4_and_ipv6() {
        let v4 = disconnect_command(
            &ConnState::Established,
            &ep("192.0.2.10", "22"),
            &ep("198.51.100.7", "51234"),
        )
        .expect("命令");
        assert_eq!(
            v4,
            "exec sh -c 'ss -K -t state established src 192.0.2.10 sport = :22 dst 198.51.100.7 dport = :51234'"
        );
        let v6 = disconnect_command(
            &ConnState::CloseWait,
            &ep("2001:db8::10", "443"),
            &ep("2001:db8:0:0:1:2:3:4", "40000"),
        )
        .expect("命令");
        assert_eq!(
            v6,
            "exec sh -c 'ss -K -t state close-wait src \"[2001:db8::10]\" sport = :443 dst \"[2001:db8::1:2:3:4]\" dport = :40000'"
        );
        // 每种有对端的状态都有自己的关键字。
        for (state, keyword) in [
            (ConnState::SynSent, "syn-sent"),
            (ConnState::SynRecv, "syn-recv"),
            (ConnState::FinWait1, "fin-wait-1"),
            (ConnState::FinWait2, "fin-wait-2"),
            (ConnState::LastAck, "last-ack"),
            (ConnState::Closing, "closing"),
        ] {
            let command = disconnect_command(&state, &ep("192.0.2.1", "1"), &ep("192.0.2.2", "2"))
                .expect("命令");
            assert!(command.contains(&format!("state {keyword} ")), "{command}");
        }
        // 没有对端的状态不构造命令。
        for state in [
            ConnState::Listen,
            ConnState::TimeWait,
            ConnState::Close,
            ConnState::Unconn,
            ConnState::Other("X".into()),
        ] {
            assert!(
                disconnect_command(&state, &ep("192.0.2.1", "1"), &ep("192.0.2.2", "2")).is_err()
            );
        }
    }

    #[test]
    fn wildcards_scopes_empty_and_injection_are_refused_and_the_command_is_never_built() {
        let good = ep("192.0.2.1", "22");
        let build = |local: &Endpoint, remote: &Endpoint| {
            disconnect_command(&ConnState::Established, local, remote)
        };
        let bad_hosts = [
            "",
            "*",
            "0.0.0.0",
            "::",
            "[::]",
            "fe80::1%eth0",
            "127.0.0.53%lo",
            "192.0.2.1;reboot",
            "192.0.2.1 ; reboot",
            "192.0.2.1\nreboot",
            "$(reboot)",
            "`reboot`",
            "192.0.2.1 sport",
            "192.0.2.1\t",
            "１９２.０.２.１",
            "192。0.2.1",
            "[2001:db8::1]",
            "2001:db8::1]",
            "192.0.2",
            "192.0.2.256",
            "localhost",
            "-K",
            "'192.0.2.1'",
            "\"192.0.2.1\"",
            "192.0.2.1|id",
            "192.0.2.1&",
            "192.0.2.1>x",
        ];
        for host in bad_hosts {
            let bad = ep(host, "22");
            assert!(build(&bad, &good).is_err(), "本地 {host:?}");
            assert!(build(&good, &bad).is_err(), "对端 {host:?}");
        }
        let bad_ports = [
            "",
            "*",
            "0",
            "65536",
            "-1",
            "+22",
            " 22",
            "22 ",
            "2 2",
            "22;id",
            "$(id)",
            "２２",
            "0x16",
            "22\n",
            "99999999999999999999",
        ];
        for port in bad_ports {
            let bad = ep("192.0.2.1", port);
            assert!(build(&bad, &good).is_err(), "本地端口 {port:?}");
            assert!(build(&good, &bad).is_err(), "对端端口 {port:?}");
        }
        // 通过校验的命令里只有常量、状态关键字、重新格式化的 IP 和数字。
        let command = build(&good, &ep("2001:db8::1", "65535")).expect("命令");
        let inner = command
            .strip_prefix("exec sh -c '")
            .and_then(|c| c.strip_suffix('\''))
            .expect("外层引号");
        assert!(!inner.contains('\''), "里面不能再有单引号：{inner}");
        for forbidden in [";", "$", "`", "\n", "&", "|", ">", "<", "sudo"] {
            assert!(!inner.contains(forbidden), "{forbidden} 出现在 {inner}");
        }
    }

    #[test]
    fn zero_padded_ports_are_normalised_not_passed_through() {
        let command = disconnect_command(
            &ConnState::Established,
            &ep("192.0.2.1", "0022"),
            &ep("192.0.2.2", "00080"),
        )
        .expect("命令");
        assert!(command.contains("sport = :22 ") && command.contains("dport = :80'"));
    }

    #[test]
    fn ss_k_results_are_classified() {
        let (local, remote) = (ep("192.0.2.10", "443"), ep("198.51.100.21", "40112"));
        let killed = "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\nESTAB 0 0 192.0.2.10:443 198.51.100.21:40112 users:((\"web-server\",pid=124,fd=11))\n";
        // 成功：退出码 0 且输出里有这一条（带不带表头都行）。
        assert_eq!(
            classify_disconnect(Some(0), killed, "", &local, &remote),
            Ok(())
        );
        let headerless = "ESTAB 0 0 192.0.2.10:443 198.51.100.21:40112\n";
        assert_eq!(
            classify_disconnect(Some(0), headerless, "", &local, &remote),
            Ok(())
        );
        // 0 条匹配：退出码 0，没有这一条（输出里是别的连接也算没有）。
        assert_eq!(
            classify_disconnect(Some(0), "", "", &local, &remote),
            Err(DisconnectError::NoMatch)
        );
        assert_eq!(
            classify_disconnect(
                Some(0),
                "ESTAB 0 0 192.0.2.10:443 198.51.100.99:40112\n",
                "",
                &local,
                &remote
            ),
            Err(DisconnectError::NoMatch)
        );
        // 内核不支持：几种常见措辞。
        for stderr in [
            "RTNETLINK answers: Operation not supported",
            "ss: kernel does not support SOCK_DESTROY",
            "Error: Operation not supported\n",
            "ss: invalid option -- 'K'\nUsage: ss [ OPTIONS ]",
            "ss: unrecognized option '--kill'",
        ] {
            assert_eq!(
                classify_disconnect(Some(1), "", stderr, &local, &remote),
                Err(DisconnectError::NotSupported),
                "{stderr}"
            );
        }
        // 没有权限：不会被当成别的错误，也不提 sudo。
        let denied = classify_disconnect(
            Some(1),
            "",
            "RTNETLINK answers: Operation not permitted",
            &local,
            &remote,
        )
        .expect_err("没权限");
        assert_eq!(denied, DisconnectError::NoPermission);
        assert_eq!(denied.to_string(), "需要 root 才能断开连接。");
        assert!(!denied.to_string().contains("sudo"));
        // 报错了退出码却是 0、输出里仍有这一行：权限 / 不支持优先。
        assert_eq!(
            classify_disconnect(Some(0), killed, "Operation not permitted", &local, &remote),
            Err(DisconnectError::NoPermission)
        );
        // 没有 ss。
        assert_eq!(
            classify_disconnect(Some(127), "", "sh: 1: ss: not found", &local, &remote),
            Err(DisconnectError::NoSs)
        );
        // 其余：给出第一行原因。
        assert_eq!(
            classify_disconnect(Some(2), "", "\n something odd \nmore", &local, &remote),
            Err(DisconnectError::Failed("something odd".to_owned()))
        );
        assert_eq!(
            classify_disconnect(None, "", "", &local, &remote),
            Err(DisconnectError::Failed(
                "ss 没有成功，也没有给出原因".to_owned()
            ))
        );
    }

    #[test]
    fn the_output_match_understands_ipv6_and_ignores_the_header_text() {
        let (local, remote) = (ep("2001:db8::10", "443"), ep("2001:db8::77", "40000"));
        let out = "ESTAB 0 0 [2001:db8::10]:443 [2001:db8::77]:40000\n";
        assert!(kill_output_mentions(out, &local, &remote));
        // 顺序反了（本地 / 对端对调）不算。
        let swapped = "ESTAB 0 0 [2001:db8::77]:40000 [2001:db8::10]:443\n";
        assert!(!kill_output_mentions(swapped, &local, &remote));
        assert!(!kill_output_mentions(
            "State Recv-Q Send-Q Local Address:Port Peer Address:Port\n",
            &local,
            &remote
        ));
    }

    const PS: &str = "\
@@ZSSH uid 0
@@ZSSH user root
@@ZSSH self 4242
@@ZSSH memtotal 16000000
@@ZSSH source procps
PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
1 0 root 0.0 0.3 48000 Ss 172800 /sbin/init
800 1 root 0.0 0.1 15000 Ss 172700 sshd: /usr/sbin/sshd -D [listener]
3090 800 root 0.0 0.1 11000 Ss 3600 sshd: root [priv]
3100 3090 root 0.0 0.1 9000 S 3600 sshd: root@notty
4242 3100 root 0.4 0.0 3000 Ss 0 sh -c exec
123 1 www-data 3.2 2.5 400000 S 86400 web-server: master process
124 123 www-data 3.2 2.5 400000 S 86400 web-server: worker process
812 1 app 62.3 12.4 1984000 Ssl 93784 /usr/bin/app-worker --queue=high
1200 800 root 0.0 0.1 11000 Ss 3600 sshd: user [priv]
1100 1 root 0.0 0.1 15000 Ss 172700 sshd: /usr/sbin/sshd -D -p 2222 [listener]
";

    #[test]
    fn killing_is_allowed_only_for_unprotected_visible_non_own_processes() {
        let report = report();
        let procs = parse_processes(PS).expect("进程");
        let ok = |local: &str, remote: &str, pid: u32| {
            kill_check(&report, row_with(&report, local, remote), Some(&procs), pid)
        };
        // 普通进程：可以。
        assert_eq!(ok("192.0.2.10:8081", "198.51.100.30:50000", 812), Ok(()));
        assert_eq!(
            ok("[2001:db8::10]:443", "[2001:db8::77]:40000", 124),
            Ok(())
        );
        // 受保护：和 U18 同一份判定（PID 1、sshd 监听者、当前会话链……）。
        let listener = report
            .rows
            .iter()
            .find(|r| r.local.display() == "0.0.0.0:80")
            .expect("80");
        assert_eq!(
            kill_check(&report, listener, Some(&procs), 123),
            Ok(()),
            "web-server 主进程不受保护"
        );
        let sshd_row = row_with(&report, "192.0.2.10:22", "198.51.100.8:40000");
        assert_eq!(
            kill_check(&report, sshd_row, Some(&procs), 1200),
            Ok(()),
            "别的用户的 sshd 会话进程不受保护（和 U18 一致）"
        );
        // 进程列表里已经没有这个 PID。
        assert_eq!(
            ok("192.0.2.10:8081", "198.51.100.30:50000", 9999),
            Err(KillBlock::NotRunning)
        );
        // 没读到进程列表。
        assert_eq!(
            kill_check(
                &report,
                row_with(&report, "192.0.2.10:8081", "198.51.100.30:50000"),
                None,
                812
            ),
            Err(KillBlock::ProcessesUnavailable)
        );
        // 进程列空（没权限看到）：没有可结束的进程。
        assert_eq!(
            ok("192.0.2.10:443", "198.51.100.21:40112", 812),
            Err(KillBlock::NoProcessInfo)
        );
        // 自己的连接：断开和结束进程都置灰，压过一切。
        let own = row_with(&report, "192.0.2.10:22", "198.51.100.7:51234");
        for pid in [3090, 3100, 812] {
            assert_eq!(
                kill_check(&report, own, Some(&procs), pid),
                Err(KillBlock::OwnConnection)
            );
        }
        // 受保护的：把 sshd 监听者挂到一行上。
        let mut protected_row = row_with(&report, "192.0.2.10:8081", "198.51.100.30:50000").clone();
        // 800 在承载本次会话的链上（监听者 → [priv] → notty）；1100 是另一个 sshd 监听者。
        protected_row.processes = vec![ProcRef {
            pid: 800,
            name: "sshd".into(),
        }];
        assert_eq!(
            kill_check(&report, &protected_row, Some(&procs), 800),
            Err(KillBlock::Protected(Protection::SshSession))
        );
        protected_row.processes = vec![ProcRef {
            pid: 1100,
            name: "sshd".into(),
        }];
        assert_eq!(
            kill_check(&report, &protected_row, Some(&procs), 1100),
            Err(KillBlock::Protected(Protection::SshDaemon))
        );
        protected_row.processes = vec![ProcRef {
            pid: 1,
            name: "init".into(),
        }];
        assert_eq!(
            kill_check(&report, &protected_row, Some(&procs), 1),
            Err(KillBlock::Protected(Protection::Init))
        );
        protected_row.processes = vec![ProcRef {
            pid: 3100,
            name: "sshd".into(),
        }];
        assert_eq!(
            kill_check(&report, &protected_row, Some(&procs), 3100),
            Err(KillBlock::Protected(Protection::SshSession))
        );
    }

    #[test]
    fn kill_block_reasons_mention_the_protection_and_the_non_root_limit() {
        assert_eq!(
            KillBlock::Protected(Protection::CriticalService).reason(),
            "受保护的系统进程不能结束：关键系统服务"
        );
        assert!(KillBlock::NoProcessInfo
            .reason()
            .contains("非 root 登录时看不到其他用户的进程"));
        assert_eq!(
            KillBlock::OwnConnection.reason(),
            "ZeeTerm 正在使用这条连接"
        );
        assert_eq!(
            KillBlock::Protected(Protection::SshDaemon).short(),
            Protection::SshDaemon.reason()
        );
    }

    #[test]
    fn the_menu_lists_at_most_six_distinct_processes() {
        let report = report();
        let nginx = row_with(&report, "0.0.0.0:80", "0.0.0.0:*");
        let (listed, hidden) = menu_processes(nginx);
        assert_eq!(listed.len(), 2);
        assert_eq!(hidden, 0);
        let mut many = nginx.clone();
        many.processes = (1..=9)
            .map(|pid| ProcRef {
                pid,
                name: "app-worker".into(),
            })
            .chain(std::iter::once(ProcRef {
                pid: 3,
                name: "app-worker".into(),
            }))
            .collect();
        let (listed, hidden) = menu_processes(&many);
        assert_eq!(
            listed.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
        assert_eq!(hidden, 3, "重复的 PID 3 不重复计数");
    }
}
