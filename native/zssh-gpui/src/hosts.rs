//! 主机库窗口：本地 SQLite 的增删改查，不发起连接。
//!
//! 复用 `zssh_core::store::Database`（同一套校验和 schema），数据目录由 `ZSSH_DATA_DIR` 决定。
//!
//! 保存已有主机时，表单没有展示的字段（标签、老算法开关、排序）一律保持库里的原值，
//! 只覆盖表单里有的字段；删除主机前先列出受影响的内容（经它跳转的主机、它的端口转发规则）。

use crate::host_db::open_database;
use ::gpui::{FocusHandle, Subscription, Task};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zssh_core::ai::host_memory::{MAX_MEMORY_CHARS, clean_memory};
use zssh_core::model::{
    AuthMethod, Group, GroupDraft, Host, HostDraft, Identity, IdentityKind, Proxy, ProxyMode,
    QuickTarget,
};
use zssh_core::store::Database;
use zssh_core::store::forwards::PortForward;

use crate::i18n::{Msg, t};

/// 主机库窗口自己的界面文案。
///
/// 只放应用自己的固定文案；主机名、用户名、路径、错误原文一律原样显示。
#[derive(Clone, Copy, Debug)]
enum HostsMsg {
    NamePlaceholder,
    HostPlaceholder,
    PortPlaceholder,
    UsernamePlaceholder,
    SearchPlaceholder,
    NewGroupPlaceholder,
    DbOpenFailed,
    HostCountSuffix,
    ReadHostsFailedPrefix,
    ReadGroupsFailedPrefix,
    ReadProxiesFailedPrefix,
    ReadIdentitiesFailedPrefix,
    QuickFilledPrefix,
    QuickFilledSuffix,
    OrderAdjustedPrefix,
    OrderAdjustedSuffix,
    GroupNameEmpty,
    GroupCreatedPrefix,
    ReadHostFailedNoSavePrefix,
    SavedPrefix,
    DeletedPrefix,
    SelectHostConnect,
    SelectHostFile,
    SelectHostMemory,
    ReadMemoryFailedPrefix,
    MemoryPlaceholder,
    EditingMemoryPrefix,
    EditingMemorySuffix,
    MemoryPanelClosed,
    MemorySaved,
    MemoryCleared,
    ClearedMemoryPrefix,
    ClearedMemorySuffix,
    ClearAgainPrefix,
    ClearAgainSuffix,
    DeleteAgainPrefix,
    JumpImpactPrefix,
    JumpImpactMiddle,
    JumpImpactSuffix,
    ForwardsImpactPrefix,
    ForwardsImpactSuffix,
    ListSeparator,
    HasMemoryMark,
    NoGroup,
    NoneLabel,
    DefaultProxy,
    DirectProxy,
    ProxyFallback,
    AuthLabel,
    PasswordAuth,
    KeyAuth,
    IdentityLabel,
    GroupLabel,
    CreateGroup,
    ProxyLabel,
    JumpLabel,
    AuthHint,
    MemoryPanelPrefix,
    UpdatedAtPrefix,
    JustNow,
    MinutesAgoSuffix,
    HoursAgoSuffix,
    DaysAgoSuffix,
    NoMemoryYet,
    Clear,
    ConfirmClear,
    Save,
    Cancel,
    MaxCharsPrefix,
    MaxCharsMiddle,
    Title,
    New,
    Connect,
    Files,
    Memory,
    Delete,
    ConfirmDelete,
    Parse,
    Proxies,
    Identities,
}

impl Msg for HostsMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::NamePlaceholder => "名称",
            Self::HostPlaceholder => "主机名或 IP",
            Self::PortPlaceholder => "端口",
            Self::UsernamePlaceholder => "用户名",
            Self::SearchPlaceholder => "搜索名称 / 地址 / 用户",
            Self::NewGroupPlaceholder => "新分组名",
            Self::DbOpenFailed => "打开主机库失败（数据目录不可写？）",
            Self::HostCountSuffix => " 台主机",
            Self::ReadHostsFailedPrefix => "读取主机失败：",
            Self::ReadGroupsFailedPrefix => "读取分组失败：",
            Self::ReadProxiesFailedPrefix => "读取代理失败：",
            Self::ReadIdentitiesFailedPrefix => "读取身份失败：",
            Self::QuickFilledPrefix => "已填入 ",
            Self::QuickFilledSuffix => "，确认后保存",
            Self::OrderAdjustedPrefix => "已调整 ",
            Self::OrderAdjustedSuffix => " 的顺序",
            Self::GroupNameEmpty => "分组名不能为空",
            Self::GroupCreatedPrefix => "已创建分组 ",
            Self::ReadHostFailedNoSavePrefix => "读取主机失败，没有保存：",
            Self::SavedPrefix => "已保存 ",
            Self::DeletedPrefix => "已删除 ",
            Self::SelectHostConnect => "先选一台主机再连接",
            Self::SelectHostFile => "先选一台主机再浏览文件",
            Self::SelectHostMemory => "先选一台主机再看记忆",
            Self::ReadMemoryFailedPrefix => "读取记忆失败：",
            Self::MemoryPlaceholder => "这台主机的服务、已知问题、近期改动；密码、令牌会自动隐藏",
            Self::EditingMemoryPrefix => "编辑 ",
            Self::EditingMemorySuffix => " 的记忆",
            Self::MemoryPanelClosed => "已关闭记忆面板（未保存的输入已丢弃）",
            Self::MemorySaved => "已保存",
            Self::MemoryCleared => "已清空",
            Self::ClearedMemoryPrefix => "已清空 ",
            Self::ClearedMemorySuffix => " 的记忆",
            Self::ClearAgainPrefix => "再次点击「确认清空」才会清空 ",
            Self::ClearAgainSuffix => " 的记忆",
            Self::DeleteAgainPrefix => "再点一次「确认删除」才会删除 ",
            Self::JumpImpactPrefix => "；",
            Self::JumpImpactMiddle => " 台经它跳转的主机（",
            Self::JumpImpactSuffix => "）会改成直连目标地址",
            Self::ForwardsImpactPrefix => "；它的 ",
            Self::ForwardsImpactSuffix => " 条端口转发规则会一起删除",
            Self::ListSeparator => "、",
            Self::HasMemoryMark => "有记忆",
            Self::NoGroup => "无分组",
            Self::NoneLabel => "无",
            Self::DefaultProxy => "默认",
            Self::DirectProxy => "直连",
            Self::ProxyFallback => "代理",
            Self::AuthLabel => "认证",
            Self::PasswordAuth => "密码",
            Self::KeyAuth => "密钥",
            Self::IdentityLabel => "凭据",
            Self::GroupLabel => "分组",
            Self::CreateGroup => "建组",
            Self::ProxyLabel => "代理",
            Self::JumpLabel => "跳板",
            Self::AuthHint => "认证方式只存方式本身；密码/私钥在凭据库接入前不落盘。",
            Self::MemoryPanelPrefix => "记忆 · ",
            Self::UpdatedAtPrefix => "更新于 ",
            Self::JustNow => "刚刚",
            Self::MinutesAgoSuffix => " 分钟前",
            Self::HoursAgoSuffix => " 小时前",
            Self::DaysAgoSuffix => " 天前",
            Self::NoMemoryYet => "还没有记忆",
            Self::Clear => "清空",
            Self::ConfirmClear => "确认清空",
            Self::Save => "保存",
            Self::Cancel => "取消",
            Self::MaxCharsPrefix => "最多 ",
            Self::MaxCharsMiddle => " 字；密码、令牌会隐藏，不传给列表",
            Self::Title => "主机库",
            Self::New => "新建",
            Self::Connect => "连接",
            Self::Files => "文件",
            Self::Memory => "记忆",
            Self::Delete => "删除",
            Self::ConfirmDelete => "确认删除",
            Self::Parse => "解析",
            Self::Proxies => "代理",
            Self::Identities => "身份",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::NamePlaceholder => "Name",
            Self::HostPlaceholder => "Host name or IP",
            Self::PortPlaceholder => "Port",
            Self::UsernamePlaceholder => "User name",
            Self::SearchPlaceholder => "Search name / address / user",
            Self::NewGroupPlaceholder => "New group name",
            Self::DbOpenFailed => {
                "Could not open the host library (is the data directory writable?)"
            }
            Self::HostCountSuffix => " hosts",
            Self::ReadHostsFailedPrefix => "Could not read hosts: ",
            Self::ReadGroupsFailedPrefix => "Could not read groups: ",
            Self::ReadProxiesFailedPrefix => "Could not read proxies: ",
            Self::ReadIdentitiesFailedPrefix => "Could not read credentials: ",
            Self::QuickFilledPrefix => "Filled in ",
            Self::QuickFilledSuffix => "; save to confirm",
            Self::OrderAdjustedPrefix => "Reordered ",
            Self::OrderAdjustedSuffix => "",
            Self::GroupNameEmpty => "Group name cannot be empty",
            Self::GroupCreatedPrefix => "Created group ",
            Self::ReadHostFailedNoSavePrefix => "Could not read the host; not saved: ",
            Self::SavedPrefix => "Saved ",
            Self::DeletedPrefix => "Deleted ",
            Self::SelectHostConnect => "Select a host before connecting",
            Self::SelectHostFile => "Select a host before browsing files",
            Self::SelectHostMemory => "Select a host before viewing memory",
            Self::ReadMemoryFailedPrefix => "Could not read memory: ",
            Self::MemoryPlaceholder => {
                "Services, known issues, recent changes for this host; passwords and tokens are hidden automatically"
            }
            Self::EditingMemoryPrefix => "Editing memory for ",
            Self::EditingMemorySuffix => "",
            Self::MemoryPanelClosed => "Memory panel closed (unsaved input was discarded)",
            Self::MemorySaved => "Saved",
            Self::MemoryCleared => "Cleared",
            Self::ClearedMemoryPrefix => "Cleared memory for ",
            Self::ClearedMemorySuffix => "",
            Self::ClearAgainPrefix => "Click \"Confirm Clear\" again to clear memory for ",
            Self::ClearAgainSuffix => "",
            Self::DeleteAgainPrefix => "Click \"Confirm Delete\" again to delete ",
            Self::JumpImpactPrefix => "; ",
            Self::JumpImpactMiddle => " host(s) that jump through it (",
            Self::JumpImpactSuffix => ") will connect directly to their targets",
            Self::ForwardsImpactPrefix => "; its ",
            Self::ForwardsImpactSuffix => " port forwarding rule(s) will be deleted too",
            Self::ListSeparator => ", ",
            Self::HasMemoryMark => "Has memory",
            Self::NoGroup => "No group",
            Self::NoneLabel => "None",
            Self::DefaultProxy => "Default",
            Self::DirectProxy => "Direct",
            Self::ProxyFallback => "Proxy",
            Self::AuthLabel => "Authentication",
            Self::PasswordAuth => "Password",
            Self::KeyAuth => "Key",
            Self::IdentityLabel => "Credential",
            Self::GroupLabel => "Group",
            Self::CreateGroup => "Create",
            Self::ProxyLabel => "Proxy",
            Self::JumpLabel => "Jump",
            Self::AuthHint => {
                "Only the authentication method is stored; passwords/private keys are not written until the credential store is connected."
            }
            Self::MemoryPanelPrefix => "Memory · ",
            Self::UpdatedAtPrefix => "Updated ",
            Self::JustNow => "just now",
            Self::MinutesAgoSuffix => " min ago",
            Self::HoursAgoSuffix => " h ago",
            Self::DaysAgoSuffix => " d ago",
            Self::NoMemoryYet => "No memory yet",
            Self::Clear => "Clear",
            Self::ConfirmClear => "Confirm Clear",
            Self::Save => "Save",
            Self::Cancel => "Cancel",
            Self::MaxCharsPrefix => "At most ",
            Self::MaxCharsMiddle => {
                " characters; passwords and tokens are hidden and never sent to the list"
            }
            Self::Title => "Host Library",
            Self::New => "New",
            Self::Connect => "Connect",
            Self::Files => "Files",
            Self::Memory => "Memory",
            Self::Delete => "Delete",
            Self::ConfirmDelete => "Confirm Delete",
            Self::Parse => "Parse",
            Self::Proxies => "Proxies",
            Self::Identities => "Credentials",
        }
    }
}

