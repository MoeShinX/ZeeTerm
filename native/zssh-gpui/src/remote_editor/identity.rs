//! 编辑会话 / 草稿绑定的服务器身份（W07）。
//!
//! 草稿和编辑器只记主机库 id：主机记录后来被改成指向另一台机器，按 id 重新拨号就会连上另一台
//! 服务器，把内容静默写到那里的同一路径上。所以打开文件时记下**实际连到的**服务器——地址、端口、
//! 用户名，以及连接时已经校验通过的主机密钥指纹——草稿恢复 / 重连后保存之前先比对，不一致或根本
//! 没记录（旧草稿）就不自动写，交给用户确认。
//!
//! 这一页只有纯逻辑和文案；界面（提示条、恢复列表）按 [`check`] 的结果决定怎么问。

use serde::{Deserialize, Serialize};
use zssh_core::sftp::SftpChannel;
use zssh_core::ssh::connection::Connection;

/// 一条连接实际连到的服务器。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServerIdentity {
    /// `用户名@主机:端口`（主机不分大小写、去掉结尾的点，和核心的远端写入锁是同一个写法）。
    /// 空 = 这条连接上没有记下身份，当作无法确认。
    pub(super) target: String,
    /// 连接时已校验通过的服务器主机密钥指纹（`SHA256:…`）。从现成的 SFTP 通道里取不到它
    /// （通道不带主机密钥），那时是 `None`，比对时只看 [`Self::target`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) host_key: Option<String>,
}

impl ServerIdentity {
    /// 现成的通道：只有地址、端口、用户名。拿不到身份（没有主机或用户名）时 `target` 是空的。
    pub(super) fn from_channel(channel: &SftpChannel) -> Self {
        let key = channel.server_key();
        // 连接上没记身份时核心退一步用 `connection#<地址>` 当锁键：它每次连接都不同，不是身份。
        let target = if key.starts_with("connection#") {
            String::new()
        } else {
            key.to_owned()
        };
        Self {
            target,
            host_key: None,
        }
    }

    /// 自己拨出来的连接：地址、端口、用户名加上这次握手校验过的主机密钥指纹。
    pub(crate) fn from_connection(connection: &Connection) -> Self {
        Self {
            target: connection.identity().unwrap_or_default().to_owned(),
            host_key: Some(connection.server_key().fingerprint.clone()),
        }
    }

    fn is_known(&self) -> bool {
        !self.target.is_empty()
    }
}

/// 比对结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdentityCheck {
    /// 同一台服务器，放行。
    Same,
    /// 无法确认：原来没记录（旧草稿）或这次拿不到身份。不自动写。
    Unconfirmed,
    /// 不是原来那台。不自动写。
    Differs {
        /// 地址 / 端口 / 用户名不同。
        target: bool,
        /// 两边都有主机密钥指纹且不同。
        host_key: bool,
    },
}

/// 原来的身份（打开文件时记下的）和现在要写的目标比对。
///
/// 主机密钥指纹只在两边都有时才比：只有一边有就退回只比地址、端口、用户名（现成通道取不到指纹，
/// 不能因此把正常的重连一概拦下）。
pub(crate) fn check(original: Option<&ServerIdentity>, current: &ServerIdentity) -> IdentityCheck {
    let Some(original) = original else {
        return IdentityCheck::Unconfirmed;
    };
    if !original.is_known() || !current.is_known() {
        return IdentityCheck::Unconfirmed;
    }
    let target = original.target != current.target;
    let host_key = matches!(
        (&original.host_key, &current.host_key),
        (Some(a), Some(b)) if a != b
    );
    if target || host_key {
        IdentityCheck::Differs { target, host_key }
    } else {
        IdentityCheck::Same
    }
}

/// 给界面看的差异说明：原来的服务器、现在的服务器，以及指纹是否也不同。
pub(crate) fn describe(original: Option<&ServerIdentity>, current: &ServerIdentity) -> String {
    let show = |identity: Option<&ServerIdentity>, missing: IdentityMsg| match identity {
        Some(identity) if identity.is_known() => identity.target.clone(),
        _ => crate::i18n::t(missing).to_owned(),
    };
    let mut text = format!(
        "{}{}{}{}",
        crate::i18n::t(IdentityMsg::Original),
        show(original, IdentityMsg::NotRecorded),
        crate::i18n::t(IdentityMsg::Current),
        show(Some(current), IdentityMsg::Unavailable),
    );
    if matches!(
        check(original, current),
        IdentityCheck::Differs { host_key: true, .. }
    ) {
        text.push_str(crate::i18n::t(IdentityMsg::HostKeyDiffers));
    }
    text
}

