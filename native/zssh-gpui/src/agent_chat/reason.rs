//! 确认卡上"为什么要确认"的原因文案。
//!
//! 核心库给出结构化原因（类型 + 参数，见 [`ApprovalReason`]），这里按当前界面语言生成整句；
//! 路径、目录名这类参数原样拼进去。命令危险规则（[`ApprovalReason::Command`]）按规则 key 取
//! `reason_text` 里的英文整句；没有 key 的（[`ApprovalReason::Text`]）原样显示。

use zssh_core::ai::reason::{ApprovalReason, SystemDirKind};

use crate::i18n::{Lang, Msg};

/// 原因文案里的固定片段。
#[derive(Clone, Copy, Debug)]
enum ReasonMsg {
    SensitiveRead,
    SensitiveWildcard,
    SystemPath,
    DirConfig,
    DirBoot,
    DirPrograms,
    DirDevices,
    DirKernel,
    DirCron,
    DirPackageDb,
    DirLibraries,
    AuthorizedKeys,
    Sudoers,
    Crontab,
    ParentEscape,
    ParentUnresolved,
    Locked,
}

impl Msg for ReasonMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::SensitiveRead => "读取敏感文件：",
            Self::SensitiveWildcard => "（含通配符或变量，无法确定具体文件，按敏感处理）",
            Self::SystemPath => "写入系统路径：",
            Self::DirConfig => "系统配置目录",
            Self::DirBoot => "引导目录",
            Self::DirPrograms => "系统程序目录",
            Self::DirDevices => "设备目录",
            Self::DirKernel => "内核接口目录",
            Self::DirCron => "定时任务目录",
            Self::DirPackageDb => "软件包数据库",
            Self::DirLibraries => "系统库目录",
            Self::AuthorizedKeys => {
                "写入 SSH 登录授权文件 authorized_keys（会改变谁能登录这台服务器）"
            }
            Self::Sudoers => "写入 sudoers 提权配置",
            Self::Crontab => "写入 crontab 定时任务",
            Self::ParentEscape => "路径里有 ..，又无法确定起点，可能落在系统目录",
            Self::ParentUnresolved => "无法解析上级目录的真实路径（可能有符号链接）",
            Self::Locked => "应用已锁屏：AI 要在服务器上做的事需要解锁后由你确认",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::SensitiveRead => "Reads a sensitive file: ",
            Self::SensitiveWildcard => {
                " (contains a wildcard or variable, so the exact file cannot be determined; treated as sensitive)"
            }
            Self::SystemPath => "Writes to a system path: ",
            Self::DirConfig => "system configuration directory",
            Self::DirBoot => "boot directory",
            Self::DirPrograms => "system program directory",
            Self::DirDevices => "device directory",
            Self::DirKernel => "kernel interface directory",
            Self::DirCron => "scheduled-task directory",
            Self::DirPackageDb => "package database",
            Self::DirLibraries => "system library directory",
            Self::AuthorizedKeys => {
                "Writes the SSH login authorization file authorized_keys (changes who can log in to this server)"
            }
            Self::Sudoers => "Writes the sudoers privilege configuration",
            Self::Crontab => "Writes a crontab scheduled task",
            Self::ParentEscape => {
                "The path contains \"..\" and its starting point cannot be determined; it may land in a system directory"
            }
            Self::ParentUnresolved => {
                "Could not resolve the real path of the parent directory (it may involve a symbolic link)"
            }
            Self::Locked => {
                "The app is locked: what the AI wants to do on the server needs your confirmation after unlocking"
            }
        }
    }
}

fn pick(lang: Lang, key: ReasonMsg) -> &'static str {
    match lang {
        Lang::Zh => key.zh(),
        Lang::En => key.en(),
    }
}

fn dir_kind(kind: SystemDirKind) -> ReasonMsg {
    match kind {
        SystemDirKind::Config => ReasonMsg::DirConfig,
        SystemDirKind::Boot => ReasonMsg::DirBoot,
        SystemDirKind::Programs => ReasonMsg::DirPrograms,
        SystemDirKind::Devices => ReasonMsg::DirDevices,
        SystemDirKind::KernelInterface => ReasonMsg::DirKernel,
        SystemDirKind::CronSpool => ReasonMsg::DirCron,
        SystemDirKind::PackageDb => ReasonMsg::DirPackageDb,
        SystemDirKind::Libraries => ReasonMsg::DirLibraries,
    }
}

/// 一条确认原因 → 当前界面语言的一句话。
pub(super) fn reason_text(reason: &ApprovalReason) -> String {
    reason_text_in(crate::i18n::current(), reason)
}

