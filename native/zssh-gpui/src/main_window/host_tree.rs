//! 主机树的纯逻辑：排序、搜索过滤、把「分组 → 主机」展开成一行一行的可见列表。
//!
//! 规则取自 0.2.14 的 `ui/src/state/hostTree.ts`（`compareNames` / `compareOrder` /
//! `treeFilterFor` / `hostsUnder` / `groupsUnder`）和 `VaultView.tsx` 里 `HostTreeAside`
//! 的行顺序。不依赖 GPUI，单测直接盯住这些规则。
//!
//! 名字排序：先分数字 / 英文 / 中文 / 其他四段，段内交给 [`super::collation`]
//! （Windows 的 `zh-CN` 排序：中文按拼音、数字按数值、英文不分大小写，对应前端的
//! `Intl.Collator("zh-Hans-CN")`）。

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};

use zssh_core::model::{Group, Host};

/// 「未分组」在展开状态里的键；分组的键是 `g<id>`。
pub const UNGROUPED_KEY: &str = "ungrouped";

/// 搜索时「未分组」那一行的名字（名字本身也能被搜到）。
pub const UNGROUPED_LABEL: &str = "未分组";

pub fn group_key(group_id: Option<i64>) -> String {
    match group_id {
        Some(id) => format!("g{id}"),
        None => UNGROUPED_KEY.to_owned(),
    }
}

