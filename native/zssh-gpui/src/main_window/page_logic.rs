//! 主机页（VaultView）和主机树共用的纯逻辑：显示选项、右边卡片 / 列表的主机顺序、
//! 同一层的排序操作（上移 / 下移 / 拖放插入）、最近打开、快速连接串、各处文案。
//!
//! 规则取自 0.2.14：`state/hostPage.ts`、`state/hostTree.ts`（`insertInLayer` /
//! `stepInLayer` / `sortByRecent`）、`components/vault/treeOrder.ts`、`hostDrag.tsx`
//! 里的 `dropHostAt` / `dropGroupAt`、`state/ungroupedFlat.ts`。不依赖 GPUI，单测直接盯规则。

use std::collections::{HashMap, HashSet};

use zssh_core::model::{
    AuthMethod, Group, Host, Identity, IdentityKind, Protocol, Proxy, ProxyMode,
};

use super::GroupSel;
use super::host_store::normalize_address;
use super::host_tree::{
    TreeFilter, compare_names, compare_order, filtered_host_order, groups_under, host_matches,
    hosts_under,
};

// ---- 显示选项（本机布局设置，键名和 0.2.14 一致） ----

/// 卡片 / 列表。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    #[default]
    List,
}

impl ViewMode {
    pub fn parse(raw: Option<&str>) -> Self {
        if raw == Some("grid") {
            Self::Grid
        } else {
            Self::List
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::List => "list",
        }
    }
}

/// 卡片 / 列表的疏密：紧凑（默认，一屏多放几台）/ 舒适。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Density {
    #[default]
    Compact,
    Comfortable,
}

impl Density {
    /// 只认 `comfortable`，其余（含没存过）都是紧凑。
    pub fn parse(raw: Option<&str>) -> Self {
        if raw == Some("comfortable") {
            Self::Comfortable
        } else {
            Self::Compact
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Compact => Self::Comfortable,
            Self::Comfortable => Self::Compact,
        }
    }
}

/// 排序方式：手动（和左边的树同一个顺序，默认）/ 最近打开 / 名称。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortMode {
    #[default]
    Manual,
    Recent,
    Name,
}

impl SortMode {
    /// 读不懂的值按手动。
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("recent") => Self::Recent,
            Some("name") => Self::Name,
            _ => Self::Manual,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Recent => "recent",
            Self::Name => "name",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "手动",
            Self::Recent => "最近打开",
            Self::Name => "名称",
        }
    }

    /// 主机页上纯图标的排序按钮的悬停提示：当前方式写在里面。
    pub fn tooltip(self) -> String {
        format!("排序：{}（点击切换：手动 / 最近打开 / 名称）", self.label())
    }

    /// 排序按钮点一下：手动 → 最近打开 → 名称 → 手动。
    pub fn next(self) -> Self {
        match self {
            Self::Manual => Self::Recent,
            Self::Recent => Self::Name,
            Self::Name => Self::Manual,
        }
    }
}

/// 设置表里存的是 JSON 文本（`"list"`），也容忍没带引号的老值。
pub fn setting_text(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::String(text)) => Some(text),
        Ok(_) => None,
        Err(_) => Some(raw.to_owned()),
    }
}

/// 写进设置表的 JSON 文本。
pub fn setting_json(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

// ---- 最近打开 ----

/// 最多记几台最近打开的主机（再早的回到按名字排）。
pub const RECENT_HOSTS_MAX: usize = 20;

/// `layout.recent_hosts` → 主机 id 列表（最近的在前）。
pub fn parse_recent(raw: &str) -> Vec<i64> {
    raw.split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .collect()
}

/// 打开了一台主机：把它挪到最前面，超出上限的丢掉。
pub fn record_recent(raw: &str, host_id: i64) -> String {
    let mut ids: Vec<i64> = parse_recent(raw)
        .into_iter()
        .filter(|id| *id != host_id)
        .collect();
    ids.insert(0, host_id);
    ids.truncate(RECENT_HOSTS_MAX);
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

fn sort_by_recent<'a>(mut hosts: Vec<&'a Host>, recent: &[i64]) -> Vec<&'a Host> {
    let rank: HashMap<i64, usize> = recent.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    hosts.sort_by(|a, b| match (rank.get(&a.id), rank.get(&b.id)) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => compare_names(&a.name, &b.name),
    });
    hosts
}

// ---- 右边卡片 / 列表要显示的主机 ----

