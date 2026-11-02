//! 主窗口读主机库的那一层：主机、分组、「平铺」开关，以及从库里重读。
//!
//! 数据来自 `zssh_core::store::Database`，和原生主机库窗口是同一个库文件。主窗口只读：
//! 新建、编辑、删除先交给现有主机库窗口（G07b 再收进主窗口），改完这里隔几秒重读一次就能看到。

//! 主窗口读写主机库的那一层：主机、分组、凭据、代理、转发规则、「平铺」开关和主机页的显示选项，
//! 以及从库里重读。
//!
//! 数据来自 `zssh_core::store::Database`，和各个调试窗口是同一个库文件。主窗口自己的写操作
//! （保存 / 删除 / 移动 / 排序）都在这里，每次写完立刻重读，界面马上就能看到；别的窗口改了
//! 东西，节拍里隔几秒重读也能看到。

use zssh_core::model::{Group, GroupDraft, Host, HostDraft, Identity, Proxy};
use zssh_core::store::Database;
use zssh_core::store::forwards::PortForward;

use super::bar_settings::BarSettings;
use super::host_tree::effective_flat;
use super::page_logic::{
    Density, HostDrop, SortMode, ViewMode, parse_recent, record_recent, setting_json, setting_text,
};

/// 同步键：用户明确选过「平铺」。
const FLAT_FLAG_KEY: &str = "vault_ungrouped_flat";
/// 本机默认（`flat` / `classic`）；用户没选过时看它。
const FLAT_DEFAULT_KEY: &str = "layout.vault_flat_default";
pub const VIEW_MODE_KEY: &str = "layout.vault_view_mode";
pub const DENSITY_KEY: &str = "layout.vault_density";
pub const SORT_MODE_KEY: &str = "layout.vault_sort_mode";
pub const RECENT_KEY: &str = "layout.recent_hosts";

/// 主机页右边卡片 / 列表的显示选项（本机布局，不同步）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VaultLayout {
    pub view_mode: ViewMode,
    pub density: Density,
    pub sort_mode: SortMode,
    /// 最近打开的主机，最近的在前。
    pub recent: Vec<i64>,
}

/// 把库里的主机变成保存草稿（原样带过去，改的字段由调用方覆盖）。
pub fn draft_of_host(host: &Host) -> HostDraft {
    HostDraft {
        id: Some(host.id),
        group_id: host.group_id,
        name: host.name.clone(),
        host: host.host.clone(),
        port: host.port,
        username: host.username.clone(),
        auth_method: host.auth_method,
        identity_id: host.identity_id,
        proxy_id: host.proxy_id,
        jump_host_id: host.jump_host_id,
        proxy_mode: host.proxy_mode,
        tags: host.tags.clone(),
        sort: host.sort,
        legacy_algorithms: host.legacy_algorithms,
    }
}

pub struct HostStore {
    db: Option<Database>,
    hosts: Vec<Host>,
    groups: Vec<Group>,
    identities: Vec<Identity>,
    proxies: Vec<Proxy>,
    forwards: Vec<PortForward>,
    layout: VaultLayout,
    bars: BarSettings,
    flat: bool,
    /// 打不开库或读失败的原因（界面原样显示，不崩）。
    error: Option<String>,
}

impl HostStore {
    /// 打开数据目录里的库并读一遍。打不开不是致命的：显示原因，主机列表是空的。
    pub fn open() -> Self {
        let mut store = match crate::hosts_demo::open_database() {
            Ok(db) => Self::with_db(Some(db), None),
            Err(err) => Self::with_db(None, Some(format!("打开主机库失败：{err}"))),
        };
        store.reload();
        store
    }

    fn with_db(db: Option<Database>, error: Option<String>) -> Self {
        Self {
            db,
            hosts: Vec::new(),
            groups: Vec::new(),
            identities: Vec::new(),
            proxies: Vec::new(),
            forwards: Vec::new(),
            layout: VaultLayout::default(),
            bars: BarSettings::default(),
            flat: false,
            error,
        }
    }

    /// 没有数据库的空仓库（测试用）。
    #[cfg(test)]
    pub fn from_parts(hosts: Vec<Host>, groups: Vec<Group>, flat: bool) -> Self {
        let mut store = Self::with_db(None, None);
        store.hosts = hosts;
        store.groups = groups;
        store.flat = flat;
        store
    }

    /// 内存库（测试用）：能真的读写。
    #[cfg(test)]
    pub fn in_memory() -> Self {
        let db = Database::open_in_memory().expect("内存库");
        let mut store = Self::with_db(Some(db), None);
        store.reload();
        store
    }

