//! 「从 SSH 配置导入」对话框的文案（V2-U59）。核心库给的提示是带参数的枚举，这里按界面语言生成句子；
//! 主机别名、地址、文件名、关键字、系统给的错误说明一律原样显示，不翻译。

use zeeterm_core::ssh_config::{
    ChainIssue, ConfigNotice, HostNote, PlanNote, SkipReason, SkippedEntry,
};
use zeeterm_core::store::hosts::MAX_CHAIN_HOPS;

use crate::i18n::{Msg, colon, count, is_en, list_sep, quote, t};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SshImportMsg {
    Title,
    Source,
    SelectAll,
    SelectNone,
    GroupLabel,
    NoGroup,
    Import,
    Cancel,
    ChooseFile,
    Exists,
    AuthPassword,
    AuthKey,
    Via,
    NotFoundDefault,
    NotFoundPicked,
    NotFoundHint,
    UnreadablePrefix,
    TooLargePrefix,
    EmptyTitle,
    EmptyHint,
    SkippedTitle,
    NoticesTitle,
    ImportFailedPrefix,
    ImportedPrefix,
    HostSuffix,
    UnsupportedLabel,
    DefaultUserPre,
    DefaultUserPost,
    PercentInHostName,
    PercentInIdentityFile,
    ExtraKeysPre,
    ExtraKeysPost,
    JumpPre,
    JumpNotFoundPost,
    JumpNotSelectedPost,
    HopMissingPre,
    HopMissingPost,
    ConflictPost,
    WiredPre,
    WiredPost,
    JumpDisabled,
    JumpLoop,
    JumpTooDeepPre,
    JumpTooDeepPost,
    PortWord,
    BadPortPost,
    HostNameWord,
    BadAddressPost,
    MatchPre,
    MatchMid,
    MatchPost,
    IncludeLoopPre,
    IncludeTooDeepPre,
    IncludeUnreadablePre,
    FileTooLargePre,
    SummaryPre,
    SummaryMid,
    SummaryPost,
}