/// 读回保存的展开键，丢掉不认识的（文件被手改或版本不同都不能让树坏掉）。
pub fn parse_expanded(raw: &[String]) -> BTreeSet<String> {
    raw.iter()
        .map(|part| part.trim())
        .filter(|part| {
            *part == UNGROUPED_KEY
                || part.strip_prefix('g').is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .map(str::to_owned)
        .collect()
}

/// 名字开头是哪一类：数字 → 英文字母 → 中文 → 其他符号。
fn name_class(name: &str) -> u8 {
    let Some(first) = name.trim_start().chars().next() else {
        return 3;
    };
    if first.is_ascii_digit() {
        0
    } else if first.is_ascii_alphabetic() {
        1
    } else if is_han(first) {
        2
    } else {
        3
    }
}

fn is_han(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// 主机、分组按名字排：先按开头分成 数字、英文、中文、其他符号 四段，段内按自然序。
pub fn compare_names(a: &str, b: &str) -> Ordering {
    name_class(a)
        .cmp(&name_class(b))
        .then_with(|| super::collation::compare(a.trim_start(), b.trim_start()))
        .then_with(|| a.cmp(b))
}

/// 同一层的显示顺序：`sort > 0` 的排前面、按 `sort` 从小到大；`sort == 0`（没调整过）
/// 排后面、按名字；`sort` 相同用名字兜底。
pub fn compare_order(a: (i64, &str), b: (i64, &str)) -> Ordering {
    let (a_set, b_set) = (a.0 > 0, b.0 > 0);
    if a_set != b_set {
        return if a_set {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if a_set && a.0 != b.0 {
        return a.0.cmp(&b.0);
    }
    compare_names(a.1, b.1)
}

/// 搜索框里的文字匹配这台主机（名字、地址、用户名、标签）。空文字都匹配。
pub fn host_matches(host: &Host, text: &str) -> bool {
    let query = text.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    host.name.to_lowercase().contains(&query)
        || super::protocol_form::host_search_address(host)
            .to_lowercase()
            .contains(&query)
        || host.username.to_lowercase().contains(&query)
        || host
            .tags
            .iter()
            .any(|tag| tag.to_lowercase().contains(&query))
}

/// 主机树搜索框的过滤结果。没在搜索时是 `None`。
/// - 主机显示：自己匹配，或所在分组（含上级分组）的名字匹配；
/// - 分组显示：名字匹配（连同整个子树）、或子树里有显示的主机；显示的分组一律自动展开，
///   不碰用户自己的展开状态，清空搜索就恢复原样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFilter {
    /// 去掉首尾空白并转小写的搜索词。
    pub text: String,
    pub host_ids: HashSet<i64>,
    pub group_ids: HashSet<i64>,
    /// 「未分组」行要不要显示；平铺模式下表示有没有显示的根层主机。
    pub ungrouped: bool,
    /// 每个显示的分组（含子分组）里显示的主机数。
    pub counts: HashMap<i64, usize>,
    pub total: usize,
}

/// `flat`：「未分组」改过名、不分组的主机直接排在树根层：没有「未分组」这一行，
/// 它的名字也不再能被搜到。
pub fn tree_filter(
    groups: &[Group],
    hosts: &[Host],
    raw_text: &str,
    flat: bool,
) -> Option<TreeFilter> {
    let text = raw_text.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }
    let parent_of: HashMap<i64, Option<i64>> = groups.iter().map(|g| (g.id, g.parent_id)).collect();
    let name_of: HashMap<i64, String> = groups
        .iter()
        .map(|g| (g.id, g.name.to_lowercase()))
        .collect();
    // 从这个分组一路往上（含自己），防环：最多走分组个数那么多步。
    let chain = |group_id: Option<i64>| -> Vec<i64> {
        let mut ids = Vec::new();
        let mut current = group_id;
        for _ in 0..=groups.len() {
            let Some(id) = current else { break };
            ids.push(id);
            current = parent_of.get(&id).copied().flatten();
        }
        ids
    };
    let name_hit = |group_id: i64| {
        chain(Some(group_id))
            .iter()
            .any(|id| name_of.get(id).is_some_and(|name| name.contains(&text)))
    };
    let ungrouped_name_hit = !flat && UNGROUPED_LABEL.contains(&text);

    let mut host_ids = HashSet::new();
    let mut group_ids = HashSet::new();
    let mut counts: HashMap<i64, usize> = HashMap::new();
    let mut ungrouped = ungrouped_name_hit;
    for group in groups {
        // 名字命中的分组连同它的整条祖先路径一起留下：嵌在没命中的父分组里的（哪怕是空分组）
        // 也要看得到。
        if name_hit(group.id) {
            group_ids.extend(chain(Some(group.id)));
        }
    }
    for host in hosts {
        let visible = host_matches(host, &text)
            || match host.group_id {
                None => ungrouped_name_hit,
                Some(id) => name_hit(id),
            };
        if !visible {
            continue;
        }
        host_ids.insert(host.id);
        match host.group_id {
            None => ungrouped = true,
            Some(id) => {
                for ancestor in chain(Some(id)) {
                    group_ids.insert(ancestor);
                    *counts.entry(ancestor).or_insert(0) += 1;
                }
            }
        }
    }
    Some(TreeFilter {
        text,
        total: host_ids.len(),
        host_ids,
        group_ids,
        ungrouped,
        counts,
    })
}

/// 某个分组（`None` = 未分组/根层）下面直接挂的主机，按 [`compare_order`] 排；搜索时只留过滤结果里的。
pub fn hosts_under<'a>(
    hosts: &'a [Host],
    group_id: Option<i64>,
    filter: Option<&TreeFilter>,
) -> Vec<&'a Host> {
    let mut out: Vec<&Host> = hosts
        .iter()
        .filter(|host| {
            host.group_id == group_id && filter.is_none_or(|f| f.host_ids.contains(&host.id))
        })
        .collect();
    out.sort_by(|a, b| compare_order((a.sort, &a.name), (b.sort, &b.name)));
    out
}

/// 某个父分组（`None` = 根）下面直接的子分组，规则同上。
pub fn groups_under<'a>(
    groups: &'a [Group],
    parent_id: Option<i64>,
    filter: Option<&TreeFilter>,
) -> Vec<&'a Group> {
    let mut out: Vec<&Group> = groups
        .iter()
        .filter(|group| {
            group.parent_id == parent_id && filter.is_none_or(|f| f.group_ids.contains(&group.id))
        })
        .collect();
    out.sort_by(|a, b| compare_order((a.sort, &a.name), (b.sort, &b.name)));
    out
}

