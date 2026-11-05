//! 连接 / 进程管理里"为什么不能断开 / 结束"和"没成功"的文案。
//!
//! 核心库里这些原因本来就是类型（`Protection`、`KillBlock`、`DisconnectBlock`、`KillError`、
//! `DisconnectError`），界面按类型和当前语言出词；核心的中文 `reason()` / `Display` 留给日志、
//! 模型和测试。进程名、用户名、服务器回的原文（`Failed(..)`）作为参数原样保留。

use zssh_core::ssh::{DisconnectBlock, DisconnectError, KillBlock, KillError, Protection};

use crate::i18n::{Lang, Msg, current, t};

#[derive(Clone, Copy, Debug)]
enum OpsMsg {
    ProtInit,
    ProtKernelThread,
    ProtSshSession,
    ProtSshDaemon,
    ProtCriticalService,
    DiscOwnConnection,
    DiscUdp,
    DiscListening,
    DiscTimeWait,
    DiscNotConnected,
    DiscNoSs,
    KillBlockNoProcessInfo,
    KillBlockProcessesUnavailable,
    KillBlockNotRunning,
    KillBlockProtected,
    ShortOwnConnection,
    ShortNoProcessInfo,
    ShortProcessesUnavailable,
    ShortNotRunning,
    DisconnectInvalidAddress,
    DisconnectBlocked,
    DisconnectGone,
    DisconnectNotSupported,
    DisconnectNoPermission,
    DisconnectNoSs,
    DisconnectNoMatch,
    DisconnectTimeout,
    DisconnectFailed,
    KillInvalidPid,
    KillNoPermissionOwner,
    KillYouAre,
    KillNoSuchProcess,
    KillChanged,
    KillProtected,
    KillTimeout,
    KillFailed,
}