/// 恢复列表里「重新上传」被拦下时的整句说明（走冲突提示：下面的「覆盖服务器上的文件」就是确认）。
pub(crate) fn reupload_blocked_message(
    original: Option<&ServerIdentity>,
    current: &ServerIdentity,
) -> String {
    format!(
        "{}{}{}",
        crate::i18n::t(IdentityMsg::BlockedPrefix),
        describe(original, current),
        crate::i18n::t(IdentityMsg::BlockedSuffix)
    )
}

#[derive(Clone, Copy, Debug)]
enum IdentityMsg {
    Original,
    Current,
    NotRecorded,
    Unavailable,
    HostKeyDiffers,
    BlockedPrefix,
    BlockedSuffix,
}

impl crate::i18n::Msg for IdentityMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Original => "原来的服务器：",
            Self::Current => "；现在的服务器：",
            Self::NotRecorded => "没有记录（旧版本保存的）",
            Self::Unavailable => "无法确认",
            Self::HostKeyDiffers => "（主机密钥指纹也不一样）",
            Self::BlockedPrefix => "这份修改对应的服务器和现在的不一致，没有上传。",
            Self::BlockedSuffix => {
                "。确认要写到现在这台服务器的同一路径，请选「覆盖服务器上的文件」（不比对那里文件的内容）；不想写就保留这份草稿。"
            }
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Original => "Original server: ",
            Self::Current => "; current server: ",
            Self::NotRecorded => "not recorded (saved by an older version)",
            Self::Unavailable => "unknown",
            Self::HostKeyDiffers => " (the host key fingerprint differs too)",
            Self::BlockedPrefix => {
                "The server for this change does not match the current one; nothing was uploaded. "
            }
            Self::BlockedSuffix => {
                ". To write to the current server at the same path, choose \"Overwrite\" (the file there is not compared); otherwise keep the draft."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(target: &str, host_key: Option<&str>) -> ServerIdentity {
        ServerIdentity {
            target: target.to_owned(),
            host_key: host_key.map(str::to_owned),
        }
    }

    #[test]
    fn the_same_server_passes() {
        let a = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        assert_eq!(check(Some(&a), &a.clone()), IdentityCheck::Same);
    }

    #[test]
    fn a_changed_address_port_or_user_is_stopped() {
        let original = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        for other in [
            "deploy@web-2.example:22",
            "deploy@web-1.example:2222",
            "root@web-1.example:22",
        ] {
            assert_eq!(
                check(Some(&original), &server(other, Some("SHA256:aaa"))),
                IdentityCheck::Differs {
                    target: true,
                    host_key: false
                },
                "{other}"
            );
        }
    }

    #[test]
    fn a_changed_host_key_is_stopped_even_at_the_same_address() {
        let original = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        assert_eq!(
            check(
                Some(&original),
                &server("deploy@web-1.example:22", Some("SHA256:bbb"))
            ),
            IdentityCheck::Differs {
                target: false,
                host_key: true
            }
        );
    }

    #[test]
    fn a_missing_original_is_not_confirmed() {
        let current = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        assert_eq!(check(None, &current), IdentityCheck::Unconfirmed);
        // 没有主机的空身份也一样。
        assert_eq!(
            check(Some(&server("", None)), &current),
            IdentityCheck::Unconfirmed
        );
        assert_eq!(
            check(Some(&current), &server("", None)),
            IdentityCheck::Unconfirmed
        );
    }

    #[test]
    fn the_host_key_is_only_compared_when_both_sides_have_one() {
        let with_key = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        let without = server("deploy@web-1.example:22", None);
        assert_eq!(check(Some(&with_key), &without), IdentityCheck::Same);
        assert_eq!(check(Some(&without), &with_key), IdentityCheck::Same);
        // 地址不同照样拦。
        assert!(matches!(
            check(Some(&without), &server("deploy@other.example:22", None)),
            IdentityCheck::Differs { target: true, .. }
        ));
    }

    #[test]
    fn the_description_names_both_sides() {
        let original = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        let current = server("deploy@web-2.example:22", Some("SHA256:bbb"));
        let text = describe(Some(&original), &current);
        assert!(text.contains("deploy@web-1.example:22"), "{text}");
        assert!(text.contains("deploy@web-2.example:22"), "{text}");
        let legacy = describe(None, &current);
        assert!(legacy.contains("deploy@web-2.example:22"), "{legacy}");
        assert!(!legacy.contains("web-1"), "{legacy}");
    }

    #[test]
    fn identity_roundtrips_and_an_absent_host_key_is_not_written() {
        let with_key = server("deploy@web-1.example:22", Some("SHA256:aaa"));
        let json = serde_json::to_string(&with_key).expect("序列化");
        assert_eq!(
            serde_json::from_str::<ServerIdentity>(&json).expect("反序列化"),
            with_key
        );
        let without = server("deploy@web-1.example:22", None);
        let json = serde_json::to_string(&without).expect("序列化");
        assert!(!json.contains("host_key"), "{json}");
        assert_eq!(
            serde_json::from_str::<ServerIdentity>(&json).expect("反序列化"),
            without
        );
    }
}