/// 这个分组连同它所有子孙分组的 id（防环：每个分组只走一次）。
pub fn group_with_descendants(groups: &[Group], group_id: i64) -> HashSet<i64> {
    let mut ids = HashSet::new();
    let mut stack = vec![group_id];
    while let Some(id) = stack.pop() {
        if !ids.insert(id) {
            continue;
        }
        stack.extend(
            groups
                .iter()
                .filter(|g| g.parent_id == Some(id))
                .map(|g| g.id),
        );
    }
    ids
}

pub struct PageOptions<'a> {
    pub query: &'a str,
    pub selected: GroupSel,
    pub mode: SortMode,
    pub recent: &'a [i64],
    /// 「未分组」改过名（平铺）：树里没有那一行，停在它上面的当成「全部主机」。
    pub flat: bool,
}

/// 右边卡片 / 列表要显示的主机和顺序（同 `pageHosts`）：
/// - 范围：选了分组就是它和所有子分组里的主机，未分组只看顶层的；搜索框里有字再按
///   名称 / 地址 / 用户名 / 标签过滤；
/// - 手动：和左边的树完全一致；最近打开：最近的在前、其余按名字；名称：按名字。
pub fn page_hosts<'a>(
    groups: &[Group],
    hosts: &'a [Host],
    opts: &PageOptions<'_>,
) -> Vec<&'a Host> {
    let selected = if opts.flat && opts.selected == GroupSel::Ungrouped {
        GroupSel::All
    } else {
        opts.selected
    };
    let scope = match selected {
        GroupSel::Group(id) => Some(group_with_descendants(groups, id)),
        _ => None,
    };
    let matched: Vec<&Host> = hosts
        .iter()
        .filter(|host| {
            if selected == GroupSel::Ungrouped && host.group_id.is_some() {
                return false;
            }
            if let Some(scope) = &scope
                && !host.group_id.is_some_and(|id| scope.contains(&id))
            {
                return false;
            }
            host_matches(host, opts.query)
        })
        .collect();
    match opts.mode {
        SortMode::Recent => return sort_by_recent(matched, opts.recent),
        SortMode::Name => {
            let mut matched = matched;
            matched.sort_by(|a, b| compare_names(&a.name, &b.name));
            return matched;
        }
        SortMode::Manual => {}
    }
    let filter = TreeFilter {
        text: String::new(),
        host_ids: matched.iter().map(|h| h.id).collect(),
        group_ids: groups.iter().map(|g| g.id).collect(),
        ungrouped: true,
        counts: HashMap::new(),
        total: matched.len(),
    };
    let order = filtered_host_order(groups, hosts, &filter, opts.flat);
    let by_id: HashMap<i64, &Host> = matched.iter().map(|h| (h.id, *h)).collect();
    let mut shown: Vec<&Host> = Vec::with_capacity(matched.len());
    let mut seen = HashSet::new();
    for id in order {
        if let Some(host) = by_id.get(&id)
            && seen.insert(id)
        {
            shown.push(host);
        }
    }
    // 分组数据不正常（上级分组不存在 / 成环）时，树走不到那些分组：主机照样列在最后，
    // 不能凭空消失。
    let mut orphans: Vec<&Host> = matched
        .into_iter()
        .filter(|h| !seen.contains(&h.id))
        .collect();
    orphans.sort_by(|a, b| compare_order((a.sort, &a.name), (b.sort, &b.name)));
    shown.extend(orphans);
    shown
}

/// 右边现在能不能拖动 / 上移下移：只有手动排序、没在搜索时可以。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderState {
    Manual,
    /// 不是手动排序。
    Mode,
    /// 搜索中。
    Search,
}

pub fn order_state(mode: SortMode, query: &str) -> OrderState {
    if mode != SortMode::Manual {
        OrderState::Mode
    } else if !query.trim().is_empty() {
        OrderState::Search
    } else {
        OrderState::Manual
    }
}

impl OrderState {
    /// 置灰时的悬停提示。
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Self::Manual => None,
            Self::Mode => Some("切到手动排序才能调整"),
            Self::Search => Some("清空搜索后才能调整"),
        }
    }
}

// ---- 搜索框里的快速连接串 ----

/// 搜索框里的文字像不像 `user@host:port` 这类快速连接串（同 `looksLikeQuickTarget`）：
/// 没有空白，并且有 `@`，或者是 `host` / `host:port`。不认识的词仍然当搜索。
pub fn looks_like_quick_target(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    if trimmed.contains('@') {
        return true;
    }
    let (host, port) = match trimmed.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (trimmed, None),
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    host_ok && port_ok
}

