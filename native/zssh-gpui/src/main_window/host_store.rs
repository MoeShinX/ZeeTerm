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

use std::collections::BTreeMap;

use zssh_core::model::{Group, GroupDraft, Host, HostDraft, Identity, Proxy};
use zssh_core::store::Database;
use zssh_core::store::forwards::PortForward;

use super::bar_settings::BarSettings;
use super::host_tree::effective_flat;
use super::page_logic::{
    Density, HostDrop, SortMode, ViewMode, parse_recent, record_recent, setting_json, setting_text,
};
use crate::i18n::{Msg, t};

/// 同步键：用户明确选过「平铺」。
const FLAT_FLAG_KEY: &str = "vault_ungrouped_flat";
/// 本机默认（`flat` / `classic`）；用户没选过时看它。
const FLAT_DEFAULT_KEY: &str = "layout.vault_flat_default";
pub const VIEW_MODE_KEY: &str = "layout.vault_view_mode";
pub const DENSITY_KEY: &str = "layout.vault_density";
pub const SORT_MODE_KEY: &str = "layout.vault_sort_mode";
pub const RECENT_KEY: &str = "layout.recent_hosts";
/// 主机树要不要画虚线连接线（本机设置，不同步，默认开）。
pub const TREE_GUIDES_KEY: &str = "layout.vault_tree_guides";

/// 本文件（主机库读写失败的原因）的界面文案。主机名、分组名是用户数据，不进这张表；
/// 库本身抛出来的错误原样附在句尾。
#[derive(Clone, Copy, Debug)]
enum StoreMsg {
    OpenFailedPrefix,
    /// 「读取」+ 下面某个「读什么」+ `ReadFailedSuffix` + 库里给的错误。
    ReadPrefix,
    ReadFailedSuffix,
    HostsWhat,
    GroupsWhat,
    IdentitiesWhat,
    ProxiesWhat,
    ForwardsWhat,
    HostOsWhat,
    DbNotOpen,
    HostGone,
}

impl Msg for StoreMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::OpenFailedPrefix => "打开主机库失败：",
            Self::ReadPrefix => "读取",
            Self::ReadFailedSuffix => "失败：",
            Self::HostsWhat => "主机",
            Self::GroupsWhat => "分组",
            Self::IdentitiesWhat => "凭据",
            Self::ProxiesWhat => "代理",
            Self::ForwardsWhat => "端口转发",
            Self::HostOsWhat => "主机系统",
            Self::DbNotOpen => "主机库没有打开",
            Self::HostGone => "主机已经不在库里了",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::OpenFailedPrefix => "Failed to open the host database: ",
            Self::ReadPrefix => "Failed to read ",
            Self::ReadFailedSuffix => ": ",
            Self::HostsWhat => "hosts",
            Self::GroupsWhat => "groups",
            Self::IdentitiesWhat => "credentials",
            Self::ProxiesWhat => "proxies",
            Self::ForwardsWhat => "port forwards",
            Self::HostOsWhat => "host OS info",
            Self::DbNotOpen => "The host database is not open",
            Self::HostGone => "The host is no longer in the database",
        }
    }
}

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
        protocol: None,
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
    /// 主机 id → 连上之后探测出的系统 id（`zssh_core::ssh::os_probe`），主机图标用；只在本机。
    host_os: BTreeMap<i64, String>,
    /// 主机树的虚线连接线开着没有。
    tree_guides: bool,
    /// 打不开库或读失败的原因（界面原样显示，不崩）。
    error: Option<String>,
    /// macOS 首次打开资料库可能要等 Keychain 授权。放到后台线程，界面先起来，
    /// 节拍里的 `reload` 再接手；超时/取消只会变成一条错误，不会冻住窗口。
    #[cfg(target_os = "macos")]
    pending_open: Option<std::sync::mpsc::Receiver<Result<Database, String>>>,
}