impl Msg for SshImportMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Title => "从 SSH 配置导入",
            Self::Source => "来源",
            Self::SelectAll => "全选",
            Self::SelectNone => "全不选",
            Self::GroupLabel => "放入分组",
            Self::NoGroup => "不分组",
            Self::Import => "导入",
            Self::Cancel => "取消",
            Self::ChooseFile => "选择文件…",
            Self::Exists => "已存在",
            Self::AuthPassword => "密码（连接时询问）",
            Self::AuthKey => "密钥 ",
            Self::Via => "经由 ",
            Self::NotFoundDefault => "没有找到 ~/.ssh/config",
            Self::NotFoundPicked => "没有找到这个文件",
            Self::NotFoundHint => {
                "ZeeTerm 只会读取 OpenSSH 的配置文件，不会修改它。配置在别处的话，可以手动选一个文件。"
            }
            Self::UnreadablePrefix => "读不了这个文件：",
            Self::TooLargePrefix => "这个文件太大，不像是 SSH 配置：",
            Self::EmptyTitle => "这份配置里没有可以导入的主机",
            Self::EmptyHint => "只有 Host * 这类通配符的段不会生成主机。",
            Self::SkippedTitle => "没有导入的条目",
            Self::NoticesTitle => "配置里的其他提示",
            Self::ImportFailedPrefix => "导入失败：",
            Self::ImportedPrefix => "已导入 ",
            Self::HostSuffix => " 台主机",
            Self::UnsupportedLabel => "这些设置没有导入",
            Self::DefaultUserPre => "没写 User，用户名取了 ",
            Self::DefaultUserPost => "",
            Self::PercentInHostName => "HostName 里有 % 转义，原样保留了",
            Self::PercentInIdentityFile => {
                "IdentityFile 里有 % 变量，没有展开，导入后请检查密钥路径"
            }
            Self::ExtraKeysPre => "还写了 ",
            Self::ExtraKeysPost => " 个 IdentityFile，只用了第一个",
            Self::JumpPre => "跳板机",
            Self::JumpNotFoundPost => "在配置里找不到，也没有同名的主机，没设跳板机",
            Self::JumpNotSelectedPost => "没有一起导入，也没有同名的主机，没设跳板机",
            Self::HopMissingPre => "多级跳板里的",
            Self::HopMissingPost => "找不到或没有导入，只接了最后一跳",
            Self::ConflictPost => "自己已经设了别的跳板机，多级跳板只接了最后一跳",
            Self::WiredPre => "为了串起多级跳板，这台设成了经由",
            Self::WiredPost => "连接",
            Self::JumpDisabled => "ProxyJump 写的是 none，没有跳板机",
            Self::JumpLoop => "跳板机绕成了圈，没设跳板机",
            Self::JumpTooDeepPre => "跳板链太深（最多 ",
            Self::JumpTooDeepPost => " 跳，含目标），没设跳板机",
            Self::PortWord => "端口",
            Self::BadPortPost => "不是 1–65535 的整数",
            Self::HostNameWord => "HostName",
            Self::BadAddressPost => "不是合法的地址",
            Self::MatchPre => "",
            Self::MatchMid => " 第 ",
            Self::MatchPost => " 行的 Match 段被整段跳过了（不支持 Match 条件）",
            Self::IncludeLoopPre => "Include 绕回了自己，没有再展开：",
            Self::IncludeTooDeepPre => "Include 嵌套太深（最多 16 层），没有再展开：",
            Self::IncludeUnreadablePre => "读不了被 Include 的文件 ",
            Self::FileTooLargePre => "文件太大，没有读：",
            Self::SummaryPre => "共 ",
            Self::SummaryMid => " 台，已选 ",
            Self::SummaryPost => " 台",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Title => "Import from SSH Config",
            Self::Source => "Source",
            Self::SelectAll => "Select All",
            Self::SelectNone => "Select None",
            Self::GroupLabel => "Add to group",
            Self::NoGroup => "No group",
            Self::Import => "Import",
            Self::Cancel => "Cancel",
            Self::ChooseFile => "Choose File…",
            Self::Exists => "Already exists",
            Self::AuthPassword => "Password (asked when connecting)",
            Self::AuthKey => "Key ",
            Self::Via => "via ",
            Self::NotFoundDefault => "~/.ssh/config was not found",
            Self::NotFoundPicked => "That file was not found",
            Self::NotFoundHint => {
                "ZeeTerm only reads the OpenSSH config file and never changes it. If yours lives elsewhere, choose it by hand."
            }
            Self::UnreadablePrefix => "Could not read this file: ",
            Self::TooLargePrefix => "This file is too large to be an SSH config: ",
            Self::EmptyTitle => "This config has no hosts to import",
            Self::EmptyHint => {
                "Sections that only use wildcards, such as Host *, do not create hosts."
            }
            Self::SkippedTitle => "Skipped entries",
            Self::NoticesTitle => "Other notes about the config",
            Self::ImportFailedPrefix => "Import failed: ",
            Self::ImportedPrefix => "Imported ",
            Self::HostSuffix => " hosts",
            Self::UnsupportedLabel => "Settings not imported",
            Self::DefaultUserPre => "No User written; using ",
            Self::DefaultUserPost => " as the user name",
            Self::PercentInHostName => "HostName has % escapes; kept as written",
            Self::PercentInIdentityFile => {
                "IdentityFile has % tokens, which were not expanded; check the key path after importing"
            }
            Self::ExtraKeysPre => "",
            Self::ExtraKeysPost => " more IdentityFile lines ignored; only the first is used",
            Self::JumpPre => "Jump host ",
            Self::JumpNotFoundPost => {
                " is not in the config or among existing hosts; no jump host set"
            }
            Self::JumpNotSelectedPost => {
                " is not being imported and no existing host has that name; no jump host set"
            }
            Self::HopMissingPre => "Jump hop ",
            Self::HopMissingPost => " was not found or not imported; only the last hop is used",
            Self::ConflictPost => " already has another jump host; only the last hop is used",
            Self::WiredPre => "To chain a multi-hop ProxyJump, this host now connects via ",
            Self::WiredPost => "",
            Self::JumpDisabled => "ProxyJump is none; no jump host",
            Self::JumpLoop => "The jump hosts form a loop; no jump host set",
            Self::JumpTooDeepPre => "The jump chain is too deep (at most ",
            Self::JumpTooDeepPost => " hops including the target); no jump host set",
            Self::PortWord => "Port",
            Self::BadPortPost => " is not an integer from 1 to 65535",
            Self::HostNameWord => "HostName",
            Self::BadAddressPost => " is not a valid address",
            Self::MatchPre => "The Match block in ",
            Self::MatchMid => " (line ",
            Self::MatchPost => ") was skipped; Match conditions are not supported",
            Self::IncludeLoopPre => "Include loops back to itself and was not expanded: ",
            Self::IncludeTooDeepPre => {
                "Include nesting is too deep (at most 16 levels); not expanded: "
            }
            Self::IncludeUnreadablePre => "Could not read an included file: ",
            Self::FileTooLargePre => "File too large; not read: ",
            Self::SummaryPre => "",
            Self::SummaryMid => " hosts, ",
            Self::SummaryPost => " selected",
        }
    }
}