/// 代理库窗口编号。
static NEXT_PROXIES: AtomicU64 = AtomicU64::new(1);

/// 身份库窗口编号。
static NEXT_IDENTITIES: AtomicU64 = AtomicU64::new(1);

/// 主机库窗口编号。
static NEXT_HOSTS: AtomicU64 = AtomicU64::new(1);

/// 「AI 使用中」标记的对表间隔；只是提示，不需要跟 100ms 的对话轮询一样快。
const ACTIVITY_TICK: Duration = Duration::from_secs(1);

#[derive(Clone, PartialEq, Eq)]
struct FormValues {
    name: String,
    host: String,
    port: String,
    user: String,
    auth: AuthMethod,
}

impl Default for FormValues {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: String::new(),
            user: String::new(),
            auth: AuthMethod::Password,
        }
    }
}

impl FormValues {
    fn new() -> Self {
        Self {
            port: "22".to_owned(),
            auth: AuthMethod::Password,
            ..Default::default()
        }
    }

    fn from_host(host: &Host) -> Self {
        Self {
            name: host.name.clone(),
            host: host.host.clone(),
            port: host.port.to_string(),
            user: host.username.clone(),
            auth: host.auth_method,
        }
    }
}

/// 快速连接串 → 表单；名称默认用主机名。
fn form_from_quick(target: &QuickTarget) -> FormValues {
    FormValues {
        name: target.host.clone(),
        host: target.host.clone(),
        port: target.port.to_string(),
        user: target.username.clone(),
        auth: AuthMethod::Password,
    }
}

/// 循环切换分组：无分组 → 第一个 → … → 无分组。
fn next_group_id(current: Option<i64>, groups: &[Group]) -> Option<i64> {
    match current {
        None => groups.first().map(|group| group.id),
        Some(id) => {
            let next = groups
                .iter()
                .position(|group| group.id == id)
                .and_then(|index| groups.get(index + 1));
            next.map(|group| group.id)
        }
    }
}

/// 代理按钮的三态循环：默认 → 直连 → 各代理 → 默认。
fn next_proxy(
    mode: ProxyMode,
    proxy_id: Option<i64>,
    proxies: &[Proxy],
) -> (ProxyMode, Option<i64>) {
    match mode {
        ProxyMode::Default => (ProxyMode::None, None),
        ProxyMode::None => match proxies.first() {
            Some(proxy) => (ProxyMode::Custom, Some(proxy.id)),
            None => (ProxyMode::Default, None),
        },
        ProxyMode::Custom => {
            let next = proxy_id
                .and_then(|id| proxies.iter().position(|proxy| proxy.id == id))
                .and_then(|index| proxies.get(index + 1));
            match next {
                Some(proxy) => (ProxyMode::Custom, Some(proxy.id)),
                None => (ProxyMode::Default, None),
            }
        }
    }
}

/// 跳板机循环：无 → 其他主机依次 → 无。
fn next_jump(current: Option<i64>, hosts: &[Host], selected: Option<i64>) -> Option<i64> {
    let candidates: Vec<&Host> = hosts
        .iter()
        .filter(|host| Some(host.id) != selected)
        .collect();
    match current {
        None => candidates.first().map(|host| host.id),
        Some(id) => candidates
            .iter()
            .position(|host| host.id == id)
            .and_then(|index| candidates.get(index + 1))
            .map(|host| host.id),
    }
}

/// 上移/下移：返回交换后的 id 顺序；越界返回 `None`。
fn reordered(ids: &[i64], index: usize, delta: isize) -> Option<Vec<i64>> {
    if index >= ids.len() {
        return None;
    }
    let target = index as isize + delta;
    if target < 0 || target as usize >= ids.len() {
        return None;
    }
    let mut out = ids.to_vec();
    out.swap(index, target as usize);
    Some(out)
}

/// 搜索：名称、地址、用户名、端口任一包含即可（忽略大小写）。
fn matches_query(host: &Host, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    host.name.to_lowercase().contains(&query)
        || host.host.to_lowercase().contains(&query)
        || host.username.to_lowercase().contains(&query)
        || host.port.to_string().contains(&query)
}

// ---------------------------------------------------------------------------
// 主机长期记忆：编辑面板的状态与纯逻辑
// ---------------------------------------------------------------------------

/// 按「保存」要做什么：清理后是空串就清空记忆，否则写入正文。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MemoryAction {
    Save(String),
    Clear,
}

/// 先过 `clean_memory`（去控制字符、隐藏密码 / 令牌、限长）再决定是不是清空。
/// `clean_memory` 自己去首尾空白，所以只输入空白等于清空。
pub(crate) fn memory_action(text: &str) -> Result<MemoryAction, String> {
    let cleaned = clean_memory(text)?;
    if cleaned.is_empty() {
        Ok(MemoryAction::Clear)
    } else {
        Ok(MemoryAction::Save(cleaned))
    }
}

/// 当前 Unix 秒；时钟早于纪元时按 0 处理（这个值只用来显示）。
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 记忆更新时间只显示相对时间：`HostMemory` 存的是 Unix 秒，不需要本地时区。
fn memory_age_label(updated_at: i64, now: i64) -> String {
    let age = now.saturating_sub(updated_at).max(0) as u64;
    if age < 60 {
        t(HostsMsg::JustNow).to_owned()
    } else if age < 3_600 {
        format!("{}{}", age / 60, t(HostsMsg::MinutesAgoSuffix))
    } else if age < 86_400 {
        format!("{}{}", age / 3_600, t(HostsMsg::HoursAgoSuffix))
    } else {
        format!("{}{}", age / 86_400, t(HostsMsg::DaysAgoSuffix))
    }
}

/// 面板标题下面那行：有记忆写更新时间，没有就直说。
pub(crate) fn memory_summary(updated_at: Option<i64>, now: i64) -> String {
    match updated_at {
        Some(updated_at) => format!(
            "{}{}",
            t(HostsMsg::UpdatedAtPrefix),
            memory_age_label(updated_at, now)
        ),
        None => t(HostsMsg::NoMemoryYet).to_owned(),
    }
}

/// 记忆面板里除输入框以外的状态。纯数据，单测不用起窗口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemoryDraft {
    pub(crate) host_id: i64,
    /// 库里现有的更新时间；`None` = 还没有记忆。
    pub(crate) updated_at: Option<i64>,
    /// 「清空」已经点过一次，按钮改成「确认清空」。
    clear_armed: bool,
    /// `clean_memory` / 存储层的错误原文；只在这个面板里显示。
    pub(crate) error: Option<String>,
    /// 上一次保存成功后的提示；和错误互斥。
    pub(crate) notice: Option<String>,
}

impl MemoryDraft {
    /// 打开面板：只记主机和更新时间，确认状态、错误、提示都从头开始。
    pub(crate) fn open(host_id: i64, updated_at: Option<i64>) -> Self {
        Self {
            host_id,
            updated_at,
            clear_armed: false,
            error: None,
            notice: None,
        }
    }