/// 一个分组（含子分组）里一共有多少台主机。
pub fn total_under(groups: &[Group], hosts: &[Host], group_id: i64) -> usize {
    let mut seen = HashSet::new();
    let mut stack = vec![group_id];
    let mut ids = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        ids.insert(id);
        stack.extend(
            groups
                .iter()
                .filter(|g| g.parent_id == Some(id))
                .map(|g| g.id),
        );
    }
    hosts
        .iter()
        .filter(|host| host.group_id.is_some_and(|id| ids.contains(&id)))
        .count()
}

/// 树里一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    /// 「全部主机」。
    All {
        count: usize,
    },
    Group {
        id: i64,
        name: String,
        depth: usize,
        expanded: bool,
        count: usize,
    },
    /// 「未分组」行（平铺模式下不出现）。
    Ungrouped {
        expanded: bool,
        count: usize,
    },
    Host {
        id: i64,
        depth: usize,
    },
    /// 平铺模式下根分组和根层主机之间的细线。
    Divider,
}

/// 展开状态怎么看：搜索时显示出来的分组一律展开；没搜索看用户自己的展开键。
fn is_expanded(expanded: &BTreeSet<String>, key: &str, filter: Option<&TreeFilter>) -> bool {
    filter.is_some() || expanded.contains(key)
}

/// 把整棵树展开成可见的行（从上到下的显示顺序）。
pub fn visible_rows(
    groups: &[Group],
    hosts: &[Host],
    filter: Option<&TreeFilter>,
    expanded: &BTreeSet<String>,
    flat: bool,
) -> Vec<TreeRow> {
    let mut rows = vec![TreeRow::All {
        count: filter.map_or(hosts.len(), |f| f.total),
    }];
    struct Ctx<'a> {
        groups: &'a [Group],
        hosts: &'a [Host],
        filter: Option<&'a TreeFilter>,
        expanded: &'a BTreeSet<String>,
    }
    fn branch(
        rows: &mut Vec<TreeRow>,
        ctx: &Ctx<'_>,
        group: &Group,
        depth: usize,
        visited: &mut HashSet<i64>,
    ) {
        // 分组数据里万一有环（手改库），别无限递归。
        if !visited.insert(group.id) {
            return;
        }
        let open = is_expanded(ctx.expanded, &group_key(Some(group.id)), ctx.filter);
        rows.push(TreeRow::Group {
            id: group.id,
            name: group.name.clone(),
            depth,
            expanded: open,
            count: ctx.filter.map_or_else(
                || total_under(ctx.groups, ctx.hosts, group.id),
                |f| f.counts.get(&group.id).copied().unwrap_or(0),
            ),
        });
        if open {
            for child in groups_under(ctx.groups, Some(group.id), ctx.filter) {
                branch(rows, ctx, child, depth + 1, visited);
            }
            for host in hosts_under(ctx.hosts, Some(group.id), ctx.filter) {
                rows.push(TreeRow::Host {
                    id: host.id,
                    depth: depth + 1,
                });
            }
        }
        visited.remove(&group.id);
    }
    let ctx = Ctx {
        groups,
        hosts,
        filter,
        expanded,
    };
    let mut visited = HashSet::new();
    let roots = groups_under(groups, None, filter);
    let root_count = roots.len();
    for group in roots {
        branch(&mut rows, &ctx, group, 0, &mut visited);
    }
    let root_hosts = hosts_under(hosts, None, filter);
    if flat {
        if root_count > 0 && !root_hosts.is_empty() {
            rows.push(TreeRow::Divider);
        }
        for host in root_hosts {
            rows.push(TreeRow::Host {
                id: host.id,
                depth: 0,
            });
        }
    } else if filter.is_none_or(|f| f.ungrouped) {
        let open = is_expanded(expanded, UNGROUPED_KEY, filter);
        rows.push(TreeRow::Ungrouped {
            expanded: open,
            count: root_hosts.len(),
        });
        if open {
            for host in root_hosts {
                rows.push(TreeRow::Host {
                    id: host.id,
                    depth: 1,
                });
            }
        }
    }
    rows
}

