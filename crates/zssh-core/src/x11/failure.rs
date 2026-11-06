//! X11 转发失败的分类。
//!
//! 同一个值两处用：界面按 [`X11Failure::code`] 和字段做结构化展示（比如"本机没有 X 服务器"
//! 时显示"一键安装 VcXsrv"按钮）；终端里按 [`X11Failure::terminal_notice`] 显示一句中文，
//! 告诉用户为什么这次没有图形转发、怎么办。X11 失败从来不是致命的：shell 照常打开。

use std::sync::RwLock;

use serde::Serialize;

use crate::error::{ErrorWithCode, Reason};

/// 界面装上的「终端提示」生成函数：核心不挑语言，终端里那一行由界面按当前语言出。
/// 没装（测试、没有界面的用法）就用中文的 [`X11Failure::advice`]。
type NoticeRenderer = fn(&X11Failure) -> String;
static NOTICE_RENDERER: RwLock<Option<NoticeRenderer>> = RwLock::new(None);

/// 装上终端提示的生成函数（界面启动时调一次）。
pub fn set_notice_renderer(renderer: NoticeRenderer) {
    if let Ok(mut slot) = NOTICE_RENDERER.write() {
        *slot = Some(renderer);
    }
}

/// 一次 X11 失败。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum X11Failure {
    /// 服务器回了 `CHANNEL_FAILURE`：sshd 没开 `X11Forwarding`，或者服务器上没有 `xauth`。
    #[error("服务器拒绝了 X11 转发请求")]
    ServerRefused,
    /// 服务器没有回复 X11 请求（超时 / 通道提前关闭 / 回复前塞来太多数据）。
    #[error("服务器没有回复 X11 转发请求：{detail}")]
    ServerNoReply { detail: Reason },
    /// 本机没有可用的 X 服务器（没人监听那个端口，或者根本没装）。
    /// `installable`：能不能由 ZeeTerm 一键安装 VcXsrv（界面据此决定要不要显示安装入口）。
    #[error("本机没有可用的 X 服务器（{display}）")]
    NoLocalXServer { display: String, installable: bool },
    /// 环境里没有 `DISPLAY`（纯 Wayland 又没有 Xwayland、或者 SSH 登录的无界面环境）：Unix 上本机没有 X 服务器可用。
    #[error("本机没有 X 服务器（环境里没有 DISPLAY）")]
    NoXDisplay,
    /// 本机 X 服务器接受了连接但拒绝了认证。`passthrough`：我们手里没有真 cookie（用户自己的 X 服务器）。
    #[error("本机 X 服务器（{display}）拒绝了连接：{reason}")]
    LocalXServerRejected {
        display: String,
        reason: String,
        passthrough: bool,
    },
    /// 连本机 X 服务器超时。
    #[error("连接本机 X 服务器（{display}）超时")]
    LocalXServerTimeout { display: String },
    /// 本机 X 服务器是 ZeeTerm 自己要拉起的，但没起来。
    #[error("没能启动本机 X 服务器：{reason}")]
    LaunchFailed { reason: Reason },
    /// 远端送来的 X11 连接带的 cookie 不属于这条连接上任何一个终端（已拒绝）。
    #[error("收到一个 X11 连接，但它的认证信息不属于任何终端，已拒绝")]
    UnknownCookie,
    /// 握手包不合法、太大或迟迟没发完。
    #[error("X11 连接的握手包不合法、太大或超时，已关闭")]
    BadSetup,
    /// 同时开着的 X11 连接太多。
    #[error("同时打开的 X11 连接太多，已拒绝新的连接")]
    TooManyChannels,
    /// 这条连接上同时开 X11 的终端太多，或没能生成随机 cookie。
    #[error("没能登记这个终端的 X11 转发：{reason}")]
    NotRegistered { reason: Reason },
}

