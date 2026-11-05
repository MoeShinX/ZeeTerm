//! 真正退出（关闭按钮在没有托盘时的退出、托盘菜单的「退出」）前的确认：右侧面板里进行中的
//! 工作（SFTP 传输、AI 回合，以及同步 / 恢复）都登记在 [`crate::install_state`] 里，这里只把
//! 登记表翻译成确认框的文字；和「更新前检查」用的是同一份登记，所以两处说法一致。
//!
//! 关到托盘不算退出（进程和后台任务继续跑），不走这个确认。

use crate::install_state::PendingWork;

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
        parts.push(format!("{sessions} 个已连接的会话"));
    }
    if work.transfers > 0 {
        parts.push(format!("{} 个文件传输", work.transfers));
    }
    if work.ai_turns > 0 {
        parts.push(format!("{} 个 AI 回合", work.ai_turns));
    }
    if work.syncs > 0 {
        parts.push("同步".to_owned());
    }
    if work.restores > 0 {
        parts.push("备份恢复".to_owned());
    }
    if parts.is_empty() {
        return None;
    }
    Some((
        format!("退出 {}？", crate::product::NAME),
        format!("还有 {}在进行，现在退出会中断它们。", parts.join("、")),
    ))
}

/// 关一个 SFTP 标签（或停靠面板的连接）时，这个面板自己还有几个未结束的传输要确认。
pub fn close_sftp_prompt(host: &str, unfinished: usize) -> Option<(String, String)> {
    (unfinished > 0).then(|| {
        (
            format!("关闭「{host}」的文件视图？"),
            format!(
                "还有 {unfinished} 个传输没有结束，关闭会取消它们（已传的部分保留，可以稍后续传）。"
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
