//! AI 对话里 `@` 选主机的纯函数（0.2.14 `ai/scopeTargets.ts` 的 `@` 部分）：用户 @ 进来的固定主机、
//! 弹出列表的过滤、输入框里 `@词` 的解析。纯函数，不依赖 GPUI。
//!
//! 目标列表本身（绑定的主机 + @ 的主机）、锁定 / 改绑 / 失效的规则在 [`super::anchor`]。ZeeTerm 的执行目标
//! 按主机建模（`host-<id>`）。「这台主机的命令落在哪个终端」不在这里算，而是执行时由终端登记表
//! [`crate::ai_terminals::Terminals::for_host`] 按「聚焦窗格 → 最近聚焦 → 最早打开」现取（见
//! `agent_host::NativeBridgeHost::execution_session`），焦点一变就跟着变，不必为此重新同步目标。

use super::anchor::Target;

/// `@` 选主机弹出的列表：按输入的字过滤（名字、不分大小写），已经在目标里的排除。
pub fn picker_rows<'a>(
    hosts: &'a [(i64, String)],
    query: &str,
    targets: &[Target],
) -> Vec<&'a (i64, String)> {
    let needle = query.trim().to_lowercase();
    hosts
        .iter()
        .filter(|(id, name)| {
            !targets.iter().any(|target| target.host_id() == Some(*id))
                && (needle.is_empty() || name.to_lowercase().contains(&needle))
        })
        .collect()
}

/// 输入框末尾正在输入的 `@词`：返回要拿去过滤的词（不含 @）。`@` 前面必须是开头或空白。
pub fn mention_query(text: &str) -> Option<String> {
    let at = text.rfind('@')?;
    let before_ok = at == 0
        || text[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
    let tail = &text[at + 1..];
    (before_ok && !tail.contains(char::is_whitespace)).then(|| tail.to_owned())
}

/// 选中一台主机后，把输入框末尾的 `@词` 去掉。
pub fn strip_mention(text: &str) -> String {
    match text.rfind('@') {
        Some(at) if mention_query(text).is_some() => text[..at].to_owned(),
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::main_window::anchor::Key;

    fn target(host_id: i64, name: &str) -> Target {
        Target {
            key: Key::Host(host_id),
            name: name.into(),
            current: true,
            pinned: false,
            locked: false,
            dangling: false,
        }
    }

    #[test]
    fn mention_picker_filters_and_skips_hosts_already_in() {
        let hosts = vec![
            (1, "web-1".to_owned()),
            (2, "Web-2".to_owned()),
            (3, "db".to_owned()),
        ];
        let targets = vec![target(1, "web-1")];
        let rows = picker_rows(&hosts, "WEB", &targets);
        assert_eq!(rows.len(), 1, "web-1 已经在目标里");
        assert_eq!(rows[0].0, 2);
        assert_eq!(picker_rows(&hosts, "", &targets).len(), 2);
        // 快速连接终端当目标：不占主机库里任何一台的位置。
        let quick = vec![Target {
            key: Key::Terminal("term-1".into()),
            ..target(0, "ops@192.0.2.9")
        }];
        assert_eq!(picker_rows(&hosts, "", &quick).len(), 3);
    }

    #[test]
    fn mention_query_needs_an_at_after_whitespace() {
        assert_eq!(mention_query("看一下 @we"), Some("we".into()));
        assert_eq!(mention_query("@"), Some(String::new()));
        assert_eq!(mention_query("mail@x"), None, "邮箱里的 @ 不算");
        assert_eq!(mention_query("@we 然后"), None, "词已经结束");
        assert_eq!(mention_query("没有"), None);
        assert_eq!(strip_mention("看一下 @we"), "看一下 ");
        assert_eq!(strip_mention("mail@x"), "mail@x");
    }
}