impl X11Failure {
    /// 稳定的英文代码，界面和日志用；同一次登记里同一个代码只在终端提示一次。
    pub fn code(&self) -> &'static str {
        match self {
            Self::ServerRefused => "server_refused",
            Self::ServerNoReply { .. } => "server_no_reply",
            Self::NoLocalXServer { .. } => "no_local_x_server",
            Self::NoXDisplay => "no_x_display",
            Self::LocalXServerRejected { .. } => "local_x_server_rejected",
            Self::LocalXServerTimeout { .. } => "local_x_server_timeout",
            Self::LaunchFailed { .. } => "launch_failed",
            Self::UnknownCookie => "unknown_cookie",
            Self::BadSetup => "bad_setup",
            Self::TooManyChannels => "too_many_channels",
            Self::NotRegistered { .. } => "not_registered",
        }
    }

    /// 写进终端的一句话（含颜色和换行）。
    pub fn terminal_notice(&self) -> String {
        let line = NOTICE_RENDERER
            .read()
            .ok()
            .and_then(|slot| *slot)
            .map_or_else(|| format!("X11：{}", self.advice()), |render| render(self));
        format!("\r\n\x1b[33m[ZeeTerm] {line}\x1b[0m\r\n")
    }

    /// 原因 + 怎么办。
    pub fn advice(&self) -> String {
        match self {
            Self::ServerRefused => "服务器没有同意 X11 转发。请在服务器的 sshd_config 里设置 \
                X11Forwarding yes 并安装 xauth，已按普通终端打开。"
                .to_owned(),
            Self::ServerNoReply { detail } => {
                format!("服务器没有回复 X11 转发请求（{detail}），已按普通终端打开。")
            }
            Self::NoLocalXServer {
                display,
                installable,
            } => {
                let tail = if *installable {
                    "可以在设置里一键安装 VcXsrv，"
                } else {
                    ""
                };
                if cfg!(unix) {
                    format!(
                        "本机没有可用的 X 服务器（连接 {display} 失败）。{tail}请确认当前桌面会话的 X 服务器\
                         （Xorg 或 Xwayland）在运行、DISPLAY 环境变量指向它，然后重新连接。"
                    )
                } else {
                    format!(
                        "本机没有可用的 X 服务器（连接 {display} 失败）。{tail}\
                         或者先启动 X410 / MobaXterm 等 X 服务器再重新连接。"
                    )
                }
            }
            Self::NoXDisplay => "本机没有 X 服务器（环境里没有 DISPLAY）。Linux 下需要 Xwayland\
                （Wayland 桌面）或 Xorg 提供 X 服务器；纯 Wayland 且没有 Xwayland、或通过 SSH 登录\
                的无界面环境没有它。已按普通终端打开。"
                .to_owned(),
            Self::LocalXServerRejected {
                display,
                reason,
                passthrough,
            } => {
                let hint = match (cfg!(unix), *passthrough) {
                    (true, true) => {
                        "ZeeTerm 没有在授权文件（XAUTHORITY 或 ~/.Xauthority）里找到这个显示的认证信息：\
                         请确认 XAUTHORITY 指向当前会话的授权文件，或用 xhost 允许当前用户连接\
                         （例如 xhost +SI:localuser:$USER）。"
                    }
                    (true, false) => {
                        "授权文件里这个显示的认证信息被拒绝了：请确认 XAUTHORITY 指向当前会话的授权文件。"
                    }
                    (false, true) => {
                        "这是你自己启动的 X 服务器，ZeeTerm 拿不到它的认证信息：请在它的设置里\
                         关闭访问控制（例如 VcXsrv 勾选 Disable access control），或配置认证后再试。"
                    }
                    (false, false) => "请关闭它再让 ZeeTerm 重新启动一个。",
                };
                format!("本机 X 服务器（{display}）拒绝了连接（{reason}）。{hint}")
            }
            Self::LocalXServerTimeout { display } => {
                format!("连接本机 X 服务器（{display}）超时，请确认它在运行且没被防火墙拦住。")
            }
            Self::LaunchFailed { reason } => {
                format!("没能启动本机 X 服务器：{reason}。已按普通终端打开。")
            }
            Self::UnknownCookie => {
                "远端送来的 X11 连接认证信息不对，已拒绝（可能是别的终端已关闭）。".to_owned()
            }
            Self::BadSetup => "远端送来的 X11 连接握手不合法或超时，已关闭。".to_owned(),
            Self::TooManyChannels => "同时打开的 X11 连接太多，已拒绝新的连接。".to_owned(),
            Self::NotRegistered { reason } => {
                format!("没能为这个终端开启 X11 转发：{reason}。已按普通终端打开。")
            }
        }
    }
}

