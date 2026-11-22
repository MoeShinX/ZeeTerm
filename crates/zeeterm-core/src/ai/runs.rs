//! 正在终端里跑着的 AI 命令的登记表（AI-2d 1.4）。
//!
//! 停止按钮要区分两种情况：这个对话有 AI 命令正在某个终端里跑（那要发
//! Ctrl+C 把它停下来），还是用户自己正在那个终端里跑东西（绝对不能乱发，
//! 会打断用户的程序）。所以每个对话在往终端里写包装脚本**之前**登记一条记录，
//! 命令结束（拿到结束标记、超时返回之后又结束、会话断开）时清掉。

use std::collections::HashMap;
use std::sync::Mutex;

/// 一条正在终端里执行的 AI 命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRun {
    /// 目标会话（真实会话 ID，不是 `host-<id>`）。
    pub session_id: String,
    /// 这次执行的随机结束标记：命令的唯一标识，清理时只清自己这条。
    pub marker: String,
}

/// 对话（scope）→ 正在跑的命令们。
///
/// 一个对话可以同时在**不同终端**里各有一条命令在跑（智能体并行发了两个终端的调用）；同一个
/// 终端上不会叠两条在跑的 AI 命令（由
/// [`TerminalState::try_acquire`](crate::ai::pty::TerminalState::try_acquire) 保证），但超时返回后
/// 仍在跑的那条会继续留在表里，后来的命令照样登记。所以每个对话是一个列表：按对话为键直接覆盖
/// 的话，停止按钮只找得到最后登记的那条，别的终端里的命令收不到 Ctrl+C。
#[derive(Debug, Default)]
pub struct TerminalRuns {
    runs: Mutex<HashMap<String, Vec<TerminalRun>>>,
}

impl TerminalRuns {
    /// 记下"这个对话在这个会话里跑着这条命令"。同一个结束标记重复登记只留一条。
    pub fn register(&self, scope_id: &str, session_id: &str, marker: &str) {
        if let Ok(mut runs) = self.runs.lock() {
            let list = runs.entry(scope_id.to_string()).or_default();
            list.retain(|run| run.marker != marker);
            list.push(TerminalRun {
                session_id: session_id.to_string(),
                marker: marker.to_string(),
            });
        }
    }

    /// 命令结束时清掉这一条记录。
    ///
    /// 只按 `marker` 移除自己那条：这个对话别的终端里还在跑的命令、以及后来登记的命令，
    /// 记录都要留着（否则停止按钮就找不到它们）。
    pub fn clear(&self, scope_id: &str, marker: &str) {
        if let Ok(mut runs) = self.runs.lock() {
            if let Some(list) = runs.get_mut(scope_id) {
                list.retain(|run| run.marker != marker);
                if list.is_empty() {
                    runs.remove(scope_id);
                }
            }
        }
    }

    /// 会话断开时的兜底清理（不知道当时是哪条命令）。
    pub fn clear_session(&self, session_id: &str) {
        if let Ok(mut runs) = self.runs.lock() {
            for list in runs.values_mut() {
                list.retain(|run| run.session_id != session_id);
            }
            runs.retain(|_, list| !list.is_empty());
        }
    }

    /// 现在一共有几条 AI 命令在终端里跑（自动更新重启前的检查用）。
    pub fn count(&self) -> usize {
        self.runs
            .lock()
            .map(|runs| runs.values().map(Vec::len).sum())
            .unwrap_or_default()
    }

    /// 这个对话在终端里跑着的全部 AI 命令（停止按钮要向它们所在的每个终端发 Ctrl+C）。
    pub fn running_all(&self, scope_id: &str) -> Vec<TerminalRun> {
        self.runs
            .lock()
            .ok()
            .and_then(|runs| runs.get(scope_id).cloned())
            .unwrap_or_default()
    }

    /// 这个对话最近登记的那条在跑的命令；没有就是 `None`（有没有在跑的判断用）。
    pub fn running(&self, scope_id: &str) -> Option<TerminalRun> {
        self.running_all(scope_id).pop()
    }

