//! 真正退出（关闭按钮在没有托盘时的退出、托盘菜单的「退出」）前的确认：右侧面板里进行中的
//! 工作（SFTP 传输、AI 回合，以及同步 / 恢复）都登记在 [`crate::install_state`] 里，这里只把
//! 登记表翻译成确认框的文字；和「更新前检查」用的是同一份登记，所以两处说法一致。
//!
//! 关到托盘不算退出（进程和后台任务继续跑），不走这个确认。

use crate::i18n::{Msg, count, t};
use crate::install_state::PendingWork;

/// 退出确认 / 关闭文件视图确认的文案。主机名、产品名是数据，不进这张表。
#[derive(Clone, Copy, Debug)]
enum CloseMsg {
    /// `count` 的中文量词（英文走单复数，用不到它）。
    SessionCountSuffix,
    /// 同上（退出确认里的「N 个文件传输」）。
    TransferCountSuffix,
    /// 关闭文件视图时只数这个面板自己的传输（「N 个传输」）。
    TransferUnitSuffix,
    /// 同上。
    AiTurnCountSuffix,
    Sync,
    Restore,
    QuitTitlePrefix,
    QuitTitleSuffix,
    QuitBodyPrefix,
    QuitBodySuffix,
    CloseTitlePrefix,
    CloseTitleSuffix,
    TransferBodyPrefix,
    TransferBodySuffix,
}

impl Msg for CloseMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::SessionCountSuffix => " 个已连接的会话",
            Self::TransferCountSuffix => " 个文件传输",
            Self::TransferUnitSuffix => " 个传输",
            Self::AiTurnCountSuffix => " 个 AI 回合",
            Self::Sync => "同步",
            Self::Restore => "备份恢复",
            Self::QuitTitlePrefix => "退出 ",
            Self::QuitTitleSuffix => "？",
            Self::QuitBodyPrefix => "还有 ",
            Self::QuitBodySuffix => "在进行，现在退出会中断它们。",
            Self::CloseTitlePrefix => "关闭「",
            Self::CloseTitleSuffix => "」的文件视图？",
            Self::TransferBodyPrefix => "还有 ",
            Self::TransferBodySuffix => {
                "没有结束，关闭会取消它们（已传的部分保留，可以稍后续传）。"
            }
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::SessionCountSuffix => " ",
            Self::TransferCountSuffix => " ",
            Self::TransferUnitSuffix => " ",
            Self::AiTurnCountSuffix => " ",
            Self::Sync => "sync",
            Self::Restore => "backup restore",
            Self::QuitTitlePrefix => "Quit ",
            Self::QuitTitleSuffix => "?",
            Self::QuitBodyPrefix => "These are still running: ",
            Self::QuitBodySuffix => ". Quitting now will interrupt them.",
            Self::CloseTitlePrefix => "Close the file view for “",
            Self::CloseTitleSuffix => "”?",
            Self::TransferBodyPrefix => "",
            Self::TransferBodySuffix => {
                " not finished yet; closing cancels the remaining transfer work (bytes already sent are kept and can be resumed later)."
            }
        }
    }
}

/// 列表的连接符跟着语言走（中文用顿号）。
fn join_list(parts: &[String]) -> String {
    parts.join(crate::i18n::list_sep())
}

/// 有进行中的工作就返回 `(标题, 正文)`，没有返回 `None`（可以直接退出）。
#[cfg(any(test, debug_assertions))]
pub fn quit_prompt(work: PendingWork) -> Option<(String, String)> {
    quit_prompt_with_sessions(work, 0)
}

/// 同 [`quit_prompt`]，再算上还连着（或正在连）的终端会话：退出会把它们全部断开，
/// 和 0.2.14 一样先问一句。
pub fn quit_prompt_with_sessions(work: PendingWork, sessions: usize) -> Option<(String, String)> {
    let mut parts: Vec<String> = Vec::new();
    if sessions > 0 {
        parts.push(count(
            sessions as u64,
            t(CloseMsg::SessionCountSuffix),
            "connected session",
            "connected sessions",
        ));
    }
    if work.transfers > 0 {
        parts.push(count(
            work.transfers as u64,
            t(CloseMsg::TransferCountSuffix),
            "file transfer",
            "file transfers",
        ));
    }
    if work.ai_turns > 0 {
        parts.push(count(
            work.ai_turns as u64,
            t(CloseMsg::AiTurnCountSuffix),
            "AI turn",
            "AI turns",
        ));
    }
    if work.syncs > 0 {
        parts.push(t(CloseMsg::Sync).to_owned());
    }
    if work.restores > 0 {
        parts.push(t(CloseMsg::Restore).to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    Some((
        format!(
            "{}{}{}",
            t(CloseMsg::QuitTitlePrefix),
            crate::product::NAME,
            t(CloseMsg::QuitTitleSuffix)
        ),
        format!(
            "{}{}{}",
            t(CloseMsg::QuitBodyPrefix),
            join_list(&parts),
            t(CloseMsg::QuitBodySuffix)
        ),
    ))
}

/// 关一个 SFTP 标签（或停靠面板的连接）时，这个面板自己还有几个未结束的传输要确认。
pub fn close_sftp_prompt(host: &str, unfinished: usize) -> Option<(String, String)> {
    (unfinished > 0).then(|| {
        (
            format!(
                "{}{host}{}",
                t(CloseMsg::CloseTitlePrefix),
                t(CloseMsg::CloseTitleSuffix)
            ),
            format!(
                "{}{}{}",
                t(CloseMsg::TransferBodyPrefix),
                count(
                    unfinished as u64,
                    t(CloseMsg::TransferUnitSuffix),
                    "transfer",
                    "transfers",
                ),
                t(CloseMsg::TransferBodySuffix)
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_work_quits_without_asking() {
        assert!(quit_prompt(PendingWork::default()).is_none());
    }

    #[test]
    fn every_kind_of_work_is_named() {
        let (title, body) = quit_prompt(PendingWork {
            transfers: 2,
            ai_turns: 1,
            syncs: 1,
            restores: 1,
            unsaved_edits: 0,
        })
        .expect("有工作");
        assert_eq!(title, "退出 ZeeTerm？");
        for needle in ["2 个文件传输", "1 个 AI 回合", "同步", "备份恢复", "中断"] {
            assert!(body.contains(needle), "{needle}：{body}");
        }
    }

    #[test]
    fn connected_sessions_alone_are_enough_to_ask() {
        assert!(quit_prompt_with_sessions(PendingWork::default(), 0).is_none());
        let (_, body) = quit_prompt_with_sessions(PendingWork::default(), 3).expect("有会话");
        assert!(body.contains("3 个已连接的会话"), "{body}");
    }

    #[test]
    fn closing_a_file_view_asks_only_with_unfinished_transfers() {
        assert!(close_sftp_prompt("web-1", 0).is_none());
        let (title, body) = close_sftp_prompt("web-1", 3).expect("有传输");
        assert!(title.contains("web-1"));
        assert!(body.contains("3 个传输"));
    }
}