    /// 从库里重读；返回内容有没有变（界面据此决定要不要重绘）。读不出来时保留旧内容。
    pub fn reload(&mut self) -> bool {
        let Some(db) = &self.db else {
            return false;
        };
        let mut error = None;
        fn read<T>(
            error: &mut Option<String>,
            what: &str,
            result: Result<T, zssh_core::error::ZsshError>,
        ) -> Option<T> {
            match result {
                Ok(value) => Some(value),
                Err(err) => {
                    *error = Some(format!("读取{what}失败：{err}"));
                    None
                }
            }
        }
        let hosts = read(&mut error, "主机", db.host_list());
        let groups = read(&mut error, "分组", db.group_list());
        let identities = read(&mut error, "凭据", db.identity_list());
        let proxies = read(&mut error, "代理", db.proxy_list());
        let forwards = read(&mut error, "端口转发", db.forward_list());
        let flag = db.setting_get(FLAT_FLAG_KEY).ok().flatten();
        let local = db.setting_get(FLAT_DEFAULT_KEY).ok().flatten();
        let flat = effective_flat(flag.as_deref(), local.as_deref());
        let layout = VaultLayout {
            view_mode: ViewMode::parse(
                setting_text(db.setting_get(VIEW_MODE_KEY).ok().flatten().as_deref()).as_deref(),
            ),
            density: Density::parse(
                setting_text(db.setting_get(DENSITY_KEY).ok().flatten().as_deref()).as_deref(),
            ),
            sort_mode: SortMode::parse(
                setting_text(db.setting_get(SORT_MODE_KEY).ok().flatten().as_deref()).as_deref(),
            ),
            recent: parse_recent(
                &setting_text(db.setting_get(RECENT_KEY).ok().flatten().as_deref())
                    .unwrap_or_default(),
            ),
        };

        let mut changed = self.error != error;
        self.error = error;
        fn replace<T: PartialEq>(slot: &mut T, value: Option<T>, changed: &mut bool) {
            if let Some(value) = value
                && *slot != value
            {
                *slot = value;
                *changed = true;
            }
        }
        replace(&mut self.hosts, hosts, &mut changed);
        replace(&mut self.groups, groups, &mut changed);
        replace(&mut self.identities, identities, &mut changed);
        replace(&mut self.proxies, proxies, &mut changed);
        replace(&mut self.forwards, forwards, &mut changed);
        replace(&mut self.layout, Some(layout), &mut changed);
        replace(&mut self.bars, Some(BarSettings::load(db)), &mut changed);
        replace(&mut self.flat, Some(flat), &mut changed);
        changed
    }

    // ---- 写操作：每一步写完立刻重读 ----