    /// 「清空」按钮文字：第一次点只是上锁。
    pub(crate) fn clear_label(&self) -> &'static str {
        if self.clear_armed {
            t(HostsMsg::ConfirmClear)
        } else {
            t(HostsMsg::Clear)
        }
    }

    /// 点「清空」：第一次上锁并返回 `false`，第二次返回 `true` 让调用方真去清库。
    pub(crate) fn click_clear(&mut self) -> bool {
        if self.clear_armed {
            true
        } else {
            self.clear_armed = true;
            false
        }
    }

    /// 保存 / 清空成功：刷新时间，错误、提示、确认状态归位。
    pub(crate) fn saved(&mut self, updated_at: Option<i64>, notice: &str) {
        self.updated_at = updated_at;
        self.clear_armed = false;
        self.error = None;
        self.notice = Some(notice.to_owned());
    }
}

/// 打开着的记忆面板：同一时间只有一台主机有。
struct MemoryEditor {
    draft: MemoryDraft,
    host_name: String,
    input: Entity<TextareaState>,
}

/// 表单 → 存储层草稿；端口留空按 22，填错按 0（交给存储层报「端口要在 1-65535」）。
/// 主机上除表单字段外、由按钮选择的部分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostExtras {
    group_id: Option<i64>,
    proxy_mode: ProxyMode,
    proxy_id: Option<i64>,
    jump_host_id: Option<i64>,
    identity_id: Option<i64>,
}

impl Default for HostExtras {
    fn default() -> Self {
        Self {
            group_id: None,
            proxy_mode: ProxyMode::Default,
            proxy_id: None,
            jump_host_id: None,
            identity_id: None,
        }
    }
}

/// 存储层要求：密码认证配密码凭据，密钥认证配密钥凭据，Agent 不限。
pub(crate) fn identity_matches(auth: AuthMethod, kind: IdentityKind) -> bool {
    match auth {
        AuthMethod::Password => kind == IdentityKind::Password,
        AuthMethod::Key => matches!(kind, IdentityKind::KeyFile | IdentityKind::KeyInline),
        AuthMethod::Agent => true,
    }
}

/// 凭据按钮循环：无 → 匹配当前认证方式的身份依次 → 无。
fn next_identity(current: Option<i64>, auth: AuthMethod, identities: &[Identity]) -> Option<i64> {
    let candidates: Vec<&Identity> = identities
        .iter()
        .filter(|identity| identity_matches(auth, identity.kind))
        .collect();
    match current {
        None => candidates.first().map(|identity| identity.id),
        Some(id) => candidates
            .iter()
            .position(|identity| identity.id == id)
            .and_then(|index| candidates.get(index + 1))
            .map(|identity| identity.id),
    }
}