// ---- 同一层的排序操作 ----

/// 这个分组（`None` = 未分组 / 平铺的根层）里主机现在的显示顺序。
pub fn host_layer_ids(hosts: &[Host], group_id: Option<i64>) -> Vec<i64> {
    hosts_under(hosts, group_id, None)
        .iter()
        .map(|h| h.id)
        .collect()
}

/// 这个父分组（`None` = 根）下分组现在的显示顺序。
pub fn group_layer_ids(groups: &[Group], parent_id: Option<i64>) -> Vec<i64> {
    groups_under(groups, parent_id, None)
        .iter()
        .map(|g| g.id)
        .collect()
}

/// 把 `moved` 从列表里拿出来，插到 `target` 前面（`after` 为真则后面）。自己、目标不在列表里返回 `None`。
pub fn insert_in_layer(ids: &[i64], moved: i64, target: i64, after: bool) -> Option<Vec<i64>> {
    if moved == target {
        return None;
    }
    let mut rest: Vec<i64> = ids.iter().copied().filter(|id| *id != moved).collect();
    let at = rest.iter().position(|id| *id == target)?;
    rest.insert(if after { at + 1 } else { at }, moved);
    Some(rest)
}

/// 把 `id` 往前（-1）或往后（+1）挪一格；已经在头 / 尾、或不在列表里返回 `None`。
pub fn step_in_layer(ids: &[i64], id: i64, delta: i32) -> Option<Vec<i64>> {
    let at = ids.iter().position(|candidate| *candidate == id)?;
    let to = at as i64 + i64::from(delta);
    if to < 0 || to >= ids.len() as i64 {
        return None;
    }
    let mut next = ids.to_vec();
    next.swap(at, to as usize);
    Some(next)
}

/// 上移 / 下移还能不能动：`(能上移, 能下移)`。
pub fn can_step_host(hosts: &[Host], host: &Host) -> (bool, bool) {
    let ids = host_layer_ids(hosts, host.group_id);
    (
        step_in_layer(&ids, host.id, -1).is_some(),
        step_in_layer(&ids, host.id, 1).is_some(),
    )
}

pub fn can_step_group(groups: &[Group], group: &Group) -> (bool, bool) {
    let ids = group_layer_ids(groups, group.parent_id);
    (
        step_in_layer(&ids, group.id, -1).is_some(),
        step_in_layer(&ids, group.id, 1).is_some(),
    )
}

/// 松手后要做的事：先（可能）换分组，再给目标那一层整层重新编号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostDrop {
    /// `Some(分组)` = 要先把主机移到那个分组（`Some(None)` = 顶层 / 未分组）。
    pub move_to: Option<Option<i64>>,
    /// 目标那一层的新顺序。
    pub order: Vec<i64>,
}

/// 把 `host_id` 放到 `target_id` 的前面 / 后面（同 `dropHostAt`）。目标在别的分组里就同时换过去。
/// 位置没变、自己放到自己上、找不到主机都返回 `None`。
pub fn plan_host_drop(
    hosts: &[Host],
    host_id: i64,
    target_id: i64,
    after: bool,
) -> Option<HostDrop> {
    let host = hosts.iter().find(|h| h.id == host_id)?;
    let target = hosts.iter().find(|h| h.id == target_id)?;
    let layer = host_layer_ids(hosts, target.group_id);
    let order = insert_in_layer(&layer, host_id, target_id, after)?;
    if host.group_id == target.group_id {
        if order == layer {
            return None;
        }
        return Some(HostDrop {
            move_to: None,
            order,
        });
    }
    Some(HostDrop {
        move_to: Some(target.group_id),
        order,
    })
}

/// 把分组放到同一个父分组下 `target_id` 那个分组的前面 / 后面（同 `dropGroupAt`）。
/// 不同层、位置没变都返回 `None`。
pub fn plan_group_drop(
    groups: &[Group],
    group_id: i64,
    target_id: i64,
    after: bool,
) -> Option<Vec<i64>> {
    let group = groups.iter().find(|g| g.id == group_id)?;
    let target = groups.iter().find(|g| g.id == target_id)?;
    if target.parent_id != group.parent_id {
        return None;
    }
    let layer = group_layer_ids(groups, group.parent_id);
    let order = insert_in_layer(&layer, group_id, target_id, after)?;
    (order != layer).then_some(order)
}

// ---- 分组 ----