    /// 打开的库；打不开时给界面一句能直接显示的话。
    pub fn db(&self) -> Result<&Database, String> {
        self.db.as_ref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| "主机库没有打开".to_owned())
        })
    }

    fn after_write<T>(
        &mut self,
        result: Result<T, zssh_core::error::ZsshError>,
    ) -> Result<T, String> {
        // 写失败也重读：库里的真实状态才是界面该显示的。
        self.reload();
        result.map_err(|err| err.to_string())
    }

    pub fn save_host(&mut self, draft: &HostDraft) -> Result<Host, String> {
        let result = self.db()?.host_save(draft);
        self.after_write(result)
    }

    /// 读库里这台主机现在的样子（保存前合并未展示的字段用）。
    pub fn host_fresh(&self, id: i64) -> Result<Host, String> {
        self.db()?.host_get(id).map_err(|err| err.to_string())
    }

    /// 删主机，并一起删它独占的密码（密钥一律不动，被别的主机用着的密码保留）；见 `host_delete`。
    pub fn delete_host_with_passwords(
        &mut self,
        id: i64,
    ) -> Result<super::host_delete::Deleted, String> {
        let result = self
            .db()
            .and_then(|db| super::host_delete::delete_host_with_passwords(db, id));
        // 写失败也重读：库里的真实状态才是界面该显示的。
        self.reload();
        result
    }

    /// 移到某个分组（右键菜单和拖放共用）；已经在那里返回 `false`。换了分组后顺序由库置 0。
    pub fn move_host_to_group(&mut self, host_id: i64, group: Option<i64>) -> Result<bool, String> {
        let Some(host) = self.host(host_id).cloned() else {
            return Err("主机已经不在库里了".to_owned());
        };
        if host.group_id == group {
            return Ok(false);
        }
        let mut draft = draft_of_host(&host);
        draft.group_id = group;
        self.save_host(&draft).map(|_| true)
    }

    /// 一批主机一起移（「未分组」改名建成分组时把现有未分组主机收进去）。
    pub fn move_hosts(&mut self, ids: &[i64], group: Option<i64>) -> Result<(), String> {
        let mut outcome = Ok(());
        for id in ids {
            if let Err(err) = self.move_host_to_group(*id, group) {
                outcome = Err(err);
                break;
            }
        }
        self.reload();
        outcome
    }

    /// 松手：先（可能）换分组，再给目标那一层整层重新编号。
    pub fn apply_host_drop(&mut self, host_id: i64, drop: &HostDrop) -> Result<(), String> {
        if let Some(group) = drop.move_to {
            self.move_host_to_group(host_id, group)?;
        }
        self.reorder_hosts(&drop.order)
    }

    pub fn reorder_hosts(&mut self, ids: &[i64]) -> Result<(), String> {
        let result = self.db()?.host_reorder(ids);
        self.after_write(result)
    }

    pub fn reorder_groups(&mut self, ids: &[i64]) -> Result<(), String> {
        let result = self.db()?.group_reorder(ids);
        self.after_write(result)
    }

    /// 「按名字重新排序」：这个分组下直接的主机和子分组回到按名字；`None` = 只管未分组的主机。
    pub fn reset_order(&mut self, group: Option<i64>) -> Result<(), String> {
        let result = self.db()?.order_reset(group);
        self.after_write(result)
    }

    pub fn save_group(&mut self, draft: &GroupDraft) -> Result<Group, String> {
        let result = self.db()?.group_save(draft);
        self.after_write(result)
    }

    pub fn delete_group(&mut self, id: i64) -> Result<(), String> {
        let result = self.db()?.group_delete(id);
        self.after_write(result)
    }

    /// 写一条本机布局设置（值按 JSON 字符串存，和 0.2.14 一致）。
    pub fn set_layout_text(&mut self, key: &str, value: &str) -> Result<(), String> {
        let result = self.db()?.setting_set(key, &setting_json(value));
        self.after_write(result)
    }

    pub fn set_view_mode(&mut self, mode: ViewMode) -> Result<(), String> {
        self.set_layout_text(VIEW_MODE_KEY, mode.as_str())
    }

    pub fn set_density(&mut self, density: Density) -> Result<(), String> {
        self.set_layout_text(DENSITY_KEY, density.as_str())
    }

    pub fn set_sort_mode(&mut self, mode: SortMode) -> Result<(), String> {
        self.set_layout_text(SORT_MODE_KEY, mode.as_str())
    }

    /// 打开了一台主机：记进「最近打开」。写失败只写日志，不打断连接。
    pub fn record_recent(&mut self, host_id: i64) {
        let current = self
            .db
            .as_ref()
            .and_then(|db| db.setting_get(RECENT_KEY).ok().flatten());
        let raw = setting_text(current.as_deref()).unwrap_or_default();
        if let Err(err) = self.set_layout_text(RECENT_KEY, &record_recent(&raw, host_id)) {
            tracing::warn!(%err, "记录最近打开的主机失败");
        }
    }

    /// 「未分组」建成分组之后：树里不再有「未分组」行，不分组的主机直接排在根层。
    /// 明确写同步键 `vault_ungrouped_flat`（用户的选择，不是本机默认）。
    pub fn set_flat(&mut self, flat: bool) -> Result<(), String> {
        let value = if flat { "true" } else { "false" };
        let result = self.db()?.setting_set(FLAT_FLAG_KEY, value);
        self.after_write(result)
    }

    pub fn identities(&self) -> &[Identity] {
        &self.identities
    }

    pub fn proxies(&self) -> &[Proxy] {
        &self.proxies
    }

    pub fn forwards(&self) -> &[PortForward] {
        &self.forwards
    }

    pub fn layout(&self) -> &VaultLayout {
        &self.layout
    }

    /// 命令栏、撰写栏、AI / 文件面板的设置（见 `bar_settings`）。
    pub fn bars(&self) -> &BarSettings {
        &self.bars
    }

    /// 写一条栏 / 面板设置，写完立刻重读（写失败也重读：库里的真实状态才是界面该显示的）。
    pub fn write_bar_setting(
        &mut self,
        write: impl FnOnce(&Database) -> Result<(), String>,
    ) -> Result<(), String> {
        let result = self.db().and_then(write);
        self.reload();
        result
    }

    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn flat(&self) -> bool {
        self.flat
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn host(&self, id: i64) -> Option<&Host> {
        self.hosts.iter().find(|host| host.id == id)
    }

    /// 右键「复制地址」复制的文字：只有地址本身，不带用户名和端口；
    /// 老数据里带方括号的 IPv6 去掉方括号（同 0.2.14 的 `hostCopyText`）。
    pub fn address_of(&self, id: i64) -> Option<String> {
        self.host(id).map(|host| normalize_address(&host.host))
    }
}