/// 「共 N 台，已选 M 台」。
pub(super) fn summary(total: usize, selected: usize) -> String {
    if is_en() {
        let noun = if total == 1 { " host, " } else { " hosts, " };
        format!("{total}{noun}{selected}{}", t(SshImportMsg::SummaryPost))
    } else {
        format!(
            "{}{total}{}{selected}{}",
            t(SshImportMsg::SummaryPre),
            t(SshImportMsg::SummaryMid),
            t(SshImportMsg::SummaryPost)
        )
    }
}

/// 导入成功后的提示。
pub(super) fn imported_toast(n: usize) -> String {
    format!(
        "{}{}",
        t(SshImportMsg::ImportedPrefix),
        count(n as u64, t(SshImportMsg::HostSuffix), "host", "hosts")
    )
}

/// 「这些设置没有导入：ProxyCommand、ForwardAgent」，拆成一段一段：第一段是说明（含冒号），后面每个关键字一段，
/// 分隔符（顿号）跟在它前面那个关键字的后面。分成块是为了窄的时候换行不会把顿号甩到下一行开头。
/// 英文的逗号后面的空格由排版的间距补，所以这里去掉。
pub(super) fn unsupported_parts(keys: &[String]) -> Vec<String> {
    let separator = list_sep().trim_end();
    let mut parts = vec![
        colon(t(SshImportMsg::UnsupportedLabel), "")
            .trim_end()
            .to_owned(),
    ];
    for (index, key) in keys.iter().enumerate() {
        let last = index + 1 == keys.len();
        parts.push(if last {
            key.clone()
        } else {
            format!("{key}{separator}")
        });
    }
    parts
}

/// 配置里没写 `User` 时的说明。
pub(super) fn default_user_note(name: &str) -> String {
    format!(
        "{}{name}{}",
        t(SshImportMsg::DefaultUserPre),
        t(SshImportMsg::DefaultUserPost)
    )
}

pub(super) fn host_note_text(note: &HostNote) -> String {
    match note {
        HostNote::PercentInHostName => t(SshImportMsg::PercentInHostName).to_owned(),
        HostNote::PercentInIdentityFile => t(SshImportMsg::PercentInIdentityFile).to_owned(),
        HostNote::ExtraIdentityFiles(n) => format!(
            "{}{n}{}",
            t(SshImportMsg::ExtraKeysPre),
            t(SshImportMsg::ExtraKeysPost)
        ),
    }
}

/// 名字夹在前后两段中间：`前 + 「名字」 + 后`。
fn named(pre: SshImportMsg, name: &str, post: SshImportMsg) -> String {
    format!("{}{}{}", t(pre), quote(name), t(post))
}

pub(super) fn plan_note_text(note: &PlanNote) -> String {
    match note {
        PlanNote::JumpDisabled => t(SshImportMsg::JumpDisabled).to_owned(),
        PlanNote::JumpNotFound(name) => {
            named(SshImportMsg::JumpPre, name, SshImportMsg::JumpNotFoundPost)
        }
        PlanNote::JumpNotSelected(name) => named(
            SshImportMsg::JumpPre,
            name,
            SshImportMsg::JumpNotSelectedPost,
        ),
        PlanNote::ChainTruncated(ChainIssue::HopMissing(name)) => named(
            SshImportMsg::HopMissingPre,
            name,
            SshImportMsg::HopMissingPost,
        ),
        PlanNote::ChainTruncated(ChainIssue::Conflict(name)) => {
            named(SshImportMsg::JumpPre, name, SshImportMsg::ConflictPost)
        }
        PlanNote::WiredAsJump { via } => {
            named(SshImportMsg::WiredPre, via, SshImportMsg::WiredPost)
        }
        PlanNote::JumpLoop => t(SshImportMsg::JumpLoop).to_owned(),
        PlanNote::JumpTooDeep => format!(
            "{}{MAX_CHAIN_HOPS}{}",
            t(SshImportMsg::JumpTooDeepPre),
            t(SshImportMsg::JumpTooDeepPost)
        ),
    }
}

/// 被跳过的条目：`别名：端口「abc」不是 1–65535 的整数`。
pub(super) fn skipped_text(entry: &SkippedEntry) -> String {
    let reason = match &entry.reason {
        SkipReason::BadPort(raw) => format!(
            "{} {}{}",
            t(SshImportMsg::PortWord),
            quote(raw),
            t(SshImportMsg::BadPortPost)
        ),
        SkipReason::BadAddress(raw) => format!(
            "{} {}{}",
            t(SshImportMsg::HostNameWord),
            quote(raw),
            t(SshImportMsg::BadAddressPost)
        ),
    };
    colon(&entry.alias, &reason)
}