fn draft_from(values: &FormValues, id: Option<i64>, extras: HostExtras) -> HostDraft {
    let port = values.port.trim();
    let port = if port.is_empty() {
        22
    } else {
        port.parse::<u16>().unwrap_or(0)
    };
    HostDraft {
        protocol: None,
        id,
        group_id: extras.group_id,
        name: values.name.trim().to_owned(),
        host: values.host.trim().to_owned(),
        port,
        username: values.user.trim().to_owned(),
        auth_method: values.auth,
        identity_id: extras.identity_id,
        proxy_id: extras.proxy_id,
        jump_host_id: extras.jump_host_id,
        proxy_mode: extras.proxy_mode,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

/// 表单没有展示的字段（标签、老算法开关、排序）保持库里的原值，只覆盖表单里有的字段。
///
/// 新建主机（`existing` 为 `None`）用默认值。不照这样做的话，在表单里只改个名字点保存，
/// 就会把标签清空、把「旧算法兼容」重置为关，之后这台老设备连不上（X1）。
fn keep_hidden_fields(mut draft: HostDraft, existing: Option<&Host>) -> HostDraft {
    if let Some(host) = existing {
        draft.tags = host.tags.clone();
        draft.legacy_algorithms = host.legacy_algorithms;
        draft.sort = host.sort;
    }
    draft
}

/// 删除一台主机会连带影响的东西。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HostDeleteImpact {
    /// 把它当跳板机的主机名：删掉之后这些主机的跳板机被清空，下次连接**直连目标地址**。
    pub(crate) via_jump: Vec<String>,
    /// 这台主机自己的端口转发规则条数：随主机一起删。
    pub(crate) forwards: usize,
}

pub(crate) fn host_delete_impact(
    hosts: &[Host],
    forwards: &[PortForward],
    id: i64,
) -> HostDeleteImpact {
    HostDeleteImpact {
        via_jump: hosts
            .iter()
            .filter(|host| host.id != id && host.jump_host_id == Some(id))
            .map(|host| host.name.clone())
            .collect(),
        forwards: forwards.iter().filter(|rule| rule.host_id == id).count(),
    }
}

/// 第一次点「删除」时状态栏上的话：有连带影响就逐条列出来。
pub(crate) fn host_delete_prompt(name: &str, impact: &HostDeleteImpact) -> String {
    let mut text = format!("{}{name}", t(HostsMsg::DeleteAgainPrefix));
    if !impact.via_jump.is_empty() {
        text.push_str(&format!(
            "{}{}{}{}{}",
            t(HostsMsg::JumpImpactPrefix),
            impact.via_jump.len(),
            t(HostsMsg::JumpImpactMiddle),
            impact.via_jump.join(t(HostsMsg::ListSeparator)),
            t(HostsMsg::JumpImpactSuffix)
        ));
    }
    if impact.forwards > 0 {
        text.push_str(&format!(
            "{}{}{}",
            t(HostsMsg::ForwardsImpactPrefix),
            impact.forwards,
            t(HostsMsg::ForwardsImpactSuffix)
        ));
    }
    text
}

pub struct HostsDemo {
    db: Option<Database>,
    hosts: Vec<Host>,
    groups: Vec<Group>,
    proxies: Vec<Proxy>,
    identities: Vec<Identity>,
    selected: Option<i64>,
    group_id: Option<i64>,
    proxy_mode: ProxyMode,
    proxy_id: Option<i64>,
    jump_host_id: Option<i64>,
    identity_id: Option<i64>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    query: Entity<InputState>,
    quick: Entity<InputState>,
    group_new: Entity<InputState>,
    /// 当前表单的认证方式（只存元数据，不存密码/私钥）。
    auth: AuthMethod,
    /// 选中主机后，下一帧把值填进输入框。
    pending_form: Option<FormValues>,
    /// 删除需要点两次；这里记着第一次点的是哪台。
    pending_delete: Option<i64>,
    /// 记忆面板：同一时间只开一个；切换主机或关掉时未保存的输入直接丢。
    memory: Option<MemoryEditor>,
    /// 有记忆的主机 id，只用来在列表里画「有记忆」小标记（不读正文）。
    memory_marks: BTreeSet<i64>,
    /// 最近一次「AI 使用中」快照；和进程级集合比对，只有变化才重绘。
    activity: Vec<i64>,
    status: String,
    focus: FocusHandle,
    _query_changed: Subscription,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
    _activity_tick: Task<()>,
}

impl HostsDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        let db = match open_database() {
            Ok(db) => Some(db),
            Err(err) => {
                tracing::warn!(%err, "打开主机库失败");
                None
            }
        };
        let focus = cx.focus_handle();
        let name = text_input(window, cx, t(HostsMsg::NamePlaceholder));
        let host = text_input(window, cx, t(HostsMsg::HostPlaceholder));
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("22")
                .placeholder(t(HostsMsg::PortPlaceholder))
        });
        let user = text_input(window, cx, t(HostsMsg::UsernamePlaceholder));
        let query = text_input(window, cx, t(HostsMsg::SearchPlaceholder));
        let quick = text_input(window, cx, "user@host:port");
        let group_new = text_input(window, cx, t(HostsMsg::NewGroupPlaceholder));
        let _query_changed = cx.observe(&query, |_this, _query, cx| cx.notify());
        let (_ime_focus, _ime_blur) = crate::ime::attach(cx, &focus, window, &ime_key);
        let mut this = Self {
            db,
            hosts: Vec::new(),
            groups: Vec::new(),
            proxies: Vec::new(),
            identities: Vec::new(),
            selected: None,
            group_id: None,
            proxy_mode: ProxyMode::Default,
            proxy_id: None,
            jump_host_id: None,
            identity_id: None,
            name,
            host,
            port,
            user,
            query,
            quick,
            group_new,
            auth: AuthMethod::Password,
            pending_form: None,
            pending_delete: None,
            memory: None,
            memory_marks: BTreeSet::new(),
            activity: crate::host_activity::snapshot(),
            status: String::new(),
            focus,
            _query_changed,
            _ime_focus,
            _ime_blur,
            _activity_tick: Task::ready(()),
        };
        if this.db.is_none() {
            this.status = t(HostsMsg::DbOpenFailed).to_owned();
        }
        this.reload(cx);
        // 「AI 使用中」标记：每秒对一次进程级快照，只有变化时才重绘；
        // 窗口关掉之后 `update_in` 会失败，任务自己退出。
        this._activity_tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(ACTIVITY_TICK).await;
                let alive = this.update_in(cx, |this, _window, cx| {
                    if this.refresh_host_activity() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        this
    }

    /// 拉一次进程级的「AI 使用中」快照；返回是否有变化（没有变化就不重绘）。
    fn refresh_host_activity(&mut self) -> bool {
        let snapshot = crate::host_activity::snapshot();
        if snapshot == self.activity {
            return false;
        }
        self.activity = snapshot;
        true
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(db) = &self.db else {
            return;
        };
        match db.host_list() {
            Ok(hosts) => {
                self.status = format!("{}{}", hosts.len(), t(HostsMsg::HostCountSuffix));
                self.hosts = hosts;
            }
            Err(err) => {
                self.status = format!(
                    "{}{}",
                    t(HostsMsg::ReadHostsFailedPrefix),
                    crate::error_text::error_text(&err)
                );
            }
        }
        match db.group_list() {
            Ok(groups) => self.groups = groups,
            Err(err) => {
                self.status = format!(
                    "{}{}",
                    t(HostsMsg::ReadGroupsFailedPrefix),
                    crate::error_text::error_text(&err)
                );
            }
        }
        match db.proxy_list() {
            Ok(proxies) => self.proxies = proxies,
            Err(err) => {
                self.status = format!(
                    "{}{}",
                    t(HostsMsg::ReadProxiesFailedPrefix),
                    crate::error_text::error_text(&err)
                );
            }
        }
        match db.identity_list() {
            Ok(identities) => self.identities = identities,
            Err(err) => {
                self.status = format!(
                    "{}{}",
                    t(HostsMsg::ReadIdentitiesFailedPrefix),
                    crate::error_text::error_text(&err)
                );
            }
        }
        if self
            .group_id
            .is_some_and(|id| !self.groups.iter().any(|group| group.id == id))
        {
            self.group_id = None;
        }
        if self.proxy_mode == ProxyMode::Custom
            && self
                .proxy_id
                .is_some_and(|id| !self.proxies.iter().any(|proxy| proxy.id == id))
        {
            self.proxy_mode = ProxyMode::Default;
            self.proxy_id = None;
        }
        if self
            .jump_host_id
            .is_some_and(|id| !self.hosts.iter().any(|host| host.id == id))
        {
            self.jump_host_id = None;
        }
        if self.identity_id.is_some_and(|id| {
            !self
                .identities
                .iter()
                .any(|identity| identity.id == id && identity_matches(self.auth, identity.kind))
        }) {
            self.identity_id = None;
        }
        // 面板开着的那台主机被删掉（或不在列表里）就顺手关掉，避免继续编辑不存在的行。
        if self.memory.as_ref().is_some_and(|editor| {
            !self
                .hosts
                .iter()
                .any(|host| host.id == editor.draft.host_id)
        }) {
            self.memory = None;
        }
        self.refresh_memory_marks();
        cx.notify();
    }

    /// 列表里的「有记忆」标记：只问有没有，不把正文读进内存。
    ///
    /// 每台主机一次单行查询（N 台 = N 次小查询）。列表只有搜索框做过滤，
    /// 过滤在渲染时按输入实时算，所以这里按全部主机查；主机多到刷新明显变慢时，
    /// 再到存储层加一条批量查询，不在这里拼 SQL。
    fn refresh_memory_marks(&mut self) {
        let Some(db) = &self.db else {
            self.memory_marks.clear();
            return;
        };
        let mut marks = BTreeSet::new();
        for host in &self.hosts {
            match db.host_memory_get(host.id) {
                Ok(Some(_)) => {
                    marks.insert(host.id);
                }
                Ok(None) => {}
                // 读不出一台不影响列表，只是这行没有标记；原因写日志。
                Err(err) => {
                    tracing::warn!(%err, host_id = host.id, "读取主机记忆标记失败");
                }
            }
        }
        self.memory_marks = marks;
    }

    fn select(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.iter().find(|host| host.id == id) else {
            return;
        };
        self.selected = Some(id);
        self.pending_form = Some(FormValues::from_host(host));
        self.auth = host.auth_method;
        self.group_id = host.group_id;
        self.proxy_mode = host.proxy_mode;
        self.proxy_id = host.proxy_id;
        self.jump_host_id = host.jump_host_id;
        self.identity_id = host.identity_id;
        self.pending_delete = None;
        // 换到别的主机才关记忆面板：未保存的输入直接丢，不弹确认（面板只是打开来看/改，
        // 真正的代价在「保存」那一步）；点同一行不动它。
        if self
            .memory
            .as_ref()
            .is_some_and(|editor| editor.draft.host_id != id)
        {
            self.memory = None;
        }
        cx.notify();
    }

    fn new_host(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.pending_form = Some(FormValues::new());
        self.auth = AuthMethod::Password;
        self.group_id = None;
        self.proxy_mode = ProxyMode::Default;
        self.proxy_id = None;
        self.jump_host_id = None;
        self.identity_id = None;
        self.pending_delete = None;
        self.memory = None;
        cx.notify();
    }

    fn set_auth(&mut self, auth: AuthMethod, cx: &mut Context<Self>) {
        self.auth = auth;
        // 换认证方式后旧凭据可能不再匹配，清掉让用户重选。
        if self.identity_id.is_some_and(|id| {
            !self
                .identities
                .iter()
                .any(|identity| identity.id == id && identity_matches(auth, identity.kind))
        }) {
            self.identity_id = None;
        }
        cx.notify();
    }

    /// 解析 `user@host:port` 并填进表单，由用户确认后保存。
    fn parse_quick(&mut self, cx: &mut Context<Self>) {
        let text = self.quick.read(cx).value().to_string();
        match zssh_core::model::parse_quick_target(&text) {
            Ok(target) => {
                self.selected = None;
                self.pending_delete = None;
                self.memory = None;
                self.auth = AuthMethod::Password;
                self.group_id = None;
                self.proxy_mode = ProxyMode::Default;
                self.proxy_id = None;
                self.jump_host_id = None;
                self.identity_id = None;
                self.pending_form = Some(form_from_quick(&target));
                self.status = format!(
                    "{}{}{}",
                    t(HostsMsg::QuickFilledPrefix),
                    target.display(),
                    t(HostsMsg::QuickFilledSuffix)
                );
            }
            Err(err) => self.status = err.to_string(),
        }
        cx.notify();
    }

    fn cycle_group(&mut self, cx: &mut Context<Self>) {
        self.group_id = next_group_id(self.group_id, &self.groups);
        cx.notify();
    }

    /// 在当前列表里上移/下移选中的主机，顺序写回存储层。
    fn move_host(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        let Some(index) = self.hosts.iter().position(|host| host.id == id) else {
            return;
        };
        let ids: Vec<i64> = self.hosts.iter().map(|host| host.id).collect();
        let Some(next) = reordered(&ids, index, delta) else {
            return;
        };
        let name = self.hosts[index].name.clone();
        let Some(db) = &self.db else {
            return;
        };
        match db.host_reorder(&next) {
            Ok(()) => {
                self.status = format!(
                    "{}{name}{}",
                    t(HostsMsg::OrderAdjustedPrefix),
                    t(HostsMsg::OrderAdjustedSuffix)
                );
                self.reload(cx);
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    fn create_group(&mut self, cx: &mut Context<Self>) {
        let Some(db) = &self.db else {
            return;
        };
        let name = self.group_new.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.status = t(HostsMsg::GroupNameEmpty).to_owned();
            cx.notify();
            return;
        }
        match db.group_save(&GroupDraft {
            id: None,
            name,
            parent_id: None,
            sort: 0,
        }) {
            Ok(group) => {
                self.group_id = Some(group.id);
                self.status = format!("{}{}", t(HostsMsg::GroupCreatedPrefix), group.name);
                self.reload(cx);
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    fn extras(&self) -> HostExtras {
        HostExtras {
            group_id: self.group_id,
            proxy_mode: self.proxy_mode,
            proxy_id: self.proxy_id,
            jump_host_id: self.jump_host_id,
            identity_id: self.identity_id,
        }
    }

    /// 凭据按钮循环：无 → 匹配当前认证方式的身份依次 → 无。
    fn cycle_identity(&mut self, cx: &mut Context<Self>) {
        self.identity_id = next_identity(self.identity_id, self.auth, &self.identities);
        cx.notify();
    }

    /// 代理按钮循环：默认 → 直连 → 各代理 → 默认。
    fn cycle_proxy(&mut self, cx: &mut Context<Self>) {
        (self.proxy_mode, self.proxy_id) =
            next_proxy(self.proxy_mode, self.proxy_id, &self.proxies);
        cx.notify();
    }

    /// 跳板机按钮循环：无 → 其他主机依次 → 无。
    fn cycle_jump(&mut self, cx: &mut Context<Self>) {
        self.jump_host_id = next_jump(self.jump_host_id, &self.hosts, self.selected);
        cx.notify();
    }

    /// 打开代理库窗口；关闭后刷新本窗口的代理列表。
    fn open_proxies_window(&mut self, cx: &mut Context<Self>) {
        let n = NEXT_PROXIES.fetch_add(1, Ordering::Relaxed);
        let key = format!("proxies-{n}");
        let close_key = key.clone();
        let hosts = cx.entity().downgrade();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(
                size(::gpui::px(760.), ::gpui::px(540.)),
                cx,
            )),
            ..crate::window_frame::app_options()
        };
        let _ = cx.open_window(options, move |window, cx| {
            let view = cx.new(|cx| crate::proxies::ProxiesDemo::new(window, cx, key));
            window.on_window_should_close(cx, move |_window, cx| {
                let _ = hosts.update(cx, |this, cx| this.reload(cx));
                crate::ime::window_destroyed(&close_key);
                true
            });
            window.focus(&view.read(cx).focus_handle(), cx);
            view
        });
    }

    /// 打开身份库窗口。
    fn open_identities_window(&mut self, cx: &mut Context<Self>) {
        let n = NEXT_IDENTITIES.fetch_add(1, Ordering::Relaxed);
        let key = format!("identities-{n}");
        let close_key = key.clone();
        let hosts = cx.entity().downgrade();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(
                size(::gpui::px(760.), ::gpui::px(540.)),
                cx,
            )),
            ..crate::window_frame::app_options()
        };
        let _ = cx.open_window(options, move |window, cx| {
            let view = cx.new(|cx| crate::identities::IdentitiesDemo::new(window, cx, key));
            window.on_window_should_close(cx, move |_window, cx| {
                let _ = hosts.update(cx, |this, cx| this.reload(cx));
                crate::ime::window_destroyed(&close_key);
                true
            });
            window.focus(&view.read(cx).focus_handle(), cx);
            view
        });
    }

    fn form_values(&self, cx: &App) -> FormValues {
        FormValues {
            name: self.name.read(cx).value().to_string(),
            host: self.host.read(cx).value().to_string(),
            port: self.port.read(cx).value().to_string(),
            user: self.user.read(cx).value().to_string(),
            auth: self.auth,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(db) = &self.db else {
            return;
        };
        // 已有主机：先读出库里现在的样子，表单里没有的字段（标签、老算法开关、排序）带过去。
        let existing = match self.selected {
            Some(id) => match db.host_get(id) {
                Ok(host) => Some(host),
                Err(err) => {
                    self.status = format!(
                        "{}{}",
                        t(HostsMsg::ReadHostFailedNoSavePrefix),
                        crate::error_text::error_text(&err)
                    );
                    cx.notify();
                    return;
                }
            },
            None => None,
        };
        let draft = keep_hidden_fields(
            draft_from(&self.form_values(cx), self.selected, self.extras()),
            existing.as_ref(),
        );
        match db.host_save(&draft) {
            Ok(host) => {
                self.selected = Some(host.id);
                self.auth = host.auth_method;
                self.group_id = host.group_id;
                self.proxy_mode = host.proxy_mode;
                self.proxy_id = host.proxy_id;
                self.jump_host_id = host.jump_host_id;
                self.identity_id = host.identity_id;
                self.pending_delete = None;
                self.status = format!("{}{}", t(HostsMsg::SavedPrefix), host.name);
                self.reload(cx);
                self.pending_form = Some(FormValues::from_host(&host));
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            self.status = t(HostsMsg::SelectHostConnect).to_owned();
            cx.notify();
            return;
        };
        // 演示窗连的是旧终端演示窗口（terminal_demo，只编进 debug）；主窗口的正式连接
        // 走 main_window 自己的 host_open，不经过这里。
        #[cfg(debug_assertions)]
        crate::terminal_demo::open_ssh_window(cx, id);
        #[cfg(not(debug_assertions))]
        {
            let _ = id;
        }
    }

    /// 打开选中主机的远端文件浏览窗口（复用同一条已认证连接）。
    fn open_sftp(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            self.status = t(HostsMsg::SelectHostFile).to_owned();
            cx.notify();
            return;
        };
        crate::sftp::open_sftp_window(cx, id);
    }

    /// 打开选中主机的记忆面板：先从库里读现有正文，再填进文本框。
    fn open_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            self.status = t(HostsMsg::SelectHostMemory).to_owned();
            cx.notify();
            return;
        };
        // 同一台已经开着：保留未保存的输入，再点一次只是不重新读库。
        if self
            .memory
            .as_ref()
            .is_some_and(|editor| editor.draft.host_id == id)
        {
            cx.notify();
            return;
        }
        let Some(name) = self
            .hosts
            .iter()
            .find(|host| host.id == id)
            .map(|host| host.name.clone())
        else {
            return;
        };
        let Some(db) = &self.db else {
            self.status = t(HostsMsg::DbOpenFailed).to_owned();
            cx.notify();
            return;
        };
        let (content, updated_at) = match db.host_memory_get(id) {
            Ok(Some(memory)) => (memory.content, Some(memory.updated_at)),
            Ok(None) => (String::new(), None),
            Err(err) => {
                self.status = format!(
                    "{}{}",
                    t(HostsMsg::ReadMemoryFailedPrefix),
                    crate::error_text::error_text(&err)
                );
                cx.notify();
                return;
            }
        };
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(t(HostsMsg::MemoryPlaceholder))
                .auto_grow(4, 10)
                .default_value(content)
        });
        self.memory = Some(MemoryEditor {
            draft: MemoryDraft::open(id, updated_at),
            host_name: name.clone(),
            input,
        });
        self.status = format!(
            "{}{name}{}",
            t(HostsMsg::EditingMemoryPrefix),
            t(HostsMsg::EditingMemorySuffix)
        );
        cx.notify();
    }

    /// 关掉面板：没保存的输入直接丢，不弹确认。
    fn close_memory(&mut self, cx: &mut Context<Self>) {
        self.memory = None;
        self.status = t(HostsMsg::MemoryPanelClosed).to_owned();
        cx.notify();
    }

    /// 面板里的错误照原文显示（`clean_memory` 的中文提示、存储层错误都一样）。
    fn memory_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.memory {
            editor.draft.error = Some(error);
            editor.draft.notice = None;
        }
        cx.notify();
    }

    /// 「保存」：先 `clean_memory`，空白等于清空；写库成功后刷新面板上的更新时间。
    fn save_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.memory else {
            return;
        };
        let host_id = editor.draft.host_id;
        let host_name = editor.host_name.clone();
        let input = editor.input.clone();
        let text = editor.input.read(cx).value().to_string();
        let action = match memory_action(&text) {
            Ok(action) => action,
            Err(err) => {
                self.memory_error(err, cx);
                return;
            }
        };
        let Some(db) = &self.db else {
            self.memory_error(t(HostsMsg::DbOpenFailed).to_owned(), cx);
            return;
        };
        let written = match &action {
            MemoryAction::Save(content) => db.host_memory_set(host_id, content),
            MemoryAction::Clear => db.host_memory_set(host_id, ""),
        };
        if let Err(err) = written {
            self.memory_error(err.to_string(), cx);
            return;
        }
        let updated_at = match db.host_memory_get(host_id) {
            Ok(memory) => memory.map(|memory| memory.updated_at),
            Err(err) => {
                // 写进去了，只是读不回时间：按刚写入 / 刚清空继续显示，原因写日志。
                tracing::warn!(%err, host_id, "保存后读回记忆失败");
                match &action {
                    MemoryAction::Save(_) => Some(now_unix()),
                    MemoryAction::Clear => None,
                }
            }
        };
        if let Some(editor) = &mut self.memory {
            let notice = match &action {
                MemoryAction::Save(_) => t(HostsMsg::MemorySaved),
                MemoryAction::Clear => t(HostsMsg::MemoryCleared),
            };
            editor.draft.saved(updated_at, notice);
        }
        if updated_at.is_some() {
            self.memory_marks.insert(host_id);
        } else {
            self.memory_marks.remove(&host_id);
        }
        // 文本框换成清理后的正文：藏掉的密码不会再留在界面上，也避免下次照原样写回去。
        let shown = match action {
            MemoryAction::Save(content) => content,
            MemoryAction::Clear => String::new(),
        };
        let cleared = shown.is_empty();
        input.update(cx, |state, cx| state.set_value(shown, window, cx));
        self.status = if cleared {
            format!(
                "{}{host_name}{}",
                t(HostsMsg::ClearedMemoryPrefix),
                t(HostsMsg::ClearedMemorySuffix)
            )
        } else {
            format!(
                "{}{host_name}{}",
                t(HostsMsg::SavedPrefix),
                t(HostsMsg::ClearedMemorySuffix)
            )
        };
        cx.notify();
    }

    /// 「清空」：点两次才真清空，清完把文本框也置空，免得再按保存又写回去。
    fn clear_memory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.memory else {
            return;
        };
        let host_id = editor.draft.host_id;
        let host_name = editor.host_name.clone();
        let input = editor.input.clone();
        if !editor.draft.click_clear() {
            self.status = format!(
                "{}{host_name}{}",
                t(HostsMsg::ClearAgainPrefix),
                t(HostsMsg::ClearAgainSuffix)
            );
            cx.notify();
            return;
        }
        let Some(db) = &self.db else {
            self.memory_error(t(HostsMsg::DbOpenFailed).to_owned(), cx);
            return;
        };
        match db.host_memory_set(host_id, "") {
            Ok(()) => {
                input.update(cx, |state, cx| state.set_value("", window, cx));
                if let Some(editor) = &mut self.memory {
                    editor.draft.saved(None, t(HostsMsg::MemoryCleared));
                }
                self.memory_marks.remove(&host_id);
                self.status = format!(
                    "{}{host_name}{}",
                    t(HostsMsg::ClearedMemoryPrefix),
                    t(HostsMsg::ClearedMemorySuffix)
                );
                cx.notify();
            }
            Err(err) => self.memory_error(err.to_string(), cx),
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let (Some(db), Some(id)) = (&self.db, self.selected) else {
            return;
        };
        let name = self
            .hosts
            .iter()
            .find(|host| host.id == id)
            .map(|host| host.name.clone())
            .unwrap_or_default();
        if self.pending_delete != Some(id) {
            self.pending_delete = Some(id);
            // 读库里最新的主机和转发规则（窗口里的列表可能是旧的）；读不出来就只按列表算。
            let hosts = db.host_list().unwrap_or_else(|_| self.hosts.clone());
            let forwards = db.forward_list().unwrap_or_default();
            self.status = host_delete_prompt(&name, &host_delete_impact(&hosts, &forwards, id));
            cx.notify();
            return;
        }
        match db.host_delete(id) {
            Ok(()) => {
                self.selected = None;
                self.pending_delete = None;
                self.memory = None;
                self.pending_form = Some(FormValues::new());
                self.reload(cx);
                self.status = format!("{}{}", t(HostsMsg::DeletedPrefix), name);
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let up_entity = entity.clone();
        let down_entity = entity.clone();
        let query = self.query.read(cx).value().to_string();
        v_flex()
            .w(::gpui::px(360.0))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .h(::gpui::px(36.0))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .child(Input::new(&self.query).w(::gpui::px(258.0)))
                    .child(Button::new("host-up").label("\u{2191}").compact().on_click(
                        move |_, _, cx| {
                            up_entity.update(cx, |this, cx| this.move_host(-1, cx));
                        },
                    ))
                    .child(
                        Button::new("host-down")
                            .label("\u{2193}")
                            .compact()
                            .on_click(move |_, _, cx| {
                                down_entity.update(cx, |this, cx| this.move_host(1, cx));
                            }),
                    ),
            )
            .children(
                self.hosts
                    .iter()
                    .filter(|host| matches_query(host, &query))
                    .map(|host| {
                        let select_entity = entity.clone();
                        let id = host.id;
                        let selected = self.selected == Some(id);
                        let has_memory = self.memory_marks.contains(&id);
                        // AI 正在这台主机上跑真实回合：行上给一个小标记。
                        // 只读进程级集合，`self.activity` 负责判断要不要重绘。
                        let activity = crate::host_activity::activity_label(
                            crate::host_activity::is_active(id),
                        );
                        h_flex()
                            .id(("host", id as usize))
                            .h(::gpui::px(30.0))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .bg(if selected {
                                cx.theme().list_active
                            } else {
                                cx.theme().background
                            })
                            .child(div().text_sm().child(host.name.clone()))
                            .when_some(activity, |row, label| {
                                row.child(
                                    div().text_xs().text_color(cx.theme().success).child(label),
                                )
                            })
                            .when(has_memory, |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(t(HostsMsg::HasMemoryMark)),
                                )
                            })
                            .child(
                                div().text_xs().child(format!(
                                    "{}@{}:{}",
                                    host.username, host.host, host.port
                                )),
                            )
                            .on_click(move |_, _, cx| {
                                select_entity.update(cx, |this, cx| this.select(id, cx));
                            })
                    }),
            )
    }

    fn form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let password_entity = cx.entity();
        let key_entity = cx.entity();
        let agent_entity = cx.entity();
        let group_cycle_entity = cx.entity();
        let group_create_entity = cx.entity();
        let proxy_cycle_entity = cx.entity();
        let jump_cycle_entity = cx.entity();
        let identity_cycle_entity = cx.entity();
        let group_label = self
            .group_id
            .and_then(|id| self.groups.iter().find(|group| group.id == id))
            .map(|group| group.name.clone())
            .unwrap_or_else(|| t(HostsMsg::NoGroup).to_owned());
        let identity_label = self
            .identity_id
            .and_then(|id| self.identities.iter().find(|identity| identity.id == id))
            .map(|identity| identity.name.clone())
            .unwrap_or_else(|| t(HostsMsg::NoneLabel).to_owned());
        let proxy_label = match self.proxy_mode {
            ProxyMode::Default => t(HostsMsg::DefaultProxy).to_owned(),
            ProxyMode::None => t(HostsMsg::DirectProxy).to_owned(),
            ProxyMode::Custom => self
                .proxy_id
                .and_then(|id| self.proxies.iter().find(|proxy| proxy.id == id))
                .map(|proxy| proxy.name.clone())
                .unwrap_or_else(|| t(HostsMsg::ProxyFallback).to_owned()),
        };
        let jump_label = self
            .jump_host_id
            .and_then(|id| self.hosts.iter().find(|host| host.id == id))
            .map(|host| host.name.clone())
            .unwrap_or_else(|| t(HostsMsg::NoneLabel).to_owned());
        v_flex()
            .flex_1()
            .p_4()
            .gap_2()
            .child(field(t(HostsMsg::NamePlaceholder), &self.name))
            .child(field(t(HostsMsg::HostPlaceholder), &self.host))
            .child(field(t(HostsMsg::PortPlaceholder), &self.port))
            .child(field(t(HostsMsg::UsernamePlaceholder), &self.user))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(::gpui::px(56.0))
                            .text_sm()
                            .child(t(HostsMsg::AuthLabel)),
                    )
                    .child(
                        Button::new("auth-password")
                            .label(t(HostsMsg::PasswordAuth))
                            .compact()
                            .toggled(self.auth == AuthMethod::Password)
                            .on_click(move |_, _, cx| {
                                password_entity.update(cx, |this, cx| {
                                    this.set_auth(AuthMethod::Password, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("auth-key")
                            .label(t(HostsMsg::KeyAuth))
                            .compact()
                            .toggled(self.auth == AuthMethod::Key)
                            .on_click(move |_, _, cx| {
                                key_entity.update(cx, |this, cx| {
                                    this.set_auth(AuthMethod::Key, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("auth-agent")
                            .label("Agent")
                            .compact()
                            .toggled(self.auth == AuthMethod::Agent)
                            .on_click(move |_, _, cx| {
                                agent_entity.update(cx, |this, cx| {
                                    this.set_auth(AuthMethod::Agent, cx);
                                });
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(::gpui::px(56.0))
                            .text_sm()
                            .child(t(HostsMsg::IdentityLabel)),
                    )
                    .child(
                        Button::new("identity-cycle")
                            .label(identity_label)
                            .compact()
                            .on_click(move |_, _, cx| {
                                identity_cycle_entity
                                    .update(cx, |this, cx| this.cycle_identity(cx));
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(::gpui::px(56.0))
                            .text_sm()
                            .child(t(HostsMsg::GroupLabel)),
                    )
                    .child(
                        Button::new("group-cycle")
                            .label(group_label)
                            .compact()
                            .on_click(move |_, _, cx| {
                                group_cycle_entity.update(cx, |this, cx| this.cycle_group(cx));
                            }),
                    )
                    .child(Input::new(&self.group_new).w(::gpui::px(140.0)))
                    .child(
                        Button::new("group-create")
                            .label(t(HostsMsg::CreateGroup))
                            .compact()
                            .on_click(move |_, _, cx| {
                                group_create_entity.update(cx, |this, cx| this.create_group(cx));
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(::gpui::px(56.0))
                            .text_sm()
                            .child(t(HostsMsg::ProxyLabel)),
                    )
                    .child(
                        Button::new("proxy-cycle")
                            .label(proxy_label)
                            .compact()
                            .on_click(move |_, _, cx| {
                                proxy_cycle_entity.update(cx, |this, cx| this.cycle_proxy(cx));
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(::gpui::px(56.0))
                            .text_sm()
                            .child(t(HostsMsg::JumpLabel)),
                    )
                    .child(
                        Button::new("jump-cycle")
                            .label(jump_label)
                            .compact()
                            .on_click(move |_, _, cx| {
                                jump_cycle_entity.update(cx, |this, cx| this.cycle_jump(cx));
                            }),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(HostsMsg::AuthHint)),
            )
    }

    /// 选中主机的记忆面板：没有打开时返回 `None`。
    fn memory_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let editor = self.memory.as_ref()?;
        let save_entity = cx.entity();
        let clear_entity = cx.entity();
        let cancel_entity = cx.entity();
        let error = editor.draft.error.clone();
        let notice = editor.draft.notice.clone();
        Some(
            v_flex()
                .p_4()
                .gap_2()
                .flex_none()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_sm().child(format!(
                            "{}{}",
                            t(HostsMsg::MemoryPanelPrefix),
                            editor.host_name
                        )))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(memory_summary(editor.draft.updated_at, now_unix())),
                        ),
                )
                .child(Textarea::new(&editor.input).h(::gpui::px(120.0)))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Button::new("memory-save")
                                .label(t(HostsMsg::Save))
                                .compact()
                                .on_click(move |_, window, cx| {
                                    save_entity.update(cx, |this, cx| this.save_memory(window, cx));
                                }),
                        )
                        .child(
                            Button::new("memory-clear")
                                .label(editor.draft.clear_label())
                                .compact()
                                .on_click(move |_, window, cx| {
                                    clear_entity
                                        .update(cx, |this, cx| this.clear_memory(window, cx));
                                }),
                        )
                        .child(
                            Button::new("memory-cancel")
                                .label(t(HostsMsg::Cancel))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cancel_entity.update(cx, |this, cx| this.close_memory(cx));
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{}{MAX_MEMORY_CHARS}{}",
                                    t(HostsMsg::MaxCharsPrefix),
                                    t(HostsMsg::MaxCharsMiddle)
                                )),
                        ),
                )
                .children(
                    error.map(|error| div().text_xs().text_color(cx.theme().danger).child(error)),
                )
                .children(
                    notice
                        .map(|notice| div().text_xs().text_color(cx.theme().success).child(notice)),
                ),
        )
    }
}

fn field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(div().w(::gpui::px(56.0)).text_sm().child(label))
        .child(Input::new(input).w(::gpui::px(300.0)))
}

fn text_input(
    window: &mut Window,
    cx: &mut Context<HostsDemo>,
    placeholder: &'static str,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

/// 打开主机库窗口；同一个进程可以开多个，IME 按窗口 key 管理。
pub(crate) fn open_hosts_window(cx: &mut App) {
    let n = NEXT_HOSTS.fetch_add(1, Ordering::Relaxed);
    let bounds = crate::window_state::restore_hosts(
        &crate::window_state::WindowState::load(),
        &crate::window_state::display_rects(cx),
    )
    .unwrap_or_else(|| WindowBounds::centered(size(::gpui::px(820.), ::gpui::px(650.)), cx));
    let options = WindowOptions {
        window_bounds: Some(bounds),
        ..crate::window_frame::app_options()
    };
    let key = format!("hosts-{n}");
    let close_key = key.clone();
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| HostsDemo::new(window, cx, key));
        window.on_window_should_close(cx, move |window, _cx| {
            let mut state = crate::window_state::WindowState::load();
            state.hosts = Some(crate::window_state::WindowState::capture(window));
            state.save();
            crate::ime::window_destroyed(&close_key);
            true
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

impl Render for HostsDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(values) = self.pending_form.take() {
            self.name = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.name)
                    .placeholder(t(HostsMsg::NamePlaceholder))
            });
            self.host = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.host)
                    .placeholder(t(HostsMsg::HostPlaceholder))
            });
            self.port = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.port)
                    .placeholder(t(HostsMsg::PortPlaceholder))
            });
            self.user = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.user)
                    .placeholder(t(HostsMsg::UsernamePlaceholder))
            });
        }

        let entity = cx.entity();
        let new_entity = entity.clone();
        let save_entity = entity.clone();
        let connect_entity = entity.clone();
        let sftp_entity = entity.clone();
        let memory_entity = entity.clone();
        let delete_entity = entity.clone();
        let quick_entity = entity.clone();
        let proxies_entity = entity.clone();
        let identities_entity = entity.clone();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(::gpui::px(44.0))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().child(t(HostsMsg::Title)))
                    .child(Button::new("host-new").label(t(HostsMsg::New)).on_click(
                        move |_, _, cx| {
                            new_entity.update(cx, |this, cx| this.new_host(cx));
                        },
                    ))
                    .child(Button::new("host-save").label(t(HostsMsg::Save)).on_click(
                        move |_, _, cx| {
                            save_entity.update(cx, |this, cx| this.save(cx));
                        },
                    ))
                    .child(
                        Button::new("host-connect")
                            .label(t(HostsMsg::Connect))
                            .on_click(move |_, _, cx| {
                                connect_entity.update(cx, |this, cx| this.connect(cx));
                            }),
                    )
                    .child(
                        Button::new("host-files")
                            .label(t(HostsMsg::Files))
                            .on_click(move |_, _, cx| {
                                sftp_entity.update(cx, |this, cx| this.open_sftp(cx));
                            }),
                    )
                    .child(
                        Button::new("host-memory")
                            .label(t(HostsMsg::Memory))
                            .on_click(move |_, window, cx| {
                                memory_entity.update(cx, |this, cx| this.open_memory(window, cx));
                            }),
                    )
                    .child(
                        Button::new("host-delete")
                            .label(
                                if self.pending_delete.is_some()
                                    && self.pending_delete == self.selected
                                {
                                    t(HostsMsg::ConfirmDelete)
                                } else {
                                    t(HostsMsg::Delete)
                                },
                            )
                            .on_click(move |_, _, cx| {
                                delete_entity.update(cx, |this, cx| this.delete(cx));
                            }),
                    )
                    .child(Input::new(&self.quick).w(::gpui::px(160.0)))
                    .child(
                        Button::new("quick-parse")
                            .label(t(HostsMsg::Parse))
                            .on_click(move |_, _, cx| {
                                quick_entity.update(cx, |this, cx| this.parse_quick(cx));
                            }),
                    )
                    .child(
                        Button::new("proxies-window")
                            .label(t(HostsMsg::Proxies))
                            .on_click(move |_, _, cx| {
                                proxies_entity.update(cx, |this, cx| this.open_proxies_window(cx));
                            }),
                    )
                    .child(
                        Button::new("identities-window")
                            .label(t(HostsMsg::Identities))
                            .on_click(move |_, _, cx| {
                                identities_entity
                                    .update(cx, |this, cx| this.open_identities_window(cx));
                            }),
                    )
                    .child(div().text_sm().child(self.status.clone())),
            )
            .child(
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.list(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .overflow_hidden()
                            .children(self.memory_panel(cx))
                            .child(self.form(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FormValues, HostExtras, MemoryAction, MemoryDraft, draft_from, form_from_quick,
        identity_matches, matches_query, memory_action, memory_summary, next_group_id,
        next_identity, next_jump, next_proxy, reordered,
    };
    use super::{HostDeleteImpact, host_delete_impact, host_delete_prompt, keep_hidden_fields};
    use zssh_core::ai::host_memory::{MAX_MEMORY_CHARS, clean_memory};
    use zssh_core::model::{
        AuthMethod, Group, Host, HostDraft, Identity, IdentityKind, Proxy, ProxyKind, ProxyMode,
    };
    use zssh_core::store::Database;

    #[test]
    fn port_defaults_and_invalid_values() {
        let values = FormValues::new();
        assert_eq!(draft_from(&values, None, HostExtras::default()).port, 22);

        let values = FormValues {
            port: " 2222 ".to_owned(),
            ..FormValues::new()
        };
        assert_eq!(draft_from(&values, None, HostExtras::default()).port, 2222);

        let values = FormValues {
            port: "abc".to_owned(),
            ..FormValues::new()
        };
        assert_eq!(draft_from(&values, None, HostExtras::default()).port, 0);
    }

    #[test]
    fn form_values_are_trimmed_and_kept() {
        let values = FormValues {
            name: " web-1 ".to_owned(),
            host: " 10.0.0.5 ".to_owned(),
            port: "22".to_owned(),
            user: " root ".to_owned(),
            auth: AuthMethod::Password,
        };
        let draft = draft_from(&values, Some(7), HostExtras::default());
        assert_eq!(draft.id, Some(7));
        assert_eq!(draft.name, "web-1");
        assert_eq!(draft.host, "10.0.0.5");
        assert_eq!(draft.username, "root");
        assert_eq!(draft.auth_method, AuthMethod::Password);
    }

    #[test]
    fn selected_auth_method_is_stored() {
        let values = FormValues {
            auth: AuthMethod::Key,
            ..FormValues::new()
        };
        assert_eq!(
            draft_from(&values, None, HostExtras::default()).auth_method,
            AuthMethod::Key
        );
    }

    #[test]
    fn reorder_swaps_and_rejects_edges() {
        assert_eq!(reordered(&[1, 2, 3], 1, -1), Some(vec![2, 1, 3]));
        assert_eq!(reordered(&[1, 2, 3], 1, 1), Some(vec![1, 3, 2]));
        assert_eq!(reordered(&[1, 2, 3], 0, -1), None);
        assert_eq!(reordered(&[1, 2, 3], 2, 1), None);
        assert_eq!(reordered(&[], 0, 1), None);
    }

    #[test]
    fn identity_matches_auth_methods() {
        assert!(identity_matches(
            AuthMethod::Password,
            IdentityKind::Password
        ));
        assert!(!identity_matches(
            AuthMethod::Password,
            IdentityKind::KeyFile
        ));
        assert!(identity_matches(AuthMethod::Key, IdentityKind::KeyFile));
        assert!(identity_matches(AuthMethod::Key, IdentityKind::KeyInline));
        assert!(!identity_matches(AuthMethod::Key, IdentityKind::Password));
        assert!(identity_matches(AuthMethod::Agent, IdentityKind::Password));
    }

    #[test]
    fn identity_cycle_filters_by_auth() {
        let make = |id: i64, name: &str, kind: IdentityKind| Identity {
            id,
            name: name.to_owned(),
            kind,
            key_path: None,
            has_secret: true,
        };
        let list = vec![
            make(1, "pw", IdentityKind::Password),
            make(2, "key1", IdentityKind::KeyFile),
            make(3, "key2", IdentityKind::KeyInline),
        ];
        assert_eq!(next_identity(None, AuthMethod::Key, &list), Some(2));
        assert_eq!(next_identity(Some(2), AuthMethod::Key, &list), Some(3));
        assert_eq!(next_identity(Some(3), AuthMethod::Key, &list), None);
        assert_eq!(next_identity(None, AuthMethod::Password, &list), Some(1));
        assert_eq!(next_identity(Some(1), AuthMethod::Password, &list), None);
    }

    #[test]
    fn proxy_cycles_default_none_custom() {
        let proxies = vec![Proxy {
            id: 3,
            name: "corp".to_owned(),
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".to_owned(),
            port: 1080,
            username: None,
            has_secret: false,
        }];
        assert_eq!(
            next_proxy(ProxyMode::Default, None, &proxies),
            (ProxyMode::None, None)
        );
        assert_eq!(
            next_proxy(ProxyMode::None, None, &proxies),
            (ProxyMode::Custom, Some(3))
        );
        assert_eq!(
            next_proxy(ProxyMode::Custom, Some(3), &proxies),
            (ProxyMode::Default, None)
        );
        let empty: Vec<Proxy> = Vec::new();
        assert_eq!(
            next_proxy(ProxyMode::None, None, &empty),
            (ProxyMode::Default, None)
        );
    }

    #[test]
    fn jump_cycles_other_hosts() {
        let make = |id: i64, name: &str| Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: None,
            name: name.to_owned(),
            host: "10.0.0.1".to_owned(),
            port: 22,
            username: "root".to_owned(),
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
        };
        let hosts = vec![make(1, "a"), make(2, "b"), make(3, "c")];
        assert_eq!(next_jump(None, &hosts, Some(1)), Some(2));
        assert_eq!(next_jump(Some(2), &hosts, Some(1)), Some(3));
        assert_eq!(next_jump(Some(3), &hosts, Some(1)), None);
        assert_eq!(next_jump(None, &hosts, None), Some(1));
    }

    #[test]
    fn group_cycles_from_none_to_first_to_none() {
        let groups = vec![
            Group {
                id: 1,
                name: "prod".to_owned(),
                parent_id: None,
                sort: 0,
            },
            Group {
                id: 2,
                name: "dev".to_owned(),
                parent_id: None,
                sort: 1,
            },
        ];
        assert_eq!(next_group_id(None, &groups), Some(1));
        assert_eq!(next_group_id(Some(1), &groups), Some(2));
        assert_eq!(next_group_id(Some(2), &groups), None);
        assert_eq!(next_group_id(None, &[]), None);
    }

    #[test]
    fn draft_keeps_group_id() {
        let values = FormValues::new();
        assert_eq!(
            draft_from(
                &values,
                None,
                HostExtras {
                    group_id: Some(9),
                    ..Default::default()
                },
            )
            .group_id,
            Some(9)
        );
    }

    #[test]
    fn quick_target_fills_the_form() {
        let target = zssh_core::model::parse_quick_target("root@example.com:2222").unwrap();
        let values = form_from_quick(&target);
        assert_eq!(values.name, "example.com");
        assert_eq!(values.host, "example.com");
        assert_eq!(values.port, "2222");
        assert_eq!(values.user, "root");
        assert_eq!(draft_from(&values, None, HostExtras::default()).port, 2222);
    }

    #[test]
    fn search_matches_name_address_user_and_port() {
        let host = Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id: 1,
            group_id: None,
            name: "web-1".to_owned(),
            host: "10.0.0.5".to_owned(),
            port: 2222,
            username: "root".to_owned(),
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
        };
        assert!(matches_query(&host, ""));
        assert!(matches_query(&host, "WEB"));
        assert!(matches_query(&host, "0.0.5"));
        assert!(matches_query(&host, "root"));
        assert!(matches_query(&host, "2222"));
        assert!(!matches_query(&host, "db"));
    }

    fn sample_host_draft() -> HostDraft {
        HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: "web-1".to_owned(),
            host: "10.0.0.5".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        }
    }

    #[test]
    fn blank_or_whitespace_memory_means_clear() {
        assert_eq!(memory_action(""), Ok(MemoryAction::Clear));
        assert_eq!(memory_action(" \n\t "), Ok(MemoryAction::Clear));
        assert_eq!(
            memory_action("  服务：nginx、redis  \n"),
            Ok(MemoryAction::Save("服务：nginx、redis".to_owned()))
        );
    }

    #[test]
    fn core_clean_memory_refuses_too_long_text_and_hides_passwords() {
        let long = "x".repeat(MAX_MEMORY_CHARS + 1);
        let err = clean_memory(&long).unwrap_err();
        assert!(err.contains(&MAX_MEMORY_CHARS.to_string()), "{err}");

        let cleaned = clean_memory("db password: hunter2").unwrap();
        assert_eq!(cleaned, "db password: [已隐藏]");
    }

    #[test]
    fn host_memory_round_trips_through_the_database() {
        let db = Database::open_in_memory().unwrap();
        let host = db.host_save(&sample_host_draft()).unwrap();
        assert_eq!(db.host_memory_get(host.id).unwrap(), None);

        let Ok(MemoryAction::Save(content)) = memory_action("  服务：nginx  \n") else {
            panic!("非空输入应该走保存");
        };
        db.host_memory_set(host.id, &content).unwrap();
        let stored = db.host_memory_get(host.id).unwrap().unwrap();
        assert_eq!(stored.content, "服务：nginx");

        db.host_memory_set(host.id, "").unwrap();
        assert_eq!(db.host_memory_get(host.id).unwrap(), None);
    }

    #[test]
    fn memory_panel_arms_before_clearing() {
        let mut draft = MemoryDraft::open(7, Some(1_000));
        assert_eq!(draft.clear_label(), "清空");
        assert!(!draft.click_clear());
        assert_eq!(draft.clear_label(), "确认清空");
        assert!(draft.click_clear());

        // 清空成功后按钮回到「清空」，时间也变成「还没有记忆」。
        draft.saved(None, "已清空");
        assert_eq!(draft.clear_label(), "清空");
        assert_eq!(draft.updated_at, None);
        assert_eq!(draft.notice.as_deref(), Some("已清空"));
        assert!(draft.error.is_none());
    }

    #[test]
    fn opening_the_panel_starts_a_clean_draft() {
        let mut draft = MemoryDraft::open(1, Some(9_000));
        assert!(!draft.click_clear());
        draft.error = Some("记忆太长".to_owned());
        draft.notice = Some("已保存".to_owned());

        // 换一台主机 = 重新开一个：确认状态、错误、旧提示都不跟过来。
        let reopened = MemoryDraft::open(2, None);
        assert_eq!(
            reopened,
            MemoryDraft {
                host_id: 2,
                updated_at: None,
                clear_armed: false,
                error: None,
                notice: None,
            }
        );

        // 关面板 = `Option<MemoryDraft>` 置空，没有单独的草稿要清理。
        let mut slot = Some(draft);
        assert!(slot.take().is_some());
        assert!(slot.is_none());
    }

    #[test]
    fn memory_summary_is_relative_time_or_empty_hint() {
        let now = 1_000_000;
        assert_eq!(memory_summary(None, now), "还没有记忆");
        assert_eq!(memory_summary(Some(now), now), "更新于 刚刚");
        assert_eq!(memory_summary(Some(now - 300), now), "更新于 5 分钟前");
        assert_eq!(memory_summary(Some(now - 7_200), now), "更新于 2 小时前");
        assert_eq!(memory_summary(Some(now - 3 * 86_400), now), "更新于 3 天前");
        // 时钟回拨不该显示负数。
        assert_eq!(memory_summary(Some(now + 600), now), "更新于 刚刚");
    }

    fn sample_host(id: i64, name: &str, jump: Option<i64>) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: None,
            name: name.to_owned(),
            host: format!("10.0.0.{id}"),
            port: 22,
            username: "root".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: jump,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// X1：先存一台带标签、旧算法开关、排序的主机，经表单只改名保存，三样都还在。
    #[test]
    fn saving_through_the_form_keeps_tags_legacy_flag_and_sort() {
        let db = Database::open_in_memory().expect("内存库");
        let created = db
            .host_save(&HostDraft {
                protocol: None,
                id: None,
                group_id: None,
                name: "router".to_owned(),
                host: "192.0.2.10".to_owned(),
                port: 22,
                username: "admin".to_owned(),
                auth_method: AuthMethod::Password,
                identity_id: None,
                proxy_id: None,
                jump_host_id: None,
                proxy_mode: ProxyMode::Default,
                tags: vec!["lab".to_owned(), "old".to_owned()],
                sort: 5,
                legacy_algorithms: true,
            })
            .expect("建主机");

        // 和窗口里 `save()` 一样：表单值 + 库里读出的原记录。
        let form = FormValues {
            name: "router-renamed".to_owned(),
            ..FormValues::from_host(&created)
        };
        let existing = db.host_get(created.id).expect("读回");
        let draft = keep_hidden_fields(
            draft_from(&form, Some(created.id), HostExtras::default()),
            Some(&existing),
        );
        let saved = db.host_save(&draft).expect("改名保存");

        assert_eq!(saved.name, "router-renamed");
        assert_eq!(
            saved.tags,
            vec!["lab".to_owned(), "old".to_owned()],
            "标签不能丢"
        );
        assert!(saved.legacy_algorithms, "旧算法兼容开关不能被重置");
        assert_eq!(saved.sort, 5);

        // 对照：不带原记录的老写法（修复前）会把它们清掉。
        let broken = db
            .host_save(&draft_from(&form, Some(created.id), HostExtras::default()))
            .expect("老写法保存");
        assert!(broken.tags.is_empty() && !broken.legacy_algorithms);
    }

    #[test]
    fn new_hosts_start_with_default_hidden_fields() {
        let draft = keep_hidden_fields(
            draft_from(&FormValues::new(), None, HostExtras::default()),
            None,
        );
        assert!(draft.tags.is_empty());
        assert!(!draft.legacy_algorithms);
        assert_eq!(draft.sort, 0);
    }

    #[test]
    fn deleting_a_jump_host_lists_the_hosts_that_go_through_it() {
        let hosts = vec![
            sample_host(1, "bastion", None),
            sample_host(2, "web-1", Some(1)),
            sample_host(3, "db-1", Some(1)),
            sample_host(4, "other", Some(2)),
        ];
        let impact = host_delete_impact(&hosts, &[], 1);
        assert_eq!(impact.via_jump, vec!["web-1".to_owned(), "db-1".to_owned()]);
        assert_eq!(impact.forwards, 0);
        let text = host_delete_prompt("bastion", &impact);
        assert!(
            text.contains("2 台") && text.contains("web-1、db-1"),
            "{text}"
        );
        assert!(text.contains("直连"), "{text}");

        // 没人用它：只剩确认提示本身。
        let impact = host_delete_impact(&hosts, &[], 3);
        assert_eq!(impact, HostDeleteImpact::default());
        assert_eq!(
            host_delete_prompt("db-1", &impact),
            "再点一次「确认删除」才会删除 db-1"
        );
    }

    #[test]
    fn deleting_a_host_counts_its_own_forward_rules() {
        use zssh_core::store::forwards::{ForwardKind, PortForward};
        let rule = |id: i64, host_id: i64| PortForward {
            id,
            host_id,
            name: format!("rule-{id}"),
            kind: ForwardKind::Local,
            bind_host: "127.0.0.1".to_owned(),
            bind_port: 8000 + id as u16,
            dest_host: "127.0.0.1".to_owned(),
            dest_port: 80,
            auto_start: false,
        };
        let hosts = vec![sample_host(1, "web", None), sample_host(2, "db", None)];
        let forwards = vec![rule(1, 1), rule(2, 1), rule(3, 2)];
        let impact = host_delete_impact(&hosts, &forwards, 1);
        assert_eq!(impact.forwards, 2);
        assert!(host_delete_prompt("web", &impact).contains("2 条端口转发规则"));
    }
}