/// 搜索结果里主机从上到下的显示顺序（↑ ↓ 在这个顺序里移动，回车连第一个）。
pub fn filtered_host_order(
    groups: &[Group],
    hosts: &[Host],
    filter: &TreeFilter,
    flat: bool,
) -> Vec<i64> {
    let expanded = BTreeSet::new();
    visible_rows(groups, hosts, Some(filter), &expanded, flat)
        .into_iter()
        .filter_map(|row| match row {
            TreeRow::Host { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

/// 搜索结果里 ↑ ↓ 之后光标落在哪台：还没选过（或选的那台已经不在结果里）时，向下从第一台
/// 开始、向上从最后一台开始；走到头就停在头上，不绕圈。结果是空的返回 `None`。
pub fn step_cursor(order: &[i64], current: Option<i64>, delta: isize) -> Option<i64> {
    if order.is_empty() {
        return None;
    }
    let at = current.and_then(|id| order.iter().position(|candidate| *candidate == id));
    let next = match at {
        None if delta > 0 => 0,
        None => order.len() - 1,
        Some(at) => (at as isize + delta).clamp(0, order.len() as isize - 1) as usize,
    };
    order.get(next).copied()
}

/// 「平不平铺」的有效值（同 `effectiveFlat`）：同步键 `vault_ungrouped_flat` 在库里存过值
/// 就用它；没存过就看本机默认是不是 `flat`。库里存的都是 JSON 文本（`true`、`"flat"`），
/// 读不懂的值按「没存过」处理，不猜。
pub fn effective_flat(stored_flag: Option<&str>, local_default: Option<&str>) -> bool {
    let json = |raw: &str| serde_json::from_str::<serde_json::Value>(raw).ok();
    if let Some(serde_json::Value::Bool(flag)) = stored_flag.and_then(json) {
        return flag;
    }
    matches!(
        local_default.and_then(json),
        Some(serde_json::Value::String(text)) if text == "flat"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::model::{AuthMethod, ProxyMode};

    fn host(id: i64, group: Option<i64>, name: &str) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: group,
            name: name.to_owned(),
            host: format!("10.0.0.{id}"),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn group(id: i64, parent: Option<i64>, name: &str) -> Group {
        Group {
            id,
            name: name.to_owned(),
            parent_id: parent,
            sort: 0,
        }
    }

    fn expanded(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| (*k).to_owned()).collect()
    }

    #[test]
    fn names_sort_digits_then_latin_then_han_then_symbols() {
        let mut names = vec!["_tmp", "中文机", "web", "10-db", "2-db", "Alpha"];
        names.sort_by(|a, b| compare_names(a, b));
        assert_eq!(names, ["2-db", "10-db", "Alpha", "web", "中文机", "_tmp"]);
    }

    #[test]
    fn numbers_compare_by_value_and_latin_ignores_case() {
        assert_eq!(compare_names("web2", "web10"), Ordering::Less);
        assert_eq!(compare_names("alpha", "Beta"), Ordering::Less);
        // 只有大小写不同时用原串兜底，结果稳定即可。
        assert_ne!(compare_names("Web", "web"), Ordering::Equal);
    }

    #[test]
    fn manual_order_comes_first_then_names() {
        let mut items = [(0, "b"), (0, "a"), (2, "z"), (1, "y"), (3, "m")];
        items.sort_by(|a, b| compare_order(*a, *b));
        assert_eq!(
            items.map(|(_, name)| name),
            ["y", "z", "m", "a", "b"],
            "sort>0 的先按 sort，sort==0 的按名字"
        );
        // sort 相同用名字兜底。
        assert_eq!(compare_order((5, "a"), (5, "b")), Ordering::Less);
    }

    #[test]
    fn expanded_keys_drop_garbage() {
        let raw: Vec<String> = ["g1", " g22 ", "ungrouped", "g", "gx", "h3", "", "g-1"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(parse_expanded(&raw), expanded(&["g1", "g22", "ungrouped"]));
    }

    #[test]
    fn rows_follow_groups_then_hosts_then_ungrouped() {
        let groups = [
            group(1, None, "prod"),
            group(2, Some(1), "db"),
            group(3, None, "dev"),
        ];
        let hosts = [
            host(10, Some(2), "pg-1"),
            host(11, Some(1), "web-1"),
            host(12, None, "scratch"),
            host(13, Some(3), "dev-1"),
        ];
        let open = expanded(&["g1", "g2", UNGROUPED_KEY]);
        let rows = visible_rows(&groups, &hosts, None, &open, false);
        assert_eq!(
            rows,
            vec![
                TreeRow::All { count: 4 },
                TreeRow::Group {
                    id: 3,
                    name: "dev".into(),
                    depth: 0,
                    expanded: false,
                    count: 1
                },
                TreeRow::Group {
                    id: 1,
                    name: "prod".into(),
                    depth: 0,
                    expanded: true,
                    count: 2
                },
                TreeRow::Group {
                    id: 2,
                    name: "db".into(),
                    depth: 1,
                    expanded: true,
                    count: 1
                },
                TreeRow::Host { id: 10, depth: 2 },
                TreeRow::Host { id: 11, depth: 1 },
                TreeRow::Ungrouped {
                    expanded: true,
                    count: 1
                },
                TreeRow::Host { id: 12, depth: 1 },
            ],
            "子分组排在主机前，收起的分组不展开子项"
        );
    }

    #[test]
    fn collapsed_ungrouped_hides_its_hosts_and_flat_mode_puts_them_at_the_root() {
        let groups = [group(1, None, "prod")];
        let hosts = [host(1, Some(1), "a"), host(2, None, "b")];
        let rows = visible_rows(&groups, &hosts, None, &expanded(&[]), false);
        assert_eq!(
            rows.last(),
            Some(&TreeRow::Ungrouped {
                expanded: false,
                count: 1
            })
        );

        let flat = visible_rows(&groups, &hosts, None, &expanded(&[]), true);
        assert_eq!(
            flat[flat.len() - 2..],
            [TreeRow::Divider, TreeRow::Host { id: 2, depth: 0 }],
            "平铺：根分组下面一条细线，然后是根层主机"
        );
        // 只有根层主机、没有分组时不画细线。
        let only_hosts = visible_rows(&[], &hosts, None, &expanded(&[]), true);
        assert!(!only_hosts.contains(&TreeRow::Divider));
    }

    #[test]
    fn a_group_cycle_in_bad_data_does_not_recurse_forever() {
        let groups = [group(1, Some(2), "a"), group(2, Some(1), "b")];
        // 两个都不是根，所以什么分组也不画；至少不能卡死。
        let rows = visible_rows(&groups, &[], None, &expanded(&["g1", "g2"]), false);
        assert_eq!(rows[0], TreeRow::All { count: 0 });
        // 环上的分组名字命中时，祖先链也要能终止。
        let filter = tree_filter(&groups, &[host(1, Some(1), "x")], "a", false);
        assert!(filter.is_some());
    }

    #[test]
    fn search_matches_name_address_user_tags_and_group_names() {
        let groups = [group(1, None, "Production"), group(2, Some(1), "db")];
        let mut tagged = host(3, None, "misc");
        tagged.tags = vec!["blue".to_owned()];
        let hosts = [host(1, Some(2), "pg-1"), host(2, None, "web-1"), tagged];
        // 分组名命中：整个子树里的主机都显示。
        let f = tree_filter(&groups, &hosts, "  PROD ", false).unwrap();
        assert_eq!(f.text, "prod");
        assert!(f.host_ids.contains(&1) && !f.host_ids.contains(&2));
        assert!(f.group_ids.contains(&1) && f.group_ids.contains(&2));
        assert_eq!(f.counts.get(&1), Some(&1));
        // 标签、地址、用户名。
        assert!(
            tree_filter(&groups, &hosts, "blue", false)
                .unwrap()
                .host_ids
                .contains(&3)
        );
        assert!(
            tree_filter(&groups, &hosts, "10.0.0.2", false)
                .unwrap()
                .host_ids
                .contains(&2)
        );
        assert!(tree_filter(&groups, &hosts, "OPS", false).unwrap().total == 3);
        // 空搜索不是过滤。
        assert_eq!(tree_filter(&groups, &hosts, "   ", false), None);
    }

    #[test]
    fn search_expands_matching_groups_without_touching_the_saved_state() {
        let groups = [group(1, None, "prod"), group(2, None, "dev")];
        let hosts = [host(1, Some(1), "web-1"), host(2, Some(2), "api")];
        let filter = tree_filter(&groups, &hosts, "web", false).unwrap();
        let rows = visible_rows(&groups, &hosts, Some(&filter), &expanded(&[]), false);
        assert_eq!(
            rows,
            vec![
                TreeRow::All { count: 1 },
                TreeRow::Group {
                    id: 1,
                    name: "prod".into(),
                    depth: 0,
                    expanded: true,
                    count: 1
                },
                TreeRow::Host { id: 1, depth: 1 },
            ],
            "不匹配的分组和主机不出现；匹配的自动展开"
        );
        assert_eq!(
            filtered_host_order(&groups, &hosts, &filter, false),
            vec![1]
        );
    }

    #[test]
    fn the_ungrouped_name_is_searchable_unless_flat() {
        let hosts = [host(1, None, "alpha")];
        assert!(tree_filter(&[], &hosts, "未分", false).unwrap().ungrouped);
        let flat = tree_filter(&[], &hosts, "未分", true).unwrap();
        assert!(!flat.ungrouped && flat.total == 0);
    }

    #[test]
    fn counts_include_subgroups() {
        let groups = [
            group(1, None, "a"),
            group(2, Some(1), "b"),
            group(3, Some(2), "c"),
        ];
        let hosts = [
            host(1, Some(3), "x"),
            host(2, Some(2), "y"),
            host(3, Some(1), "z"),
        ];
        assert_eq!(total_under(&groups, &hosts, 1), 3);
        assert_eq!(total_under(&groups, &hosts, 2), 2);
        assert_eq!(total_under(&groups, &hosts, 3), 1);
    }

    #[test]
    fn the_arrow_cursor_starts_at_an_end_and_stops_there() {
        let order = [4, 7, 9];
        assert_eq!(step_cursor(&order, None, 1), Some(4));
        assert_eq!(step_cursor(&order, None, -1), Some(9));
        assert_eq!(step_cursor(&order, Some(4), 1), Some(7));
        assert_eq!(step_cursor(&order, Some(9), 1), Some(9), "到底不绕圈");
        assert_eq!(step_cursor(&order, Some(4), -1), Some(4), "到顶不绕圈");
        // 选的那台已经被搜索结果过滤掉了：当作没选过。
        assert_eq!(step_cursor(&order, Some(99), 1), Some(4));
        assert_eq!(step_cursor(&[], Some(4), 1), None);
    }

    #[test]
    fn flat_default_follows_the_stored_flag_then_the_local_default() {
        let flat = Some("\"flat\"");
        assert!(effective_flat(Some("true"), None));
        assert!(!effective_flat(Some("false"), flat), "用户明确存过就用它");
        assert!(effective_flat(None, flat));
        assert!(!effective_flat(None, Some("\"classic\"")));
        assert!(!effective_flat(None, None));
        // 读不懂的值当作没存过。
        assert!(effective_flat(Some("maybe"), flat));
        assert!(!effective_flat(None, Some("flat")));
    }
}