/// 分组按树的顺序摊平（父组在前，子组跟在后面），带层级用来缩进。
pub fn flatten_groups(groups: &[Group]) -> Vec<(&Group, usize)> {
    fn walk<'a>(
        groups: &'a [Group],
        parent: Option<i64>,
        depth: usize,
        out: &mut Vec<(&'a Group, usize)>,
        seen: &mut HashSet<i64>,
    ) {
        for group in groups_under(groups, parent, None) {
            if !seen.insert(group.id) {
                continue;
            }
            out.push((group, depth));
            walk(groups, Some(group.id), depth + 1, out, seen);
        }
    }
    let mut out = Vec::new();
    walk(groups, None, 0, &mut out, &mut HashSet::new());
    out
}

/// 编辑分组时不能把自己或自己的子孙当父分组。
pub fn forbidden_parents(groups: &[Group], group_id: i64) -> HashSet<i64> {
    group_with_descendants(groups, group_id)
}

// ---- 文案（「未分组」改名成「顶层」之后叫法跟着变） ----

/// 提示 / 列表里「没有分组」的叫法。
pub fn ungrouped_label(flat: bool) -> &'static str {
    if flat { "顶层" } else { "未分组" }
}

/// 主机详情里分组下拉的第一项。
pub fn ungrouped_option_label(flat: bool) -> &'static str {
    if flat { "（顶层）" } else { "未分组" }
}

/// 右键「移动到分组」子菜单的第一项。
pub fn ungrouped_move_label(flat: bool) -> &'static str {
    if flat {
        "顶层（不分组）"
    } else {
        "未分组"
    }
}

/// 删除分组的确认文案：分组下的主机去哪儿。
pub fn delete_group_message(flat: bool) -> &'static str {
    if flat {
        "分组下的主机会移到顶层，不会被删。"
    } else {
        "分组下的主机会变成未分组，不会被删。"
    }
}

// ---- 主机属性面板 ----

/// 属性面板里的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoRow {
    pub key: &'static str,
    pub label: &'static str,
    pub value: String,
}

/// 单击主机在树底部展开的只读属性：名称、地址、用户名、协议、端口，有跳板机 / 指定代理时再加一行；
/// 不显示任何凭据，跟随默认代理的看不出具体是谁，也不显示（同 `hostInfoRows`）。
pub fn host_info_rows(host: &Host, hosts: &[Host], proxies: &[Proxy]) -> Vec<InfoRow> {
    let protocol = InfoRow {
        key: "protocol",
        label: "协议",
        value: super::protocol_form::protocol_tag(host.protocol.protocol).to_owned(),
    };
    // 串口没有地址、端口、用户名、代理、跳板机：显示设备和波特率。
    if host.protocol.protocol == Protocol::Serial {
        let options = host.protocol.serial.clone().unwrap_or_default();
        return vec![
            InfoRow {
                key: "name",
                label: "名称",
                value: host.name.clone(),
            },
            protocol,
            InfoRow {
                key: "device",
                label: "设备",
                value: options.device,
            },
            InfoRow {
                key: "baud",
                label: "波特率",
                value: options.baud_rate.to_string(),
            },
        ];
    }
    let mut rows = vec![
        InfoRow {
            key: "name",
            label: "名称",
            value: host.name.clone(),
        },
        InfoRow {
            key: "address",
            label: "主机",
            value: normalize_address(&host.host),
        },
        InfoRow {
            key: "user",
            label: "用户名",
            value: host.username.clone(),
        },
        protocol,
        InfoRow {
            key: "port",
            label: "端口",
            value: host.port.to_string(),
        },
    ];
    if let Some(jump) = host
        .jump_host_id
        .and_then(|id| hosts.iter().find(|h| h.id == id))
    {
        rows.push(InfoRow {
            key: "jump",
            label: "跳板机",
            value: jump.name.clone(),
        });
    }
    if host.proxy_mode == ProxyMode::Custom
        && let Some(proxy) = host
            .proxy_id
            .and_then(|id| proxies.iter().find(|p| p.id == id))
    {
        rows.push(InfoRow {
            key: "proxy",
            label: "代理",
            value: proxy.name.clone(),
        });
    }
    rows
}

/// 「复制密码」要不要显示：主机是密码登录、引用了密码类凭据、且凭据里存了密码。
pub fn can_copy_password(host: &Host, identities: &[Identity]) -> bool {
    if host.auth_method != AuthMethod::Password {
        return false;
    }
    let Some(id) = host.identity_id else {
        return false;
    };
    identities
        .iter()
        .find(|identity| identity.id == id)
        .is_some_and(|identity| identity.kind == IdentityKind::Password && identity.has_secret)
}