/// 去掉首尾空白；`[2001:db8::1]` 这种带方括号的 IPv6 写成裸地址。
pub fn normalize_address(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        let inner = inner.trim();
        if inner.contains(':') {
            return inner.to_owned();
        }
    }
    trimmed.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::model::{AuthMethod, GroupDraft, HostDraft, ProxyMode};

    fn host(id: i64, name: &str, address: &str) -> Host {
        Host {
            id,
            group_id: None,
            name: name.to_owned(),
            host: address.to_owned(),
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

    #[test]
    fn copy_address_gives_only_the_bare_address() {
        let store = HostStore::from_parts(
            vec![
                host(1, "a", "  203.0.113.5 "),
                host(2, "b", "[2001:db8::1]"),
                host(3, "c", "[not-ipv6]"),
            ],
            Vec::new(),
            false,
        );
        assert_eq!(store.address_of(1).as_deref(), Some("203.0.113.5"));
        assert_eq!(store.address_of(2).as_deref(), Some("2001:db8::1"));
        assert_eq!(store.address_of(3).as_deref(), Some("[not-ipv6]"));
        assert_eq!(store.address_of(99), None);
    }

    #[test]
    fn reload_without_a_database_changes_nothing() {
        let mut store = HostStore::from_parts(vec![host(1, "a", "192.0.2.1")], Vec::new(), true);
        assert!(!store.reload());
        assert_eq!(store.hosts().len(), 1);
        assert!(store.flat());
    }

    #[test]
    fn reload_picks_up_changes_made_elsewhere_and_reports_them_once() {
        let db = Database::open_in_memory().expect("内存库");
        let mut store = HostStore::with_db(Some(db), None);
        assert!(!store.reload(), "空库第一次读不算变化");
        let db = store.db.as_ref().expect("库");
        db.host_save(&zssh_core::model::HostDraft {
            id: None,
            group_id: None,
            name: "web-1".to_owned(),
            host: "192.0.2.10".to_owned(),
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
        })
        .expect("存主机");
        assert!(store.reload());
        assert_eq!(store.hosts().len(), 1);
        assert!(!store.reload(), "没有新变化就不再报告");
    }

    #[test]
    fn the_flat_switch_follows_the_stored_setting() {
        let db = Database::open_in_memory().expect("内存库");
        db.setting_set(FLAT_DEFAULT_KEY, "\"flat\"")
            .expect("写设置");
        let mut store = HostStore::with_db(Some(db), None);
        assert!(store.reload());
        assert!(store.flat(), "没存过明确选择，看本机默认");
        store
            .db
            .as_ref()
            .expect("库")
            .setting_set(FLAT_FLAG_KEY, "false")
            .expect("写设置");
        assert!(store.reload());
        assert!(!store.flat(), "用户明确存过就用它");
    }

    fn draft(name: &str, group: Option<i64>) -> HostDraft {
        HostDraft {
            id: None,
            group_id: group,
            name: name.to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: vec!["blue".to_owned()],
            sort: 0,
            legacy_algorithms: true,
        }
    }

    fn group_draft(name: &str, parent: Option<i64>) -> GroupDraft {
        GroupDraft {
            id: None,
            name: name.to_owned(),
            parent_id: parent,
            sort: 0,
        }
    }

    #[test]
    fn moving_a_host_keeps_its_hidden_fields_and_reports_when_nothing_moved() {
        let mut store = HostStore::in_memory();
        let prod = store.save_group(&group_draft("prod", None)).expect("分组");
        let web = store.save_host(&draft("web", None)).expect("主机");
        assert!(
            store
                .move_host_to_group(web.id, Some(prod.id))
                .expect("移动")
        );
        let moved = store.host(web.id).expect("还在");
        assert_eq!(moved.group_id, Some(prod.id));
        assert_eq!(moved.tags, vec!["blue".to_owned()], "标签不能丢");
        assert!(moved.legacy_algorithms, "旧算法开关不能被重置");
        assert!(
            !store
                .move_host_to_group(web.id, Some(prod.id))
                .expect("没变"),
            "已经在那个分组里不算移动"
        );
        assert!(
            store.move_host_to_group(999, None).is_err(),
            "找不到的主机要报错"
        );
    }

    #[test]
    fn a_drop_between_groups_moves_first_then_numbers_the_target_layer() {
        let mut store = HostStore::in_memory();
        let a = store.save_group(&group_draft("a", None)).expect("分组 a");
        let b = store.save_group(&group_draft("b", None)).expect("分组 b");
        let x = store.save_host(&draft("x", Some(a.id))).expect("x");
        let p = store.save_host(&draft("p", Some(b.id))).expect("p");
        let q = store.save_host(&draft("q", Some(b.id))).expect("q");
        let plan = super::super::page_logic::plan_host_drop(store.hosts(), x.id, q.id, false)
            .expect("要调整");
        store.apply_host_drop(x.id, &plan).expect("应用");
        let in_b = super::super::page_logic::host_layer_ids(store.hosts(), Some(b.id));
        assert_eq!(in_b, vec![p.id, x.id, q.id], "x 落在 p 和 q 之间");
        assert_eq!(store.host(x.id).expect("x").group_id, Some(b.id));
        // 同组里再拖回最前，整层重新编号并持久化。
        let plan = super::super::page_logic::plan_host_drop(store.hosts(), q.id, p.id, false)
            .expect("要调整");
        store.apply_host_drop(q.id, &plan).expect("应用");
        assert_eq!(
            super::super::page_logic::host_layer_ids(store.hosts(), Some(b.id)),
            vec![q.id, p.id, x.id]
        );
        store.reset_order(Some(b.id)).expect("按名字重排");
        assert_eq!(
            super::super::page_logic::host_layer_ids(store.hosts(), Some(b.id)),
            vec![p.id, q.id, x.id],
            "回到按名字 p q x"
        );
    }

    #[test]
    fn deleting_a_group_leaves_its_hosts_ungrouped_and_group_order_persists() {
        let mut store = HostStore::in_memory();
        let a = store.save_group(&group_draft("a", None)).expect("a");
        let b = store.save_group(&group_draft("b", None)).expect("b");
        let host = store.save_host(&draft("web", Some(a.id))).expect("web");
        store.reorder_groups(&[b.id, a.id]).expect("排序");
        let order = super::super::page_logic::group_layer_ids(store.groups(), None);
        assert_eq!(order, vec![b.id, a.id]);
        store.delete_group(a.id).expect("删除");
        assert_eq!(store.host(host.id).expect("主机还在").group_id, None);
        assert_eq!(store.groups().len(), 1);
    }

    #[test]
    fn layout_options_and_recent_hosts_persist_in_the_settings_table() {
        let mut store = HostStore::in_memory();
        assert_eq!(
            store.layout(),
            &VaultLayout::default(),
            "没存过默认卡片 / 紧凑 / 手动"
        );
        store.set_view_mode(ViewMode::List).expect("视图");
        store.set_density(Density::Comfortable).expect("疏密");
        store.set_sort_mode(SortMode::Recent).expect("排序");
        store.record_recent(3);
        store.record_recent(5);
        store.record_recent(3);
        let layout = store.layout().clone();
        assert_eq!(layout.view_mode, ViewMode::List);
        assert_eq!(layout.density, Density::Comfortable);
        assert_eq!(layout.sort_mode, SortMode::Recent);
        assert_eq!(layout.recent, vec![3, 5]);
        // 键名和值的写法与 0.2.14 一致（JSON 字符串），两个版本读同一个库时互相认得。
        let db = store.db().expect("库");
        assert_eq!(
            db.setting_get(VIEW_MODE_KEY).expect("读").as_deref(),
            Some("\"list\"")
        );
        assert_eq!(
            db.setting_get(RECENT_KEY).expect("读").as_deref(),
            Some("\"3,5\"")
        );
    }

    #[test]
    fn renaming_ungrouped_into_a_group_turns_the_flat_switch_on() {
        let mut store = HostStore::in_memory();
        let loose = store.save_host(&draft("loose", None)).expect("主机");
        assert!(!store.flat());
        let created = store.save_group(&group_draft("all", None)).expect("分组");
        store
            .move_hosts(&[loose.id], Some(created.id))
            .expect("收进去");
        store.set_flat(true).expect("平铺");
        assert!(store.flat(), "同步键写了 true 就平铺");
        assert_eq!(
            store.host(loose.id).expect("主机").group_id,
            Some(created.id)
        );
    }
}