pub(super) fn notice_text(notice: &ConfigNotice) -> String {
    match notice {
        ConfigNotice::MatchSkipped { file, line } => format!(
            "{}{file}{}{line}{}",
            t(SshImportMsg::MatchPre),
            t(SshImportMsg::MatchMid),
            t(SshImportMsg::MatchPost)
        ),
        ConfigNotice::IncludeLoop { file } => {
            format!("{}{file}", t(SshImportMsg::IncludeLoopPre))
        }
        ConfigNotice::IncludeTooDeep { file } => {
            format!("{}{file}", t(SshImportMsg::IncludeTooDeepPre))
        }
        ConfigNotice::IncludeUnreadable { file, error } => format!(
            "{}{}",
            t(SshImportMsg::IncludeUnreadablePre),
            colon(file, error)
        ),
        ConfigNotice::FileTooLarge { file } => {
            format!("{}{file}", t(SshImportMsg::FileTooLargePre))
        }
    }
}

#[cfg(test)]
mod tests {
    use zeeterm_core::ssh_config::{ChainIssue, ConfigNotice, HostNote, PlanNote, SkipReason};

    use super::*;

    /// 每条提示都有中英两个说法，而且英文里没有中文（i18n 检查之外再兜一道：拼句子的片段也要走表）。
    #[test]
    fn every_message_has_both_languages_and_the_english_has_no_han() {
        use SshImportMsg::*;
        for key in [
            Title,
            Source,
            SelectAll,
            SelectNone,
            GroupLabel,
            NoGroup,
            Import,
            Cancel,
            ChooseFile,
            Exists,
            AuthPassword,
            AuthKey,
            Via,
            NotFoundDefault,
            NotFoundPicked,
            NotFoundHint,
            UnreadablePrefix,
            TooLargePrefix,
            EmptyTitle,
            EmptyHint,
            SkippedTitle,
            NoticesTitle,
            ImportFailedPrefix,
            ImportedPrefix,
            HostSuffix,
            UnsupportedLabel,
            DefaultUserPre,
            PercentInHostName,
            PercentInIdentityFile,
            ExtraKeysPost,
            JumpPre,
            JumpNotFoundPost,
            JumpNotSelectedPost,
            HopMissingPre,
            HopMissingPost,
            ConflictPost,
            WiredPre,
            JumpDisabled,
            JumpLoop,
            JumpTooDeepPre,
            JumpTooDeepPost,
            PortWord,
            BadPortPost,
            HostNameWord,
            BadAddressPost,
            MatchMid,
            MatchPost,
            IncludeLoopPre,
            IncludeTooDeepPre,
            IncludeUnreadablePre,
            FileTooLargePre,
            SummaryMid,
            SummaryPost,
        ] {
            assert!(!key.zh().is_empty(), "{key:?} 缺中文");
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
            assert!(
                key.en()
                    .chars()
                    .all(|c| !('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{key:?} 的英文里混进了中文"
            );
        }
    }

    #[test]
    fn the_not_found_text_names_the_default_path() {
        assert_eq!(SshImportMsg::NotFoundDefault.zh(), "没有找到 ~/.ssh/config");
    }

    #[test]
    fn sentences_are_built_from_the_notes() {
        assert_eq!(summary(8, 6), "共 8 台，已选 6 台");
        assert_eq!(imported_toast(3), "已导入 3 台主机");
        assert_eq!(
            unsupported_parts(&["ProxyCommand".into(), "ForwardAgent".into()]),
            ["这些设置没有导入：", "ProxyCommand、", "ForwardAgent"]
        );
        assert_eq!(
            host_note_text(&HostNote::ExtraIdentityFiles(2)),
            "还写了 2 个 IdentityFile，只用了第一个"
        );
        assert_eq!(
            plan_note_text(&PlanNote::JumpNotFound("gw".into())),
            "跳板机「gw」在配置里找不到，也没有同名的主机，没设跳板机"
        );
        assert_eq!(
            plan_note_text(&PlanNote::ChainTruncated(ChainIssue::Conflict("b".into()))),
            "跳板机「b」自己已经设了别的跳板机，多级跳板只接了最后一跳"
        );
        assert_eq!(
            plan_note_text(&PlanNote::WiredAsJump { via: "a".into() }),
            "为了串起多级跳板，这台设成了经由「a」连接"
        );
        assert!(plan_note_text(&PlanNote::JumpTooDeep).contains(&MAX_CHAIN_HOPS.to_string()));
        assert_eq!(
            skipped_text(&zeeterm_core::ssh_config::SkippedEntry {
                alias: "db".into(),
                reason: SkipReason::BadPort("abc".into()),
            }),
            "db：端口 「abc」不是 1–65535 的整数"
        );
        assert_eq!(
            notice_text(&ConfigNotice::MatchSkipped {
                file: "config".into(),
                line: 4
            }),
            "config 第 4 行的 Match 段被整段跳过了（不支持 Match 条件）"
        );
        assert_eq!(default_user_note("ops"), "没写 User，用户名取了 ops");
    }
}
