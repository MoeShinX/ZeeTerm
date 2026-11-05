//! 确认卡片上"为什么要确认"的原因（结构化）。
//!
//! core 不依赖界面框架，也不知道界面语言：它只给出**类型和参数**（哪一类敏感文件、哪个系统目录、
//! 哪个路径），界面按当前语言生成文案。[`ApprovalReason::text`] 给出固定的中文原文，用在
//! 不分语言的地方（给模型的工具结果、日志、测试）。审计库（`store::audit`）只记 `danger`
//! 等级，不记原因文字，所以这里的改动不影响审计内容。

use serde::{Deserialize, Serialize};

/// 锁屏时确认卡上写的原因（中文原文，界面另有按语言的写法）。
pub const LOCKED_REASON: &str = "应用已锁屏：AI 要在服务器上做的事需要解锁后由你确认";

/// 系统目录的类别：对应确认卡上"写入系统路径：<类别> <目录>"里的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemDirKind {
    /// `/etc`
    Config,
    /// `/boot`
    Boot,
    /// `/usr`、`/bin`、`/sbin`
    Programs,
    /// `/dev`
    Devices,
    /// `/proc`、`/sys`
    KernelInterface,
    /// `/var/spool/cron`
    CronSpool,
    /// `/var/lib/dpkg`、`/var/lib/rpm`
    PackageDb,
    /// `/lib`、`/lib32`、`/lib64`、`/libx32`
    Libraries,
}

impl SystemDirKind {
    /// 中文类别名。
    pub fn zh(self) -> &'static str {
        match self {
            Self::Config => "系统配置目录",
            Self::Boot => "引导目录",
            Self::Programs => "系统程序目录",
            Self::Devices => "设备目录",
            Self::KernelInterface => "内核接口目录",
            Self::CronSpool => "定时任务目录",
            Self::PackageDb => "软件包数据库",
            Self::Libraries => "系统库目录",
        }
    }
}

/// 一条确认原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalReason {
    /// 读取敏感文件（私钥、云凭据……）。`exact = false` 表示路径含通配符或变量，按"可能命中"处理。
    SensitiveRead { path: String, exact: bool },
    /// 写入系统目录。`dir` 是命中的目录本身（`/etc`、`/lib64`……）。
    SystemPath { kind: SystemDirKind, dir: String },
    /// 写入 SSH 登录授权文件 `authorized_keys`。
    AuthorizedKeys,
    /// 写入 sudoers 提权配置。
    Sudoers,
    /// 写入 crontab 定时任务。
    Crontab,
    /// 路径里有 `..` 又无法确定起点，可能落在系统目录。
    ParentEscape,
    /// 无法解析上级目录的真实路径（可能有符号链接）。
    ParentUnresolved,
    /// 应用已锁屏。
    Locked,
    /// 还没有结构化的原因（命令危险规则等）：只有中文原文。
    Text { text: String },
}

impl ApprovalReason {
    /// 还没结构化的中文原因。
    pub fn text_only(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// 中文原文：给模型的工具结果、日志和测试用，不随界面语言变。
    pub fn text(&self) -> String {
        match self {
            Self::SensitiveRead { path, exact: true } => format!("读取敏感文件：{path}"),
            Self::SensitiveRead { path, exact: false } => {
                format!("读取敏感文件：{path}（含通配符或变量，无法确定具体文件，按敏感处理）")
            }
            Self::SystemPath { kind, dir } => format!("写入系统路径：{} {dir}", kind.zh()),
            Self::AuthorizedKeys => {
                "写入 SSH 登录授权文件 authorized_keys（会改变谁能登录这台服务器）".to_owned()
            }
            Self::Sudoers => "写入 sudoers 提权配置".to_owned(),
            Self::Crontab => "写入 crontab 定时任务".to_owned(),
            Self::ParentEscape => "路径里有 ..，又无法确定起点，可能落在系统目录".to_owned(),
            Self::ParentUnresolved => "无法解析上级目录的真实路径（可能有符号链接）".to_owned(),
            Self::Locked => LOCKED_REASON.to_owned(),
            Self::Text { text } => text.clone(),
        }
    }
}

impl std::fmt::Display for ApprovalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_text_keeps_the_old_wording() {
        assert_eq!(
            ApprovalReason::SensitiveRead {
                path: "~/.ssh/id_rsa".to_owned(),
                exact: true
            }
            .text(),
            "读取敏感文件：~/.ssh/id_rsa"
        );
        assert!(ApprovalReason::SensitiveRead {
            path: "~/.ssh/*".to_owned(),
            exact: false
        }
        .text()
        .ends_with("（含通配符或变量，无法确定具体文件，按敏感处理）"));
        assert_eq!(
            ApprovalReason::SystemPath {
                kind: SystemDirKind::Config,
                dir: "/etc".to_owned()
            }
            .text(),
            "写入系统路径：系统配置目录 /etc"
        );
        assert_eq!(ApprovalReason::Locked.text(), LOCKED_REASON);
    }

    #[test]
    fn serializes_as_a_tagged_object() {
        let json = serde_json::to_value(ApprovalReason::SystemPath {
            kind: SystemDirKind::Libraries,
            dir: "/lib64".to_owned(),
        })
        .expect("可序列化");
        assert_eq!(json["type"], "system_path");
        assert_eq!(json["kind"], "libraries");
        assert_eq!(json["dir"], "/lib64");
    }
}