    /// 这条命令（按结束标记）还登记着吗。
    pub fn is_registered(&self, scope_id: &str, marker: &str) -> bool {
        self.running_all(scope_id)
            .iter()
            .any(|run| run.marker == marker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_clear_by_marker() {
        let runs = TerminalRuns::default();
        assert!(runs.running("s1").is_none(), "一开始没有记录");
        runs.register("s1", "sess-1", "__M1__");
        assert_eq!(
            runs.running("s1"),
            Some(TerminalRun {
                session_id: "sess-1".to_string(),
                marker: "__M1__".to_string(),
            })
        );
        // 标记对得上才清。
        runs.clear("s1", "__OTHER__");
        assert!(runs.running("s1").is_some(), "标记不对不能清");
        runs.clear("s1", "__M1__");
        assert!(runs.running("s1").is_none(), "标记对上就清掉");
        assert_eq!(runs.count(), 0);
    }

    #[test]
    fn a_stale_clear_does_not_remove_a_newer_command() {
        // 超时那条命令的迟到清理不能把同一个对话后来的命令删掉。
        let runs = TerminalRuns::default();
        runs.register("s1", "sess-1", "__OLD__");
        runs.register("s1", "sess-1", "__NEW__");
        runs.clear("s1", "__OLD__");
        assert_eq!(
            runs.running("s1").map(|run| run.marker),
            Some("__NEW__".to_string()),
            "新命令的记录要在"
        );
    }

    /// 2026-11-11 审查：以对话为键直接覆盖，同一对话在两个终端各有一条命令时，先登记的那条
    /// 停止按钮找不到，不会收到 Ctrl+C。
    #[test]
    fn one_scope_can_have_commands_running_in_several_terminals() {
        let runs = TerminalRuns::default();
        runs.register("s1", "sess-1", "__A__");
        runs.register("s1", "sess-2", "__B__");
        let all = runs.running_all("s1");
        assert_eq!(all.len(), 2, "两条都要登记着：{all:?}");
        assert!(all.iter().any(|run| run.session_id == "sess-1"));
        assert!(all.iter().any(|run| run.session_id == "sess-2"));
        assert_eq!(runs.count(), 2);
        // 命令结束只移除自己那条
        runs.clear("s1", "__B__");
        let left = runs.running_all("s1");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].marker, "__A__");
        assert!(runs.is_registered("s1", "__A__"));
        assert!(!runs.is_registered("s1", "__B__"));
        runs.clear("s1", "__A__");
        assert!(runs.running_all("s1").is_empty());
    }

    #[test]
    fn registering_the_same_marker_twice_keeps_one_record() {
        let runs = TerminalRuns::default();
        runs.register("s1", "sess-1", "__A__");
        runs.register("s1", "sess-1", "__A__");
        assert_eq!(runs.count(), 1);
    }

    #[test]
    fn scopes_are_independent() {
        let runs = TerminalRuns::default();
        runs.register("s1", "sess-1", "__M1__");
        runs.register("s2", "sess-2", "__M2__");
        runs.clear("s1", "__M1__");
        assert!(runs.running("s1").is_none());
        assert!(runs.running("s2").is_some(), "别的对话不受影响");
    }

    #[test]
    fn clear_session_drops_only_that_session() {
        let runs = TerminalRuns::default();
        runs.register("s1", "sess-1", "__M1__");
        runs.register("s1", "sess-2", "__M3__");
        runs.register("s2", "sess-2", "__M2__");
        runs.clear_session("sess-1");
        assert!(
            runs.running("s1").is_some_and(|run| run.marker == "__M3__"),
            "断开会话的记录清掉，同一对话在别的会话里的命令留着"
        );
        assert!(runs.running("s2").is_some(), "其他会话不受影响");
        runs.clear_session("sess-2");
        assert_eq!(runs.count(), 0);
    }
}