/// 快速连接串 `user@host[:port]` / `user@[::1]:2222` 里的地址部分，给标签右键「复制地址」用。
pub fn quick_target_address(target: &str) -> Option<String> {
    let target = target.trim();
    let at = target.find('@').filter(|at| *at > 0)?;
    let rest = &target[at + 1..];
    if let Some(inner) = rest.strip_prefix('[') {
        let close = inner.find(']')?;
        let address = inner[..close].trim();
        return (!address.is_empty()).then(|| address.to_owned());
    }
    let host = rest.split(':').next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::model::{AuthMethod, ProxyKind};

    fn host(id: i64, group: Option<i64>, name: &str, sort: i64) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: group,
            name: name.to_owned(),
            host: format!("192.0.2.{id}"),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn group(id: i64, parent: Option<i64>, name: &str, sort: i64) -> Group {
        Group {
            id,
            name: name.to_owned(),
            parent_id: parent,
            sort,
        }
    }

    fn ids(hosts: &[&Host]) -> Vec<i64> {
        hosts.iter().map(|h| h.id).collect()
    }

    fn opts<'a>(
        selected: GroupSel,
        mode: SortMode,
        query: &'a str,
        recent: &'a [i64],
    ) -> PageOptions<'a> {
        PageOptions {
            query,
            selected,
            mode,
            recent,
            flat: false,
        }
    }

    /// prod(1) ⊃ db(2)；dev(3)；还有一台没分组的。
    fn sample() -> (Vec<Group>, Vec<Host>) {
        (
            vec![
                group(1, None, "prod", 0),
                group(2, Some(1), "db", 0),
                group(3, None, "dev", 0),
            ],
            vec![
                host(1, Some(1), "web", 0),
                host(2, Some(2), "pg", 0),
                host(3, Some(3), "dev-box", 0),
                host(4, None, "loose", 0),
            ],
        )
    }

    #[test]
    fn settings_parse_leniently_and_round_trip() {
        assert_eq!(ViewMode::parse(Some("list")), ViewMode::List);
        assert_eq!(ViewMode::parse(Some("grid")), ViewMode::Grid);
        assert_eq!(ViewMode::parse(None), ViewMode::List);
        assert_eq!(Density::parse(Some("comfortable")), Density::Comfortable);
        assert_eq!(Density::parse(Some("whatever")), Density::Compact);
        assert_eq!(Density::parse(None), Density::Compact, "没存过默认紧凑");
        assert_eq!(SortMode::parse(Some("name")), SortMode::Name);
        assert_eq!(SortMode::parse(Some("bogus")), SortMode::Manual);
        // 纯图标的排序按钮：当前方式写在悬停提示里。
        for mode in [SortMode::Manual, SortMode::Recent, SortMode::Name] {
            assert!(
                mode.tooltip()
                    .starts_with(&format!("排序：{}", mode.label()))
            );
        }
        assert_eq!(setting_text(Some("\"list\"")).as_deref(), Some("list"));
        assert_eq!(
            setting_text(Some("list")).as_deref(),
            Some("list"),
            "老的没引号"
        );
        assert_eq!(setting_text(Some("42")), None);
        assert_eq!(setting_text(None), None);
        assert_eq!(setting_json("list"), "\"list\"");
    }

    #[test]
    fn the_sort_button_cycles_manual_recent_name_manual() {
        let mut mode = SortMode::Manual;
        let seen: Vec<_> = (0..4)
            .map(|_| {
                mode = mode.next();
                mode
            })
            .collect();
        assert_eq!(
            seen,
            [
                SortMode::Recent,
                SortMode::Name,
                SortMode::Manual,
                SortMode::Recent
            ]
        );
        assert_eq!(SortMode::Recent.label(), "最近打开");
        assert_eq!(Density::Compact.toggled(), Density::Comfortable);
    }

    #[test]
    fn recent_hosts_move_to_the_front_without_duplicates_and_cap_at_twenty() {
        assert_eq!(parse_recent("3, 1,x,-2,0,5"), vec![3, 1, 5]);
        assert_eq!(record_recent("3,1,2", 2), "2,3,1");
        assert_eq!(record_recent("", 7), "7");
        let long: String = (1..=25)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let recorded = parse_recent(&record_recent(&long, 99));
        assert_eq!(recorded.len(), RECENT_HOSTS_MAX);
        assert_eq!(recorded[0], 99);
    }

    #[test]
    fn manual_mode_follows_the_tree_subgroups_first_then_the_loose_hosts_last() {
        let (groups, hosts) = sample();
        let shown = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::All, SortMode::Manual, "", &[]),
        );
        // 根分组按名字 dev、prod；prod 里子分组 db 先于本组主机 web；未分组在最后。
        assert_eq!(ids(&shown), vec![3, 2, 1, 4]);
    }

    #[test]
    fn manual_order_changes_show_up_and_a_selected_group_scopes_with_its_subgroups() {
        let (groups, mut hosts) = sample();
        hosts[0].sort = 2;
        hosts.push(host(5, Some(1), "api", 1));
        let shown = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::Group(1), SortMode::Manual, "", &[]),
        );
        assert_eq!(
            ids(&shown),
            vec![2, 5, 1],
            "子分组 db 先，本组里 sort 小的 api 在 web 前"
        );
        let db = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::Group(2), SortMode::Manual, "", &[]),
        );
        assert_eq!(ids(&db), vec![2]);
        let loose = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::Ungrouped, SortMode::Manual, "", &[]),
        );
        assert_eq!(ids(&loose), vec![4]);
    }

    #[test]
    fn recent_puts_the_latest_first_and_name_ignores_manual_order() {
        let (groups, mut hosts) = sample();
        hosts[0].sort = 1;
        let recent = [4, 1];
        let shown = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::All, SortMode::Recent, "", &recent),
        );
        // 最近的 4、1 在前；其余 dev-box、pg 按名字。
        assert_eq!(ids(&shown), vec![4, 1, 3, 2]);
        let by_name = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::All, SortMode::Name, "", &recent),
        );
        assert_eq!(ids(&by_name), vec![3, 4, 2, 1], "dev-box loose pg web");
        let stale = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::All, SortMode::Recent, "", &[99, 3]),
        );
        assert_eq!(ids(&stale)[0], 3, "记录里已删掉的主机不碍事");
    }

    #[test]
    fn search_filters_first_then_orders_and_needs_both_conditions_with_a_group() {
        let (groups, mut hosts) = sample();
        hosts[3].tags = vec!["blue".to_owned()];
        let by_tag = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::All, SortMode::Manual, "BLUE", &[]),
        );
        assert_eq!(ids(&by_tag), vec![4]);
        let scoped = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::Group(1), SortMode::Name, "pg", &[]),
        );
        assert_eq!(
            ids(&scoped),
            vec![2],
            "pg 在 prod 的子分组里；web 不匹配 pg"
        );
        let none = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::Group(3), SortMode::Manual, "web", &[]),
        );
        assert!(none.is_empty());
    }

    #[test]
    fn a_group_cycle_or_missing_parent_never_hides_or_loops() {
        let groups = vec![group(1, Some(2), "a", 0), group(2, Some(1), "b", 0)];
        let hosts = vec![
            host(1, Some(1), "x", 0),
            host(2, None, "y", 0),
            host(3, Some(9), "z", 0),
        ];
        let shown = page_hosts(
            &groups,
            &hosts,
            &opts(GroupSel::All, SortMode::Manual, "", &[]),
        );
        let mut seen = ids(&shown);
        seen.sort();
        assert_eq!(seen, vec![1, 2, 3], "树走不到的主机照样列出来");
        assert!(page_hosts(&[], &[], &opts(GroupSel::All, SortMode::Manual, "", &[])).is_empty());
    }

    #[test]
    fn a_flat_tree_has_no_ungrouped_row_so_it_counts_as_all() {
        let (groups, hosts) = sample();
        let mut options = opts(GroupSel::Ungrouped, SortMode::Name, "", &[]);
        options.flat = true;
        assert_eq!(page_hosts(&groups, &hosts, &options).len(), 4);
    }

    #[test]
    fn dragging_and_menus_are_only_allowed_in_manual_mode_without_a_search() {
        assert_eq!(order_state(SortMode::Manual, ""), OrderState::Manual);
        assert_eq!(order_state(SortMode::Manual, "  "), OrderState::Manual);
        assert_eq!(order_state(SortMode::Manual, "web"), OrderState::Search);
        assert_eq!(
            order_state(SortMode::Name, "web"),
            OrderState::Mode,
            "模式优先"
        );
        assert_eq!(OrderState::Mode.hint(), Some("切到手动排序才能调整"));
        assert_eq!(OrderState::Search.hint(), Some("清空搜索后才能调整"));
        assert_eq!(OrderState::Manual.hint(), None);
    }

    #[test]
    fn quick_targets_are_told_apart_from_plain_searches() {
        for yes in [
            "root@192.0.2.1",
            "ops@host:2222",
            "192.0.2.5",
            "web-1.example:22",
            "u@[::1]:22",
        ] {
            assert!(looks_like_quick_target(yes), "{yes}");
        }
        for no in ["", "  ", "two words", "web 1", "host:", "host:abc", "中文"] {
            assert!(!looks_like_quick_target(no), "{no}");
        }
        assert_eq!(
            quick_target_address("root@192.0.2.1:22").as_deref(),
            Some("192.0.2.1")
        );
        assert_eq!(
            quick_target_address("root@[2001:db8::1]:22").as_deref(),
            Some("2001:db8::1")
        );
        assert_eq!(quick_target_address("nohost"), None);
        assert_eq!(quick_target_address("@x"), None);
    }

    #[test]
    fn layer_inserts_place_before_or_after_and_refuse_nonsense() {
        let ids = [1, 2, 3, 4];
        assert_eq!(insert_in_layer(&ids, 4, 2, false), Some(vec![1, 4, 2, 3]));
        assert_eq!(insert_in_layer(&ids, 1, 3, true), Some(vec![2, 3, 1, 4]));
        assert_eq!(
            insert_in_layer(&ids, 9, 3, true),
            Some(vec![1, 2, 3, 9, 4]),
            "不在这一层的先进来"
        );
        assert_eq!(insert_in_layer(&ids, 2, 2, true), None);
        assert_eq!(insert_in_layer(&ids, 1, 9, true), None);
        assert_eq!(step_in_layer(&ids, 2, -1), Some(vec![2, 1, 3, 4]));
        assert_eq!(step_in_layer(&ids, 4, 1), None, "尾上不能再下移");
        assert_eq!(step_in_layer(&ids, 1, -1), None, "头上不能再上移");
        assert_eq!(step_in_layer(&ids, 9, 1), None);
    }

    #[test]
    fn step_buttons_are_greyed_at_the_ends_and_only_compare_siblings() {
        let (groups, hosts) = sample();
        // 根层分组 dev、prod：dev 不能上移，prod 不能下移；子分组 db 独占一层，两头都不能动。
        assert_eq!(can_step_group(&groups, &groups[2]), (false, true));
        assert_eq!(can_step_group(&groups, &groups[0]), (true, false));
        assert_eq!(can_step_group(&groups, &groups[1]), (false, false));
        // 未分组的主机在自己那一层。
        assert_eq!(can_step_host(&hosts, &hosts[3]), (false, false));
        let mut more = hosts.clone();
        more.push(host(6, None, "zeta", 0));
        assert_eq!(can_step_host(&more, &more[3]), (false, true));
        assert_eq!(can_step_host(&more, &more[4]), (true, false));
    }

    #[test]
    fn dropping_inside_one_group_renumbers_the_whole_layer() {
        let hosts = vec![
            host(1, Some(1), "a", 0),
            host(2, Some(1), "b", 0),
            host(3, Some(1), "c", 0),
        ];
        // 把 c 拖到 a 的前半：c a b。
        let plan = plan_host_drop(&hosts, 3, 1, false).expect("要调整");
        assert_eq!(plan.move_to, None);
        assert_eq!(plan.order, vec![3, 1, 2]);
        // 后半：a 之后 → a c b。
        assert_eq!(
            plan_host_drop(&hosts, 3, 1, true).expect("要调整").order,
            vec![1, 3, 2]
        );
        // 位置没变、拖到自己身上：什么都不做。
        assert_eq!(
            plan_host_drop(&hosts, 1, 2, false),
            None,
            "a 本来就在 b 前面"
        );
        assert_eq!(plan_host_drop(&hosts, 2, 2, true), None);
        assert_eq!(plan_host_drop(&hosts, 9, 1, true), None);
    }

    #[test]
    fn dropping_next_to_a_host_of_another_group_moves_it_there_first() {
        let hosts = vec![
            host(1, Some(1), "a", 0),
            host(2, Some(2), "b", 0),
            host(3, Some(2), "c", 0),
            host(4, None, "top", 0),
        ];
        let plan = plan_host_drop(&hosts, 1, 3, false).expect("要换分组");
        assert_eq!(plan.move_to, Some(Some(2)));
        assert_eq!(plan.order, vec![2, 1, 3], "b 1 c");
        // 拖到顶层主机旁边：移到顶层并插在那个位置。
        let top = plan_host_drop(&hosts, 1, 4, true).expect("到顶层");
        assert_eq!(top.move_to, Some(None));
        assert_eq!(top.order, vec![4, 1]);
    }

    #[test]
    fn groups_only_reorder_among_siblings() {
        let groups = vec![
            group(1, None, "a", 0),
            group(2, None, "b", 0),
            group(3, Some(1), "child", 0),
        ];
        assert_eq!(plan_group_drop(&groups, 2, 1, false), Some(vec![2, 1]));
        assert_eq!(plan_group_drop(&groups, 1, 2, false), None, "位置没变");
        assert_eq!(
            plan_group_drop(&groups, 3, 2, false),
            None,
            "子分组拖不进别的层"
        );
        assert_eq!(plan_group_drop(&groups, 1, 1, true), None);
    }

    #[test]
    fn groups_flatten_parents_before_children_with_depth() {
        let groups = vec![
            group(1, None, "b", 0),
            group(2, Some(1), "child", 0),
            group(3, None, "a", 0),
            group(4, Some(2), "grand", 0),
        ];
        let flat: Vec<(i64, usize)> = flatten_groups(&groups)
            .iter()
            .map(|(g, d)| (g.id, *d))
            .collect();
        assert_eq!(flat, vec![(3, 0), (1, 0), (2, 1), (4, 2)]);
        let bad = vec![group(1, Some(2), "a", 0), group(2, Some(1), "b", 0)];
        assert!(
            flatten_groups(&bad).is_empty(),
            "成环的走不到，但不会死循环"
        );
        let forbidden = forbidden_parents(&groups, 1);
        assert!(forbidden.contains(&1) && forbidden.contains(&2) && forbidden.contains(&4));
        assert!(!forbidden.contains(&3));
    }

    #[test]
    fn labels_follow_the_flat_switch() {
        assert_eq!(ungrouped_label(false), "未分组");
        assert_eq!(ungrouped_label(true), "顶层");
        assert_eq!(ungrouped_option_label(true), "（顶层）");
        assert_eq!(ungrouped_move_label(true), "顶层（不分组）");
        assert!(delete_group_message(true).contains("顶层"));
        assert!(delete_group_message(false).contains("未分组"));
    }

    #[test]
    fn the_info_panel_shows_jump_and_custom_proxy_but_never_a_credential() {
        let mut target = host(1, None, "web", 0);
        target.host = "[2001:db8::1]".to_owned();
        let jump = host(2, None, "bastion", 0);
        let proxies = vec![Proxy {
            id: 7,
            name: "office".to_owned(),
            kind: ProxyKind::Socks5,
            host: "198.51.100.1".to_owned(),
            port: 1080,
            username: None,
            has_secret: true,
        }];
        let plain = host_info_rows(&target, &[target.clone(), jump.clone()], &proxies);
        let keys: Vec<_> = plain.iter().map(|r| r.key).collect();
        assert_eq!(keys, ["name", "address", "user", "protocol", "port"]);
        assert_eq!(plain[1].value, "2001:db8::1", "方括号去掉");
        target.jump_host_id = Some(2);
        target.proxy_mode = ProxyMode::Custom;
        target.proxy_id = Some(7);
        let full = host_info_rows(&target, &[target.clone(), jump], &proxies);
        let labels: Vec<_> = full.iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            ["名称", "主机", "用户名", "协议", "端口", "跳板机", "代理"]
        );
        target.proxy_mode = ProxyMode::Default;
        assert_eq!(
            host_info_rows(&target, &[target.clone()], &proxies).len(),
            5,
            "跟随默认代理不显示"
        );
    }

    #[test]
    fn copy_password_needs_a_password_host_with_a_stored_password_identity() {
        let mut target = host(1, None, "web", 0);
        let identity = |kind, has_secret| Identity {
            id: 5,
            name: "i".to_owned(),
            kind,
            key_path: None,
            has_secret,
        };
        assert!(!can_copy_password(&target, &[]), "没选凭据");
        target.identity_id = Some(5);
        assert!(can_copy_password(
            &target,
            &[identity(IdentityKind::Password, true)]
        ));
        assert!(!can_copy_password(
            &target,
            &[identity(IdentityKind::Password, false)]
        ));
        assert!(!can_copy_password(
            &target,
            &[identity(IdentityKind::KeyInline, true)]
        ));
        target.auth_method = AuthMethod::Key;
        assert!(!can_copy_password(
            &target,
            &[identity(IdentityKind::Password, true)]
        ));
    }
}
