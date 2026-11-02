//! AI 对话的目标列表（0.2.14 `ai/scopeTargets.ts`）：当前标签的主机 + 用户 @ 的主机 + 切焦点时
//! 自动保留下来的主机。纯函数，不依赖 GPUI。
//!
//! ZeeTerm 的执行目标按主机建模（`host-<id>`）。「绑定会话」（这台主机的命令落在哪个终端）不在这里算，
//! 而是执行时由终端登记表 [`crate::ai_terminals::Terminals::for_host`] 按「聚焦窗格 → 最近聚焦 → 最早
//! 打开」现取（见 `agent_host::NativeBridgeHost::execution_session`），焦点一变就跟着变，不必为此重新
//! 同步目标。快速连接（没有主机记录）的标签没有目标（0.2.14 里它的目标是会话编号，见 V2-G10b 报告）。

use std::collections::HashSet;

/// 留在对话范围里的一台主机：用户 @ 进来的，或者焦点切走时自动保留的原来那台。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub host_id: i64,
    pub name: String,
    /// 焦点切走时自动保留下来的（不是用户 @ 的）：它的标签一关就跟着消失。
    pub retained: bool,
}

/// 目标栏里的一个标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host_id: i64,
    pub name: String,
    /// 是当前标签聚焦窗格所在的主机（标签上标「当前」，不能 ×）。
    pub current: bool,
    /// 是被 @ 或被自动保留进来的（可以 ×）。
    pub pinned: bool,
}

/// 拼目标列表：当前在最前，后面是固定的。**一台主机只有一个目标**。
/// 自动保留的目标跟着标签走：这台主机已经没有任何标签开着，就不再留。
pub fn build(
    current: Option<(i64, &str)>,
    pinned: &[Pin],
    open_hosts: &HashSet<i64>,
) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    if let Some((host_id, name)) = current {
        out.push(Target {
            host_id,
            name: name.to_owned(),
            current: true,
            pinned: pinned.iter().any(|pin| pin.host_id == host_id),
        });
    }
    for pin in pinned {
        if out.iter().any(|target| target.host_id == pin.host_id) {
            continue;
        }
        if pin.retained && !open_hosts.contains(&pin.host_id) {
            continue;
        }
        out.push(Target {
            host_id: pin.host_id,
            name: pin.name.clone(),
            current: false,
            pinned: true,
        });
    }
    out
}

/// 焦点切走时保留原来的「当前」：对话进行中，原来那台主机的标签还开着、又没被代表，就记成一个
/// 自动保留的固定目标（显示成可以 × 掉的标签）。新的「当前」是同一台主机时什么都不用做。
pub fn retain_previous(
    pinned: &[Pin],
    previous: Option<(i64, &str)>,
    current_host: Option<i64>,
    open_hosts: &HashSet<i64>,
) -> Vec<Pin> {
    let Some((host_id, name)) = previous else {
        return pinned.to_vec();
    };
    if !open_hosts.contains(&host_id)
        || current_host == Some(host_id)
        || pinned.iter().any(|pin| pin.host_id == host_id)
    {
        return pinned.to_vec();
    }
    let mut next = pinned.to_vec();
    next.push(Pin {
        host_id,
        name: name.to_owned(),
        retained: true,
    });
    next
}

/// 去掉标签已经全关了的自动保留目标。
pub fn prune_retained(pinned: &[Pin], open_hosts: &HashSet<i64>) -> Vec<Pin> {
    pinned
        .iter()
        .filter(|pin| !(pin.retained && !open_hosts.contains(&pin.host_id)))
        .cloned()
        .collect()
}

/// @ 一台主机后的固定目标列表，以及这次要不要提示「已经在对话里了」。
pub fn pin_host(
    pinned: &[Pin],
    host_id: i64,
    name: &str,
    current_host: Option<i64>,
) -> (Vec<Pin>, bool) {
    if let Some(index) = pinned.iter().position(|pin| pin.host_id == host_id) {
        if pinned[index].retained {
            // 自动保留的被用户明确 @ 了：从此是用户固定的（标签关了也不消失）。
            let mut next = pinned.to_vec();
            next[index].retained = false;
            return (next, false);
        }
        return (pinned.to_vec(), true);
    }
    let mut next = pinned.to_vec();
    next.push(Pin {
        host_id,
        name: name.to_owned(),
        retained: false,
    });
    // 当前标签就是这台主机：目标本来就在列表里，只记一个固定标记（切走焦点后它仍留着）。
    (next, current_host == Some(host_id))
}