impl HostStore {
    /// 打开数据目录里的库并读一遍。打不开不是致命的：显示原因，主机列表是空的。
    pub fn open() -> Self {
        #[cfg(target_os = "macos")]
        {
            let (sender, receiver) = std::sync::mpsc::channel();
            let _ = std::thread::Builder::new()
                .name("zssh-host-db-open".to_owned())
                .spawn(move || {
                    let result = crate::host_db::open_database()
                        .map_err(|err| crate::error_text::error_text(&err));
                    let _ = sender.send(result);
                });
            let mut store = Self::with_db(None, None);
            store.pending_open = Some(receiver);
            store.reload();
            store
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut store = match crate::host_db::open_database() {
                Ok(db) => Self::with_db(Some(db), None),
                Err(err) => Self::with_db(
                    None,
                    Some(format!(
                        "{}{}",
                        t(StoreMsg::OpenFailedPrefix),
                        crate::error_text::error_text(&err)
                    )),
                ),
            };
            store.reload();
            store
        }
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
            host_os: BTreeMap::new(),
            tree_guides: true,
            error,
            #[cfg(target_os = "macos")]
            pending_open: None,
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

    /// 后台开库还在进行：主窗口据此显示加载页，不提前展示主机或恢复提示。
    pub fn is_pending_open(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.pending_open.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// 库有没有被别的连接改过（SQLite `PRAGMA data_version`）；读不出来时 `None`。
    ///
    /// 主窗口用它决定要不要重读，不再每 2 秒无条件跑 12 条查询（审查 M-14）。
    pub fn data_version(&self) -> Option<i64> {
        self.db.as_ref()?.data_version().ok()
    }

    /// 从库里重读；返回内容有没有变（界面据此决定要不要重绘）。读不出来时保留旧内容。
    pub fn reload(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        let mut adopted = false;
        #[cfg(not(target_os = "macos"))]
        let adopted = false;
        #[cfg(target_os = "macos")]
        if let Some(receiver) = self.pending_open.take() {
            match receiver.try_recv() {
                Ok(Ok(db)) => {
                    self.db = Some(db);
                    adopted = true;
                }
                Ok(Err(err)) => {
                    self.error = Some(format!("{}{}", t(StoreMsg::OpenFailedPrefix), err));
                    return true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.pending_open = Some(receiver);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.error = Some(t(StoreMsg::DbNotOpen).to_owned());
                    return true;
                }
            }
        }
        let Some(db) = &self.db else {
            return adopted;
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
                    *error = Some(format!(
                        "{}{what}{}{err}",
                        t(StoreMsg::ReadPrefix),
                        t(StoreMsg::ReadFailedSuffix)
                    ));
                    None
                }
            }
        }
        let hosts = read(&mut error, t(StoreMsg::HostsWhat), db.host_list());
        let groups = read(&mut error, t(StoreMsg::GroupsWhat), db.group_list());
        // 内置助手接口的 API Key 也存在保险库里，但它归接口管：凭据页不显示、主机表单选不到。
        let ai_keys = db.ai_endpoint_key_identity_ids().unwrap_or_default();
        let identities = read(&mut error, t(StoreMsg::IdentitiesWhat), db.identity_list())
            .map(|list| without_ai_keys(list, &ai_keys));
        let proxies = read(&mut error, t(StoreMsg::ProxiesWhat), db.proxy_list());
        let forwards = read(&mut error, t(StoreMsg::ForwardsWhat), db.forward_list());
        let host_os = read(&mut error, t(StoreMsg::HostOsWhat), db.host_os_list());
        let tree_guides = super::bar_settings::parse_bool(
            db.setting_get(TREE_GUIDES_KEY).ok().flatten().as_deref(),
            true,
        );
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
        replace(&mut self.host_os, host_os, &mut changed);
        replace(&mut self.tree_guides, Some(tree_guides), &mut changed);
        changed || adopted
    }

    // ---- 写操作：每一步写完立刻重读 ----

    /// 打开的库；打不开时给界面一句能直接显示的话。
    pub fn db(&self) -> Result<&Database, String> {
        self.db.as_ref().ok_or_else(|| {
            self.error
                .clone()
                .unwrap_or_else(|| t(StoreMsg::DbNotOpen).to_owned())
        })
    }

    fn after_write<T>(
        &mut self,
        result: Result<T, zssh_core::error::ZsshError>,
    ) -> Result<T, String> {
        // 写失败也重读：库里的真实状态才是界面该显示的。
        self.reload();
        result.map_err(|err| crate::error_text::error_text(&err))
    }

    pub fn save_host(&mut self, draft: &HostDraft) -> Result<Host, String> {
        let result = self.db()?.host_save(draft);
        self.after_write(result)
    }

    /// 按保存计划落库：主机的密码身份、主机本身、释放不再用的独占密码（见 `host_form::apply_save`）。
    pub fn save_host_plan(
        &mut self,
        plan: super::host_form::SavePlan,
    ) -> Result<Host, super::protocol_form::FieldError> {
        use super::protocol_form::{FieldError, FormField};
        let result = self
            .db()
            .map_err(|message| FieldError::new(FormField::General, message))
            .and_then(|db| super::host_form::apply_save(db, plan));
        // 写失败也重读：库里的真实状态才是界面该显示的。
        self.reload();
        result
    }