/// 错误码与结构化参数：界面按它们出当前语言的整句（`Display` / `advice()` 的中文只留给日志和兜底）。
impl ErrorWithCode for X11Failure {
    fn code(&self) -> &'static str {
        match self {
            Self::ServerRefused => "x11f.server_refused",
            Self::ServerNoReply { .. } => "x11f.server_no_reply",
            Self::NoLocalXServer { .. } => "x11f.no_local_x_server",
            Self::NoXDisplay => "x11f.no_x_display",
            Self::LocalXServerRejected { .. } => "x11f.local_x_server_rejected",
            Self::LocalXServerTimeout { .. } => "x11f.local_x_server_timeout",
            Self::LaunchFailed { .. } => "x11f.launch_failed",
            Self::UnknownCookie => "x11f.unknown_cookie",
            Self::BadSetup => "x11f.bad_setup",
            Self::TooManyChannels => "x11f.too_many_channels",
            Self::NotRegistered { .. } => "x11f.not_registered",
        }
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::ServerRefused
            | Self::NoXDisplay
            | Self::UnknownCookie
            | Self::BadSetup
            | Self::TooManyChannels => Vec::new(),
            Self::ServerNoReply { detail } => vec![("detail", detail.zh().to_owned())],
            Self::NoLocalXServer {
                display,
                installable,
            } => vec![
                ("display", display.clone()),
                ("installable", u8::from(*installable).to_string()),
            ],
            Self::LocalXServerRejected {
                display,
                reason,
                passthrough,
            } => vec![
                ("display", display.clone()),
                ("reason", reason.clone()),
                ("passthrough", u8::from(*passthrough).to_string()),
            ],
            Self::LocalXServerTimeout { display } => vec![("display", display.clone())],
            Self::LaunchFailed { reason } | Self::NotRegistered { reason } => {
                vec![("detail", reason.zh().to_owned())]
            }
        }
    }

    fn causes(&self) -> Vec<(&'static str, &dyn ErrorWithCode)> {
        match self {
            Self::ServerNoReply { detail } => vec![("detail", detail as &dyn ErrorWithCode)],
            Self::LaunchFailed { reason } | Self::NotRegistered { reason } => {
                vec![("detail", reason as &dyn ErrorWithCode)]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<X11Failure> {
        vec![
            X11Failure::ServerRefused,
            X11Failure::ServerNoReply {
                detail: "超时".into(),
            },
            X11Failure::NoLocalXServer {
                display: "127.0.0.1:6020".into(),
                installable: true,
            },
            X11Failure::NoXDisplay,
            X11Failure::LocalXServerRejected {
                display: "localhost:6000".into(),
                reason: "Authorization required".into(),
                passthrough: true,
            },
            X11Failure::LocalXServerTimeout {
                display: "localhost:6000".into(),
            },
            X11Failure::LaunchFailed {
                reason: "找不到程序".into(),
            },
            X11Failure::UnknownCookie,
            X11Failure::BadSetup,
            X11Failure::TooManyChannels,
            X11Failure::NotRegistered {
                reason: "太多".into(),
            },
        ]
    }

    #[test]
    fn every_failure_has_a_distinct_code_and_a_chinese_notice() {
        let failures = all();
        let mut codes: Vec<&str> = failures.iter().map(X11Failure::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), failures.len(), "代码必须互不相同");
        for failure in &failures {
            let notice = failure.terminal_notice();
            assert!(notice.starts_with("\r\n"), "{notice:?}");
            assert!(notice.ends_with("\x1b[0m\r\n"), "{notice:?}");
            assert!(notice.contains("[ZeeTerm] X11："), "{notice:?}");
            assert!(!failure.to_string().is_empty());
        }
    }

    #[test]
    fn notices_tell_the_user_what_to_do() {
        assert!(X11Failure::ServerRefused.advice().contains("X11Forwarding"));
        assert!(X11Failure::ServerRefused.advice().contains("xauth"));
        let install = X11Failure::NoLocalXServer {
            display: "x".into(),
            installable: true,
        };
        assert!(install.advice().contains("一键安装 VcXsrv"));
        let no_install = X11Failure::NoLocalXServer {
            display: "x".into(),
            installable: false,
        };
        assert!(!no_install.advice().contains("一键安装"));
        let own = X11Failure::LocalXServerRejected {
            display: "x".into(),
            reason: "r".into(),
            passthrough: true,
        };
        // 授权信息拿不到时的建议因系统而异：Windows 是关闭 X 服务器的访问控制，Unix 是检查授权文件。
        if cfg!(unix) {
            assert!(own.advice().contains("XAUTHORITY"));
            assert!(own.advice().contains("xhost"));
        } else {
            assert!(own.advice().contains("关闭访问控制"));
        }
        let ours = X11Failure::LocalXServerRejected {
            display: "x".into(),
            reason: "r".into(),
            passthrough: false,
        };
        assert!(!ours.advice().contains("关闭访问控制"));
    }

    #[test]
    fn a_missing_display_variable_explains_what_linux_needs() {
        let failure = X11Failure::NoXDisplay;
        let advice = failure.advice();
        assert!(
            advice.contains("Xwayland") && advice.contains("Xorg"),
            "{advice}"
        );
        assert!(advice.contains("DISPLAY"), "{advice}");
        assert!(!advice.contains("VcXsrv"), "不能提示装 VcXsrv：{advice}");
        assert_eq!(failure.code(), "no_x_display");
        assert_eq!(ErrorWithCode::code(&failure), "x11f.no_x_display");
        assert!(ErrorWithCode::params(&failure).is_empty());
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["kind"], "no_x_display");
    }

    #[test]
    fn the_unreachable_server_advice_matches_the_system() {
        let advice = X11Failure::NoLocalXServer {
            display: ":0".into(),
            installable: false,
        }
        .advice();
        if cfg!(unix) {
            assert!(
                advice.contains("Xwayland") && !advice.contains("X410"),
                "{advice}"
            );
        } else {
            assert!(advice.contains("X410"), "{advice}");
        }
    }

    #[test]
    fn failures_serialize_with_a_kind_tag_for_the_ui() {
        let json = serde_json::to_value(X11Failure::NoLocalXServer {
            display: "localhost:6000".into(),
            installable: true,
        })
        .unwrap();
        assert_eq!(json["kind"], "no_local_x_server");
        assert_eq!(json["installable"], true);
    }
}