/// 去掉一个固定目标（标签上的 ×）。
pub fn unpin(pinned: &[Pin], host_id: i64) -> Vec<Pin> {
    pinned
        .iter()
        .filter(|pin| pin.host_id != host_id)
        .cloned()
        .collect()
}

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
            !targets.iter().any(|target| target.host_id == *id)
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

    fn open(ids: &[i64]) -> HashSet<i64> {
        ids.iter().copied().collect()
    }

    fn pin(host_id: i64, name: &str, retained: bool) -> Pin {
        Pin {
            host_id,
            name: name.into(),
            retained,
        }
    }

    #[test]
    fn current_comes_first_and_one_host_is_one_target() {
        let targets = build(
            Some((1, "web-1")),
            &[pin(1, "web-1", false), pin(2, "db", false)],
            &open(&[1]),
        );
        assert_eq!(targets.len(), 2, "固定的那台和当前是同一台，只算一个");
        assert!(targets[0].current && targets[0].pinned);
        assert_eq!(targets[1].host_id, 2);
        assert!(!targets[1].current);
    }

    #[test]
    fn retained_targets_follow_their_tabs() {
        let pins = [pin(2, "db", true), pin(3, "cache", false)];
        let alive = build(Some((1, "web-1")), &pins, &open(&[1, 2]));
        assert_eq!(alive.len(), 3);
        let gone = build(Some((1, "web-1")), &pins, &open(&[1]));
        assert_eq!(
            gone.iter().map(|t| t.host_id).collect::<Vec<_>>(),
            vec![1, 3],
            "自动保留的标签关了就消失，@ 的不消失"
        );
        assert_eq!(
            prune_retained(&pins, &open(&[1])),
            vec![pin(3, "cache", false)]
        );
    }

    #[test]
    fn leaving_a_host_keeps_it_unless_it_is_covered() {
        let keep = retain_previous(&[], Some((1, "web-1")), Some(2), &open(&[1, 2]));
        assert_eq!(keep, vec![pin(1, "web-1", true)]);
        assert!(
            retain_previous(&[], Some((1, "web-1")), Some(1), &open(&[1])).is_empty(),
            "新当前是同一台主机"
        );
        assert!(
            retain_previous(&[], Some((1, "web-1")), Some(2), &open(&[2])).is_empty(),
            "原来那台的标签已经关了"
        );
        let covered = [pin(1, "web-1", false)];
        assert_eq!(
            retain_previous(&covered, Some((1, "web-1")), Some(2), &open(&[1])),
            covered
        );
        assert!(retain_previous(&[], None, Some(2), &open(&[2])).is_empty());
    }

    #[test]
    fn pinning_an_already_present_host_is_reported() {
        let (next, already) = pin_host(&[], 1, "web-1", Some(1));
        assert!(already, "当前标签就是它");
        assert_eq!(next.len(), 1);
        let (_, already) = pin_host(&next, 1, "web-1", Some(2));
        assert!(already, "第二次 @ 同一台");
        let (promoted, already) = pin_host(&[pin(1, "web-1", true)], 1, "web-1", Some(2));
        assert!(!already);
        assert!(!promoted[0].retained, "明确 @ 了，从此是用户固定的");
        assert_eq!(unpin(&promoted, 1), vec![]);
    }

    #[test]
    fn mention_picker_filters_and_skips_hosts_already_in() {
        let hosts = vec![
            (1, "web-1".to_owned()),
            (2, "Web-2".to_owned()),
            (3, "db".to_owned()),
        ];
        let targets = build(Some((1, "web-1")), &[], &open(&[1]));
        let rows = picker_rows(&hosts, "WEB", &targets);
        assert_eq!(rows.len(), 1, "web-1 已经在目标里");
        assert_eq!(rows[0].0, 2);
        assert_eq!(picker_rows(&hosts, "", &targets).len(), 2);
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