    /// 读库里这台主机现在的样子（保存前合并未展示的字段用）。
    pub fn host_fresh(&self, id: i64) -> Result<Host, String> {
        self.db()?
            .host_get(id)
            .map_err(|err| crate::error_text::error_text(&err))
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
            return Err(t(StoreMsg::HostGone).to_owned());
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

    /// 这台主机该显示哪个发行版图标：只有 SSH 主机、探测出过系统、且我们有这个系统的图标时才有。
    /// Telnet / 串口不探测，沿用各自的协议图标。
    pub fn os_icon_of(&self, host: &Host) -> Option<&str> {
        if host.protocol.protocol != zssh_core::model::Protocol::Ssh {
            return None;
        }
        self.host_os
            .get(&host.id)
            .map(String::as_str)
            .filter(|os| crate::os_icons::is_known(os))
    }

    /// 主机树画不画虚线连接线（设置 → 外观，默认开）。
    pub fn tree_guides(&self) -> bool {
        self.tree_guides
    }

    pub fn set_tree_guides(&mut self, on: bool) -> Result<(), String> {
        let result = self
            .db()?
            .setting_set(TREE_GUIDES_KEY, if on { "true" } else { "false" });
        self.after_write(result)
    }

    /// 造一条探测记录（测试 / 调试驱动用：真实路径是连上之后由 `host_os` 写库）。
    #[cfg(any(test, debug_assertions))]
    pub fn set_host_os(&mut self, host_id: i64, os: &str) -> Result<(), String> {
        let result = self.db()?.host_os_set(host_id, os).map(|_| ());
        self.after_write(result)
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

/// 去掉被内置助手接口当 API Key 用的凭据（界面的凭据列表、主机表单的下拉都只看这份）。
pub fn without_ai_keys(mut identities: Vec<Identity>, ai_keys: &[i64]) -> Vec<Identity> {
    identities.retain(|identity| !ai_keys.contains(&identity.id));
    identities
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::model::{AuthMethod, GroupDraft, HostDraft, ProxyMode};

    fn host(id: i64, name: &str, address: &str) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
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
    fn an_endpoints_api_key_credential_is_hidden_from_the_credential_list_and_host_form() {
        use zssh_core::ai::agent::builtin::config::{
            AiEndpointDraft, EndpointKind, OpenAiOptionsConfig,
        };
        use zssh_core::model::{IdentityDraft, IdentityKind};
        let mut store = HostStore::in_memory();
        let db = store.db().expect("内存库");
        let normal = db
            .identity_save(&IdentityDraft {
                id: None,
                name: "普通密码".to_owned(),
                kind: IdentityKind::Password,
                key_path: None,
                secret: Some("pw".to_owned()),
                passphrase: None,
            })
            .expect("存凭据");
        let endpoint = db
            .ai_endpoint_save(&AiEndpointDraft {
                id: None,
                name: "主力".to_owned(),
                kind: EndpointKind::OpenAiChat,
                base_url: "https://api.example.com/v1".to_owned(),
                api_key: Some("sk-test-0001".to_owned()),
                default_model: None,
                models: Vec::new(),
                overrides: Default::default(),
                openai_options: OpenAiOptionsConfig::default(),
                preset_id: None,
            })
            .expect("存接口");
        let key_id = endpoint.key_identity_id.expect("Key 在保险库里");
        // 库里确实有两条凭据，但界面读到的只有普通那条。
        assert_eq!(db.identity_list().expect("读").len(), 2);
        assert!(store.reload());
        let names: Vec<&str> = store.identities().iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["普通密码"], "{names:?}");
        assert!(store.identities().iter().all(|i| i.id != key_id));
        // 主机表单读的也是这份（`store.identities()`）；核心库另外在保存主机时拒绝选 Key 凭据
        // （见 core 的 `the_key_identities_are_listed_and_hosts_cannot_pick_them`）。
        let _ = normal;
    }

    #[test]
    fn without_ai_keys_only_drops_the_listed_ids() {
        use zssh_core::model::{Identity, IdentityKind};
        let identity = |id: i64| Identity {
            id,
            name: format!("凭据{id}"),
            kind: IdentityKind::Password,
            key_path: None,
            has_secret: true,
        };
        let kept = without_ai_keys(vec![identity(1), identity(2), identity(3)], &[2]);
        assert_eq!(kept.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(without_ai_keys(vec![identity(1)], &[]).len(), 1);
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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_background_database_open_is_adopted_by_the_next_reload() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut store = HostStore::with_db(None, None);
        store.pending_open = Some(receiver);
        assert!(!store.reload(), "还没打开时不假装有数据");
        assert!(store.db().is_err());
        sender
            .send(Ok(Database::open_in_memory().expect("内存库")))
            .unwrap();
        assert!(store.reload(), "接手新库要通知界面重画");
        assert!(store.db().is_ok());

        // 后台打开失败也必须变成界面错误，而不是永远空白。
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut store = HostStore::with_db(None, None);
        store.pending_open = Some(receiver);
        sender.send(Err("授权失败".to_owned())).unwrap();
        assert!(store.reload());
        match store.db() {
            Ok(_) => panic!("失败的后台打开不能变成可用库"),
            Err(message) => assert!(message.contains("授权失败")),
        }
    }

    #[test]
    fn reload_picks_up_changes_made_elsewhere_and_reports_them_once() {
        let db = Database::open_in_memory().expect("内存库");
        let mut store = HostStore::with_db(Some(db), None);
        assert!(!store.reload(), "空库第一次读不算变化");
        let db = store.db.as_ref().expect("库");
        db.host_save(&zssh_core::model::HostDraft {
            protocol: None,
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
            protocol: None,
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

    #[test]
    fn a_probed_ssh_host_shows_its_distro_icon_and_everything_else_keeps_the_protocol_icon() {
        use zssh_core::model::{Protocol, ProtocolSettings};
        let mut store = HostStore::in_memory();
        let ssh = store.save_host(&draft("web-1", None)).expect("主机");
        let mut telnet_draft = draft("switch-1", None);
        telnet_draft.protocol = Some(ProtocolSettings::new(Protocol::Telnet));
        let telnet = store.save_host(&telnet_draft).expect("主机");
        // 还没探测过：协议图标。
        assert_eq!(store.os_icon_of(store.host(ssh.id).expect("主机")), None);
        store.set_host_os(ssh.id, "debian").expect("写");
        // Telnet 不探测；就算库里有记录（不该有）也不用发行版图标。
        store.set_host_os(telnet.id, "debian").expect("写");
        assert_eq!(
            store.os_icon_of(store.host(ssh.id).expect("主机")),
            Some("debian")
        );
        assert_eq!(store.os_icon_of(store.host(telnet.id).expect("主机")), None);
        // 库里是我们没有图标的 id（比如以后的版本多探出一种）：回退到协议图标，不崩。
        store.set_host_os(ssh.id, "plan9").expect("写");
        assert_eq!(store.os_icon_of(store.host(ssh.id).expect("主机")), None);
    }

    #[test]
    fn probe_results_are_picked_up_by_the_periodic_reload_and_follow_the_host() {
        let mut store = HostStore::in_memory();
        let host = store.save_host(&draft("db-1", None)).expect("主机");
        // 后台探测任务自己开库写（这里直接写同一个库），界面靠定时重读看到。
        store
            .db()
            .expect("库")
            .host_os_set(host.id, "ubuntu")
            .expect("写");
        assert_eq!(store.os_icon_of(store.host(host.id).expect("主机")), None);
        assert!(store.reload(), "探测结果变了要报告有变化，界面才重绘");
        assert_eq!(
            store.os_icon_of(store.host(host.id).expect("主机")),
            Some("ubuntu")
        );
        assert!(!store.reload());
        // 删主机：探测记录一起清掉（外键级联）。
        store.delete_host_with_passwords(host.id).expect("删");
        assert!(
            store
                .db()
                .expect("库")
                .host_os_list()
                .expect("读")
                .is_empty()
        );
    }

    #[test]
    fn tree_guides_are_on_by_default_and_the_switch_is_remembered_locally() {
        let mut store = HostStore::in_memory();
        assert!(store.tree_guides());
        store.set_tree_guides(false).expect("关");
        assert!(!store.tree_guides());
        assert_eq!(
            store
                .db()
                .expect("库")
                .setting_get(TREE_GUIDES_KEY)
                .expect("读")
                .as_deref(),
            Some("false")
        );
        store.set_tree_guides(true).expect("开");
        assert!(store.tree_guides());
        // 读不懂的值回到默认（开）。
        store
            .db()
            .expect("库")
            .setting_set(TREE_GUIDES_KEY, "\"maybe\"")
            .expect("写");
        store.reload();
        assert!(store.tree_guides());
    }

    #[test]
    fn the_tree_guides_key_is_a_local_only_setting() {
        use zssh_core::sync::settings::{SETTINGS_SYNC_CLASS, SyncMode};
        let class = SETTINGS_SYNC_CLASS
            .iter()
            .find(|(key, _)| *key == TREE_GUIDES_KEY)
            .map(|(_, mode)| *mode);
        assert_eq!(class, Some(SyncMode::LocalOnly));
    }
}