impl Msg for OpsMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ProtInit => "系统初始化进程",
            Self::ProtKernelThread => "内核线程",
            Self::ProtSshSession => "当前 SSH 会话",
            Self::ProtSshDaemon => "结束会断开所有 SSH 连接",
            Self::ProtCriticalService => "关键系统服务",
            Self::DiscOwnConnection => "ZeeTerm 正在使用这条连接",
            Self::DiscUdp => "UDP 没有连接可断",
            Self::DiscListening => "监听端口没有对端，要停掉请结束进程",
            Self::DiscTimeWait => "TIME-WAIT 已经在关闭",
            Self::DiscNotConnected => "这条连接没有对端，不需要断开",
            Self::DiscNoSs => "服务器上没有 ss 命令，无法单独断开连接",
            Self::KillBlockNoProcessInfo => {
                "看不到这条连接属于哪个进程（非 root 登录时看不到其他用户的进程）"
            }
            Self::KillBlockProcessesUnavailable => "还没读到进程列表，无法确认这个进程能不能结束",
            Self::KillBlockNotRunning => "这个进程已经不在了",
            Self::KillBlockProtected => "受保护的系统进程不能结束：{reason}",
            Self::ShortOwnConnection => "ZeeTerm 正在使用",
            Self::ShortNoProcessInfo => "看不到进程",
            Self::ShortProcessesUnavailable => "没读到进程列表",
            Self::ShortNotRunning => "进程已不在",
            Self::DisconnectInvalidAddress => "地址或端口不合法，没有发出任何命令：{detail}",
            Self::DisconnectBlocked => "不能断开这条连接：{reason}。",
            Self::DisconnectGone => "这条连接已经不在了，列表已刷新。",
            Self::DisconnectNotSupported => {
                "这台服务器的内核不支持单独断开连接，可以改为结束占用它的进程。"
            }
            Self::DisconnectNoPermission => "需要 root 才能断开连接。",
            Self::DisconnectNoSs => "服务器上没有 ss 命令，无法单独断开连接。",
            Self::DisconnectNoMatch => {
                "没有找到这条连接（可能已经关闭）；如果刷新后它还在，多半是内核不支持单独断开，可以改为结束占用它的进程。"
            }
            Self::DisconnectTimeout | Self::KillTimeout => "等了 5 秒没有结果。",
            Self::DisconnectFailed => "断开失败：{detail}",
            Self::KillInvalidPid => "PID 不合法（只能是 1 到 {max} 的纯数字）。",
            Self::KillNoPermissionOwner => {
                "没有权限结束这个进程（属于用户 {owner}{me}），需要 root 或 sudo。"
            }
            Self::KillYouAre => "，你现在是 {user}",
            Self::KillNoSuchProcess => "这个进程已经不存在了。",
            Self::KillChanged => {
                "这个 PID 现在对应的已经不是你选中的那个进程，列表已刷新，请重新选择。"
            }
            Self::KillProtected => "这是受保护的系统进程（{reason}），不能结束。",
            Self::KillFailed => "结束失败：{detail}",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ProtInit => "System init process",
            Self::ProtKernelThread => "Kernel thread",
            Self::ProtSshSession => "The current SSH session",
            Self::ProtSshDaemon => "Ending it would drop every SSH connection",
            Self::ProtCriticalService => "Critical system service",
            Self::DiscOwnConnection => "ZeeTerm is using this connection",
            Self::DiscUdp => "UDP has no connection to drop",
            Self::DiscListening => "A listening port has no peer; end the process to stop it",
            Self::DiscTimeWait => "TIME-WAIT is already closing",
            Self::DiscNotConnected => "This connection has no peer, so there is nothing to drop",
            Self::DiscNoSs => {
                "The server has no ss command, so a single connection cannot be dropped"
            }
            Self::KillBlockNoProcessInfo => {
                "Cannot see which process owns this connection (without root you cannot see other users' processes)"
            }
            Self::KillBlockProcessesUnavailable => {
                "The process list has not been read yet, so it cannot be confirmed whether this process can be ended"
            }
            Self::KillBlockNotRunning => "This process is gone",
            Self::KillBlockProtected => "Protected system processes cannot be ended: {reason}",
            Self::ShortOwnConnection => "In use by ZeeTerm",
            Self::ShortNoProcessInfo => "No process info",
            Self::ShortProcessesUnavailable => "No process list",
            Self::ShortNotRunning => "Process gone",
            Self::DisconnectInvalidAddress => {
                "The address or port is invalid; no command was sent: {detail}"
            }
            Self::DisconnectBlocked => "This connection cannot be dropped: {reason}.",
            Self::DisconnectGone => "This connection no longer exists; the list was refreshed.",
            Self::DisconnectNotSupported => {
                "This server's kernel does not support dropping a single connection; end the process that holds it instead."
            }
            Self::DisconnectNoPermission => "Root is required to drop a connection.",
            Self::DisconnectNoSs => {
                "The server has no ss command, so a single connection cannot be dropped."
            }
            Self::DisconnectNoMatch => {
                "The connection was not found (it may already be closed); if it is still there after refreshing, the kernel most likely does not support dropping a single connection; end the process that holds it instead."
            }
            Self::DisconnectTimeout | Self::KillTimeout => "No result after 5 seconds.",
            Self::DisconnectFailed => "Failed to drop the connection: {detail}",
            Self::KillInvalidPid => "The PID is invalid (digits only, from 1 to {max}).",
            Self::KillNoPermissionOwner => {
                "No permission to end this process (owned by {owner}{me}); root or sudo is required."
            }
            Self::KillYouAre => ", you are {user}",
            Self::KillNoSuchProcess => "This process no longer exists.",
            Self::KillChanged => {
                "This PID no longer belongs to the process you selected; the list was refreshed, please select again."
            }
            Self::KillProtected => {
                "This is a protected system process ({reason}) and cannot be ended."
            }
            Self::KillFailed => "Failed to end the process: {detail}",
        }
    }
}

/// 把 `{name}` 占位符换成参数；模板和参数都是这里自己写的，对不上就原样留着。
fn fill(lang: Lang, msg: OpsMsg, params: &[(&str, &str)]) -> String {
    let mut text = match lang {
        Lang::Zh => msg.zh(),
        Lang::En => msg.en(),
    }
    .to_owned();
    for (name, value) in params {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// 进程为什么受保护（悬停说明）。
pub(crate) fn protection(protection: Protection) -> &'static str {
    t(match protection {
        Protection::Init => OpsMsg::ProtInit,
        Protection::KernelThread => OpsMsg::ProtKernelThread,
        Protection::SshSession => OpsMsg::ProtSshSession,
        Protection::SshDaemon => OpsMsg::ProtSshDaemon,
        Protection::CriticalService => OpsMsg::ProtCriticalService,
    })
}

/// 一条连接为什么不能断开。
pub(crate) fn disconnect_block(block: DisconnectBlock) -> &'static str {
    t(match block {
        DisconnectBlock::OwnConnection => OpsMsg::DiscOwnConnection,
        DisconnectBlock::Udp => OpsMsg::DiscUdp,
        DisconnectBlock::Listening => OpsMsg::DiscListening,
        DisconnectBlock::TimeWait => OpsMsg::DiscTimeWait,
        DisconnectBlock::NotConnected => OpsMsg::DiscNotConnected,
        DisconnectBlock::NoSs => OpsMsg::DiscNoSs,
    })
}

/// 一个进程为什么不能结束（完整句子）。
pub(crate) fn kill_block_reason(block: KillBlock) -> String {
    kill_block_reason_in(current(), block)
}