/// 指定语言生成（测试用；界面走 [`reason_text`] 读当前语言）。
pub(super) fn reason_text_in(lang: Lang, reason: &ApprovalReason) -> String {
    match reason {
        ApprovalReason::SensitiveRead { path, exact } => {
            let mut text = format!("{}{path}", pick(lang, ReasonMsg::SensitiveRead));
            if !exact {
                text.push_str(pick(lang, ReasonMsg::SensitiveWildcard));
            }
            text
        }
        ApprovalReason::SystemPath { kind, dir } => format!(
            "{}{} {dir}",
            pick(lang, ReasonMsg::SystemPath),
            pick(lang, dir_kind(*kind))
        ),
        ApprovalReason::AuthorizedKeys => pick(lang, ReasonMsg::AuthorizedKeys).to_owned(),
        ApprovalReason::Sudoers => pick(lang, ReasonMsg::Sudoers).to_owned(),
        ApprovalReason::Crontab => pick(lang, ReasonMsg::Crontab).to_owned(),
        ApprovalReason::ParentEscape => pick(lang, ReasonMsg::ParentEscape).to_owned(),
        ApprovalReason::ParentUnresolved => pick(lang, ReasonMsg::ParentUnresolved).to_owned(),
        ApprovalReason::Locked => pick(lang, ReasonMsg::Locked).to_owned(),
        ApprovalReason::Command { rule, text } => match lang {
            Lang::En => {
                crate::reason_text::english(rule).map_or_else(|| text.clone(), str::to_owned)
            }
            Lang::Zh => text.clone(),
        },
        ApprovalReason::Text { text } => text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_reason() -> Vec<ApprovalReason> {
        let mut all = vec![
            ApprovalReason::SensitiveRead {
                path: "~/.ssh/id_ed25519".to_owned(),
                exact: true,
            },
            ApprovalReason::SensitiveRead {
                path: "~/.ssh/*".to_owned(),
                exact: false,
            },
            ApprovalReason::AuthorizedKeys,
            ApprovalReason::Sudoers,
            ApprovalReason::Crontab,
            ApprovalReason::ParentEscape,
            ApprovalReason::ParentUnresolved,
            ApprovalReason::Locked,
        ];
        for (kind, dir) in [
            (SystemDirKind::Config, "/etc"),
            (SystemDirKind::Boot, "/boot"),
            (SystemDirKind::Programs, "/usr"),
            (SystemDirKind::Devices, "/dev"),
            (SystemDirKind::KernelInterface, "/proc"),
            (SystemDirKind::CronSpool, "/var/spool/cron"),
            (SystemDirKind::PackageDb, "/var/lib/dpkg"),
            (SystemDirKind::Libraries, "/lib64"),
        ] {
            all.push(ApprovalReason::SystemPath {
                kind,
                dir: dir.to_owned(),
            });
        }
        all
    }

    #[test]
    fn chinese_matches_the_core_text_exactly() {
        for reason in every_reason() {
            assert_eq!(
                reason_text_in(Lang::Zh, &reason),
                reason.text(),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn english_has_no_chinese_and_keeps_the_path_parameters() {
        for reason in every_reason() {
            let text = reason_text_in(Lang::En, &reason);
            assert!(
                !text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{reason:?} → {text}"
            );
        }
        assert_eq!(
            reason_text_in(
                Lang::En,
                &ApprovalReason::SensitiveRead {
                    path: "~/.ssh/id_ed25519".to_owned(),
                    exact: true
                }
            ),
            "Reads a sensitive file: ~/.ssh/id_ed25519"
        );
        let wildcard = reason_text_in(
            Lang::En,
            &ApprovalReason::SensitiveRead {
                path: "~/.ssh/*".to_owned(),
                exact: false,
            },
        );
        assert!(
            wildcard.contains("~/.ssh/*") && wildcard.contains("wildcard"),
            "{wildcard}"
        );
        assert_eq!(
            reason_text_in(
                Lang::En,
                &ApprovalReason::SystemPath {
                    kind: SystemDirKind::Config,
                    dir: "/etc".to_owned()
                }
            ),
            "Writes to a system path: system configuration directory /etc"
        );
    }

    /// 真的让命令危险规则跑一遍：每条命中的规则都有英文整句，中文和核心给的原文逐字相同。
    #[test]
    fn command_rules_read_as_whole_english_sentences() {
        let commands = [
            "rm -rf /",
            ":(){ :|:& };:",
            "mkfs.ext4 /dev/sda1",
            "shutdown -h now",
            "iptables -F",
            "python3 -c 'print(1)'",
            "dd if=/dev/zero of=/dev/sda",
            "echo x > /etc/passwd",
            "su - root",
            "systemctl stop sshd",
        ];
        let mut seen = 0;
        for command in commands {
            let verdict = zssh_core::ai::safety::assess(command);
            assert_eq!(verdict.reasons.len(), verdict.rules.len(), "{command}");
            for (text, rule) in verdict.reasons.iter().zip(&verdict.rules) {
                let reason = ApprovalReason::command(rule, text.clone());
                assert_eq!(reason_text_in(Lang::Zh, &reason), *text);
                let en = reason_text_in(Lang::En, &reason);
                assert_ne!(en, *text, "{rule} 没有英文");
                assert!(
                    !en.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                    "{rule}: {en}"
                );
                seen += 1;
            }
        }
        assert!(seen >= 8, "命令例子要真的命中规则：{seen}");
        // 认不得的规则 key 退回中文原文。
        let unknown = ApprovalReason::command("cmd.from_the_future", "新规则");
        assert_eq!(reason_text_in(Lang::En, &unknown), "新规则");
    }

    #[test]
    fn untyped_reasons_pass_through_unchanged() {
        let text = ApprovalReason::text_only("会格式化文件系统");
        assert_eq!(reason_text_in(Lang::En, &text), "会格式化文件系统");
        assert_eq!(reason_text_in(Lang::Zh, &text), "会格式化文件系统");
    }
}