pub(crate) fn kill_block_reason_in(lang: Lang, block: KillBlock) -> String {
    let pick = |msg: OpsMsg| fill(lang, msg, &[]);
    match block {
        KillBlock::OwnConnection => pick(OpsMsg::DiscOwnConnection),
        KillBlock::NoProcessInfo => pick(OpsMsg::KillBlockNoProcessInfo),
        KillBlock::ProcessesUnavailable => pick(OpsMsg::KillBlockProcessesUnavailable),
        KillBlock::NotRunning => pick(OpsMsg::KillBlockNotRunning),
        KillBlock::Protected(why) => fill(
            lang,
            OpsMsg::KillBlockProtected,
            &[("reason", protection_in(lang, why))],
        ),
    }
}

/// 菜单右侧的简短原因。
pub(crate) fn kill_block_short(block: KillBlock) -> &'static str {
    match block {
        KillBlock::OwnConnection => t(OpsMsg::ShortOwnConnection),
        KillBlock::NoProcessInfo => t(OpsMsg::ShortNoProcessInfo),
        KillBlock::ProcessesUnavailable => t(OpsMsg::ShortProcessesUnavailable),
        KillBlock::NotRunning => t(OpsMsg::ShortNotRunning),
        KillBlock::Protected(why) => protection(why),
    }
}

fn protection_in(lang: Lang, why: Protection) -> &'static str {
    let msg = match why {
        Protection::Init => OpsMsg::ProtInit,
        Protection::KernelThread => OpsMsg::ProtKernelThread,
        Protection::SshSession => OpsMsg::ProtSshSession,
        Protection::SshDaemon => OpsMsg::ProtSshDaemon,
        Protection::CriticalService => OpsMsg::ProtCriticalService,
    };
    match lang {
        Lang::Zh => msg.zh(),
        Lang::En => msg.en(),
    }
}

fn disconnect_block_in(lang: Lang, block: DisconnectBlock) -> &'static str {
    let msg = match block {
        DisconnectBlock::OwnConnection => OpsMsg::DiscOwnConnection,
        DisconnectBlock::Udp => OpsMsg::DiscUdp,
        DisconnectBlock::Listening => OpsMsg::DiscListening,
        DisconnectBlock::TimeWait => OpsMsg::DiscTimeWait,
        DisconnectBlock::NotConnected => OpsMsg::DiscNotConnected,
        DisconnectBlock::NoSs => OpsMsg::DiscNoSs,
    };
    match lang {
        Lang::Zh => msg.zh(),
        Lang::En => msg.en(),
    }
}

/// 断开连接没成功的提示。
pub(crate) fn disconnect_error(err: &DisconnectError) -> String {
    disconnect_error_in(current(), err)
}

pub(crate) fn disconnect_error_in(lang: Lang, err: &DisconnectError) -> String {
    let plain = |msg: OpsMsg| fill(lang, msg, &[]);
    match err {
        DisconnectError::InvalidAddress(detail) => fill(
            lang,
            OpsMsg::DisconnectInvalidAddress,
            &[("detail", detail)],
        ),
        DisconnectError::Blocked(block) => fill(
            lang,
            OpsMsg::DisconnectBlocked,
            &[("reason", disconnect_block_in(lang, *block))],
        ),
        DisconnectError::Gone => plain(OpsMsg::DisconnectGone),
        DisconnectError::NotSupported => plain(OpsMsg::DisconnectNotSupported),
        DisconnectError::NoPermission => plain(OpsMsg::DisconnectNoPermission),
        DisconnectError::NoSs => plain(OpsMsg::DisconnectNoSs),
        DisconnectError::NoMatch => plain(OpsMsg::DisconnectNoMatch),
        DisconnectError::Timeout => plain(OpsMsg::DisconnectTimeout),
        DisconnectError::Failed(detail) => {
            fill(lang, OpsMsg::DisconnectFailed, &[("detail", detail)])
        }
    }
}

/// 结束进程没成功的提示；没权限时点出进程属于谁、当前用户是谁。
pub(crate) fn kill_error(err: &KillError, owner: &str, login_user: Option<&str>) -> String {
    kill_error_in(current(), err, owner, login_user)
}

pub(crate) fn kill_error_in(
    lang: Lang,
    err: &KillError,
    owner: &str,
    login_user: Option<&str>,
) -> String {
    let plain = |msg: OpsMsg| fill(lang, msg, &[]);
    match err {
        KillError::InvalidPid => fill(
            lang,
            OpsMsg::KillInvalidPid,
            &[("max", &zssh_core::ssh::processes::PID_MAX.to_string())],
        ),
        KillError::NoPermission => {
            let me = login_user
                .filter(|user| !user.is_empty())
                .map(|user| fill(lang, OpsMsg::KillYouAre, &[("user", user)]))
                .unwrap_or_default();
            fill(
                lang,
                OpsMsg::KillNoPermissionOwner,
                &[("owner", owner), ("me", &me)],
            )
        }
        KillError::NoSuchProcess => plain(OpsMsg::KillNoSuchProcess),
        KillError::Changed => plain(OpsMsg::KillChanged),
        KillError::Protected(why) => fill(
            lang,
            OpsMsg::KillProtected,
            &[("reason", protection_in(lang, *why))],
        ),
        KillError::Timeout => plain(OpsMsg::KillTimeout),
        KillError::Failed(detail) => fill(lang, OpsMsg::KillFailed, &[("detail", detail)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }

    const PROTECTIONS: [Protection; 5] = [
        Protection::Init,
        Protection::KernelThread,
        Protection::SshSession,
        Protection::SshDaemon,
        Protection::CriticalService,
    ];

    /// 中文和核心库的 `reason()` / `Display` 逐字相同；英文没有汉字。
    #[test]
    fn protections_and_blocks_match_the_core_in_chinese_and_are_english_otherwise() {
        for why in PROTECTIONS {
            assert_eq!(protection_in(Lang::Zh, why), why.reason());
            assert!(!has_cjk(protection_in(Lang::En, why)), "{why:?}");
        }
        for block in [
            DisconnectBlock::OwnConnection,
            DisconnectBlock::Udp,
            DisconnectBlock::Listening,
            DisconnectBlock::TimeWait,
            DisconnectBlock::NotConnected,
            DisconnectBlock::NoSs,
        ] {
            assert_eq!(disconnect_block_in(Lang::Zh, block), block.reason());
            assert!(!has_cjk(disconnect_block_in(Lang::En, block)), "{block:?}");
        }
        let mut blocks = vec![
            KillBlock::OwnConnection,
            KillBlock::NoProcessInfo,
            KillBlock::ProcessesUnavailable,
            KillBlock::NotRunning,
        ];
        blocks.extend(PROTECTIONS.map(KillBlock::Protected));
        for block in blocks {
            assert_eq!(kill_block_reason_in(Lang::Zh, block), block.reason());
            assert!(
                !has_cjk(&kill_block_reason_in(Lang::En, block)),
                "{block:?}"
            );
        }
    }

    #[test]
    fn disconnect_and_kill_errors_match_the_core_in_chinese_and_are_english_otherwise() {
        let disconnects = [
            DisconnectError::Blocked(DisconnectBlock::Udp),
            DisconnectError::Gone,
            DisconnectError::NotSupported,
            DisconnectError::NoPermission,
            DisconnectError::NoSs,
            DisconnectError::NoMatch,
            DisconnectError::Timeout,
            DisconnectError::Failed("boom".into()),
        ];
        for err in disconnects {
            assert_eq!(disconnect_error_in(Lang::Zh, &err), err.to_string());
            assert!(!has_cjk(&disconnect_error_in(Lang::En, &err)), "{err:?}");
        }
        let kills = [
            KillError::InvalidPid,
            KillError::NoSuchProcess,
            KillError::Changed,
            KillError::Protected(Protection::SshDaemon),
            KillError::Timeout,
            KillError::Failed("boom".into()),
        ];
        for err in kills {
            assert_eq!(
                kill_error_in(Lang::Zh, &err, "root", None),
                err.describe("root", None)
            );
            assert!(
                !has_cjk(&kill_error_in(Lang::En, &err, "root", None)),
                "{err:?}"
            );
        }
        // 没权限：点出进程属于谁、你现在是谁（用户名原样保留）。
        let denied = KillError::NoPermission;
        assert_eq!(
            kill_error_in(Lang::Zh, &denied, "www", Some("app")),
            denied.describe("www", Some("app"))
        );
        assert_eq!(
            kill_error_in(Lang::En, &denied, "www", Some("app")),
            "No permission to end this process (owned by www, you are app); root or sudo is required."
        );
        assert_eq!(
            kill_error_in(Lang::En, &denied, "www", None),
            "No permission to end this process (owned by www); root or sudo is required."
        );
    }
}

/// 协议的显示名：SSH / Telnet 是专有名词，串口按界面语言（核心的 `Protocol::label` 只有中文）。
pub(crate) fn protocol_label(protocol: zssh_core::model::Protocol) -> &'static str {
    use zssh_core::model::Protocol;
    match protocol {
        Protocol::Ssh => "SSH",
        Protocol::Telnet => "Telnet",
        Protocol::Serial => t(ProtocolMsg::Serial),
    }
}

#[derive(Clone, Copy)]
enum ProtocolMsg {
    Serial,
}

impl Msg for ProtocolMsg {
    fn zh(&self) -> &'static str {
        "串口"
    }

    fn en(&self) -> &'static str {
        "Serial"
    }
}
