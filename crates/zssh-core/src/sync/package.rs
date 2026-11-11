//! 同步包的明文结构。
//!
//! 明文是一个带 `schema_version` 的 JSON，加密后才写出去。引用关系（主机 →
//! 分组 / 凭据 / 代理 / 跳板机）全部用 `uid`；本机自增 id **不出现在这里**。
//!
//! 记录里带 `updated_at` 和 `device_id`：三方合并时用时间较新的那份，时间相同
//! 或相差很小时用设备 ID 做确定性的平局规则（[`crate::sync::merge`]）。

use crate::error::Reason;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

use super::error::SyncError;
use crate::ssh::known_hosts::normalize_host;

/// 基础明文格式版本。包头的 `version` 是加密信封的版本，这个是明文数据结构的版本。
///
/// Z-SSH 0.2.14 只认这一版：它解析时丢掉不认识的字段、再原样写回云端，所以任何带新字段的
/// 数据都不能用这个版本号写出去（见 [`SNAPSHOT_VERSION_PROTOCOLS`]）。
pub const SNAPSHOT_VERSION: u32 = 1;

/// 带协议字段（Telnet / 串口 / X11）的明文格式版本。
///
/// 只有包里确实有非 SSH 主机、X11 转发或协议选项时才用它写（[`SyncSnapshot::required_version`]），
/// 其余数据仍写 [`SNAPSHOT_VERSION`]，旧版本继续能同步。旧版本读到它会返回
/// `FormatTooNew`、不改云端也不改本地，而不是静默丢字段。
pub const SNAPSHOT_VERSION_PROTOCOLS: u32 = 2;

/// 带内置助手接口（`ai_endpoints`、默认接口设置、`builtin` 默认智能体）的明文格式版本。
///
/// 只有包里确实有这些内容时才用它写（[`SyncSnapshot::required_version`]）。只认到 v2 的
/// ZeeTerm 和 Z-SSH 0.2.14 读到它都会返回 `FormatTooNew`、不改云端也不改本地，而不是把接口配置
/// 和 Key 静默丢掉。
pub const SNAPSHOT_VERSION_BUILTIN_AI: u32 = 3;

/// 这个版本能读的最高明文格式版本。
pub const MAX_SNAPSHOT_VERSION: u32 = SNAPSHOT_VERSION_BUILTIN_AI;

/// 墓碑保留 90 天。
pub const TOMBSTONE_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;

pub const ENTITY_GROUP: &str = "group";
pub const ENTITY_HOST: &str = "host";
pub const ENTITY_IDENTITY: &str = "identity";
pub const ENTITY_PROXY: &str = "proxy";
pub const ENTITY_KNOWN_HOST: &str = "known_host";
pub const ENTITY_AI_ENDPOINT: &str = "ai_endpoint";

/// 内置助手默认接口设置的键（和 `store::settings::AI_BUILTIN_DEFAULT_KEY` 一致）。
const BUILTIN_DEFAULT_SETTING: &str = crate::store::settings::AI_BUILTIN_DEFAULT_KEY;

/// 一个同步快照（云端的“当前版本”或本机的“上次同步基线”都是它）。
///
/// `Serialize` 手写：写出去的 `schema_version` 永远按内容算（[`SyncSnapshot::required_version`]），
/// 不信内存里那个字段——合并 / 导入出来的快照字段是 1，但里面可能已经有新字段的主机。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SyncSnapshot {
    pub schema_version: u32,
    /// 导出这份快照的设备。
    pub device_id: String,
    /// 导出时间（Unix 秒）。
    pub created_at: i64,
    #[serde(default)]
    pub groups: Vec<GroupRecord>,
    #[serde(default)]
    pub hosts: Vec<HostRecord>,
    #[serde(default)]
    pub identities: Vec<IdentityRecord>,
    #[serde(default)]
    pub proxies: Vec<ProxyRecord>,
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
    #[serde(default)]
    pub known_hosts: Vec<KnownHostRecord>,
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
    /// 内置助手的接口（V2-A3）。Key 不在这里：它是 `identities` 里的一条密码类凭据，
    /// 随凭据一起加密同步，这里只引用它的 uid。老格式的包没有这个字段。
    #[serde(default)]
    pub ai_endpoints: Vec<AiEndpointRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupRecord {
    pub uid: String,
    pub name: String,
    #[serde(default)]
    pub parent_uid: Option<String>,
    #[serde(default)]
    pub sort: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostRecord {
    pub uid: String,
    #[serde(default)]
    pub group_uid: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: String,
    #[serde(default)]
    pub identity_uid: Option<String>,
    #[serde(default)]
    pub proxy_uid: Option<String>,
    #[serde(default)]
    pub jump_host_uid: Option<String>,
    /// 包里没写代理方式（旧版本 / 别的来源）：落成「跟随默认代理」，不能是空串（空串读不回库）。
    #[serde(default = "default_proxy_mode")]
    pub proxy_mode: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sort: i64,
    #[serde(default)]
    pub legacy_algorithms: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub device_id: String,
    /// 连接协议（`ssh` / `telnet` / `serial`）。SSH 不写出来：老格式的主机记录就是这个样子。
    #[serde(
        default = "default_protocol",
        skip_serializing_if = "is_default_protocol"
    )]
    pub protocol: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub x11_forwarding: bool,
    /// Telnet / 串口选项（`{"telnet": ...}` 或 `{"serial": ...}`）。原样保留，包括这个版本
    /// 不认识的键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_options: Option<Value>,
}

fn default_proxy_mode() -> String {
    "default".to_owned()
}

fn default_protocol() -> String {
    "ssh".to_owned()
}

fn is_default_protocol(protocol: &String) -> bool {
    protocol == "ssh"
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentityRecord {
    pub uid: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub key_path: Option<String>,
    /// 明文密码 / 私钥 / 口令。**只在加密后的同步包和 DPAPI 基线里存在**。
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub device_id: String,
}

/// `Debug` 手写：`secret` 是明文秘密，不能进日志（R-8 第 11 节）。
impl std::fmt::Debug for IdentityRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityRecord")
            .field("uid", &self.uid)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("key_path", &self.key_path)
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .field("updated_at", &self.updated_at)
            .field("device_id", &self.device_id)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyRecord {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub device_id: String,
}

/// `Debug` 手写：`secret` 是明文代理密码，不能进日志（R-8 第 11 节）。
impl std::fmt::Debug for ProxyRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyRecord")
            .field("uid", &self.uid)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .field("updated_at", &self.updated_at)
            .field("device_id", &self.device_id)
            .finish()
    }
}

/// 内置助手接口（V2-A3）。
///
/// Key 不在记录里：接口的 Key 是 `identities` 里的一条密码类凭据，随凭据记录一起走同一套
/// 加密，这里只引用它的 uid（`key_identity_uid`），合并时凭据被删就像主机引用的凭据被删一样
/// 置空引用。`overrides` / `openai_options` 在包里是 JSON 值，原样保留（含这个版本不认识的键）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiEndpointRecord {
    pub uid: String,
    pub name: String,
    /// `openai_chat` / `anthropic`。
    pub kind: String,
    pub base_url: String,
    #[serde(default)]
    pub key_identity_uid: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "empty_object")]
    pub overrides: Value,
    #[serde(default = "empty_object")]
    pub openai_options: Value,
    #[serde(default)]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub device_id: String,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

impl AiEndpointRecord {
    pub fn same_content(&self, other: &Self) -> bool {
        self.uid == other.uid
            && self.name == other.name
            && self.kind == other.kind
            && self.base_url == other.base_url
            && self.key_identity_uid == other.key_identity_uid
            && self.default_model == other.default_model
            && self.models == other.models
            && self.overrides == other.overrides
            && self.openai_options == other.openai_options
            && self.preset_id == other.preset_id
            && self.created_at == other.created_at
    }
}

/// known_hosts 的一条记录。记录键是“主机 + 端口 + 密钥类型”，没有 uid。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownHostRecord {
    /// 主机名或 IP；哈希主机名原样存（`|1|salt|hash`），这时端口固定按 22 处理。
    pub host: String,
    pub port: u16,
    pub key_type: String,
    /// 公钥 base64（不含 `ssh-ed25519` 前缀）。
    pub key: String,
    /// 只有 `@revoked` 会写，普通记录是 `None`。
    #[serde(default)]
    pub marker: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub device_id: String,
}

/// 明文秘密在记录被丢掉时清零（合并、导入过程中会有很多临时副本）。
impl Drop for IdentityRecord {
    fn drop(&mut self) {
        if let Some(secret) = self.secret.as_mut() {
            secret.zeroize();
        }
    }
}

impl Drop for ProxyRecord {
    fn drop(&mut self) {
        if let Some(secret) = self.secret.as_mut() {
            secret.zeroize();
        }
    }
}

impl KnownHostRecord {
    /// 合并用的记录键：主机 + 端口 + 密钥类型。
    pub fn key(&self) -> String {
        known_host_key(&self.host, self.port, &self.key_type)
    }

    pub fn same_content(&self, other: &Self) -> bool {
        // 主机名大小写不同写的是同一台机器（和记录键一致），否则旧数据里的 `Web-1`
        // 和规范化后的 `web-1` 会被当成“改过”。
        normalize_host(&self.host) == normalize_host(&other.host)
            && self.port == other.port
            && self.key_type == other.key_type
            && self.key == other.key
            && self.marker == other.marker
    }
}

/// 已知主机的记录键。主机名按 OpenSSH 的规则转小写（见 [`normalize_host`]）：
/// `Web-1` 和 `web-1` 是同一条记录，合并、去重、墓碑、从文件里删除都靠这个键。
pub fn known_host_key(host: &str, port: u16, key_type: &str) -> String {
    format!("{}\u{1f}{port}\u{1f}{key_type}", normalize_host(host))
}

/// 把旧版本写下的墓碑键（主机名没转小写）改成当前的记录键写法；不是 [`known_host_key`]
/// 格式的原样返回。
pub fn canonical_known_host_key(key: &str) -> String {
    match key.split_once('\u{1f}') {
        Some((host, rest)) => format!("{}\u{1f}{rest}", normalize_host(host)),
        None => key.to_owned(),
    }
}

/// 删除事件。过期后不再参与合并，但也不会让记录复活（见合并说明）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    /// [`ENTITY_GROUP`] / [`ENTITY_HOST`] / [`ENTITY_IDENTITY`] / [`ENTITY_PROXY`] /
    /// [`ENTITY_KNOWN_HOST`]。
    pub entity: String,
    /// uid；known_hosts 是 [`known_host_key`] 的结果。
    pub key: String,
    pub deleted_at: i64,
    pub device_id: String,
}

impl Tombstone {
    pub fn is_expired(&self, now: i64) -> bool {
        now.saturating_sub(self.deleted_at) > TOMBSTONE_RETENTION_SECS
    }
}

impl GroupRecord {
    pub fn same_content(&self, other: &Self) -> bool {
        self.uid == other.uid
            && self.name == other.name
            && self.parent_uid == other.parent_uid
            && self.sort == other.sort
    }
}

impl HostRecord {
    pub fn same_content(&self, other: &Self) -> bool {
        self.uid == other.uid
            && self.group_uid == other.group_uid
            && self.name == other.name
            && self.host == other.host
            && self.port == other.port
            && self.username == other.username
            && self.auth_method == other.auth_method
            && self.identity_uid == other.identity_uid
            && self.proxy_uid == other.proxy_uid
            && self.jump_host_uid == other.jump_host_uid
            && self.proxy_mode == other.proxy_mode
            && self.tags == other.tags
            && self.sort == other.sort
            && self.legacy_algorithms == other.legacy_algorithms
            && self.created_at == other.created_at
            && self.protocol == other.protocol
            && self.x11_forwarding == other.x11_forwarding
            && self.protocol_options == other.protocol_options
    }

    /// 这条记录带了老版本（Z-SSH 0.2.14）不认识的字段，包必须用 [`SNAPSHOT_VERSION_PROTOCOLS`] 写。
    pub fn needs_protocol_format(&self) -> bool {
        self.protocol != "ssh" || self.x11_forwarding || self.protocol_options.is_some()
    }
}

impl IdentityRecord {
    pub fn same_content(&self, other: &Self) -> bool {
        self.uid == other.uid
            && self.name == other.name
            && self.kind == other.kind
            && self.key_path == other.key_path
            && self.secret == other.secret
    }
}

impl ProxyRecord {
    pub fn same_content(&self, other: &Self) -> bool {
        self.uid == other.uid
            && self.name == other.name
            && self.kind == other.kind
            && self.host == other.host
            && self.port == other.port
            && self.username == other.username
            && self.secret == other.secret
    }
}

impl Serialize for SyncSnapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 借用的镜像结构：字段顺序和名字与 `SyncSnapshot` 一致，只有版本号是现算的。
        #[derive(Serialize)]
        struct Wire<'a> {
            schema_version: u32,
            device_id: &'a str,
            created_at: i64,
            groups: &'a [GroupRecord],
            hosts: &'a [HostRecord],
            identities: &'a [IdentityRecord],
            proxies: &'a [ProxyRecord],
            settings: &'a BTreeMap<String, Value>,
            known_hosts: &'a [KnownHostRecord],
            tombstones: &'a [Tombstone],
            // 没有接口时不写这个键：和老格式的包字节一致。
            #[serde(skip_serializing_if = "<[AiEndpointRecord]>::is_empty")]
            ai_endpoints: &'a [AiEndpointRecord],
        }
        Wire {
            schema_version: self.required_version(),
            device_id: &self.device_id,
            created_at: self.created_at,
            groups: &self.groups,
            hosts: &self.hosts,
            identities: &self.identities,
            proxies: &self.proxies,
            settings: &self.settings,
            known_hosts: &self.known_hosts,
            tombstones: &self.tombstones,
            ai_endpoints: &self.ai_endpoints,
        }
        .serialize(serializer)
    }
}

impl SyncSnapshot {
    pub fn empty(device_id: impl Into<String>, created_at: i64) -> Self {
        Self {
            schema_version: SNAPSHOT_VERSION,
            device_id: device_id.into(),
            created_at,
            groups: Vec::new(),
            hosts: Vec::new(),
            identities: Vec::new(),
            proxies: Vec::new(),
            settings: BTreeMap::new(),
            known_hosts: Vec::new(),
            tombstones: Vec::new(),
            ai_endpoints: Vec::new(),
        }
    }

    /// 解析明文并检查数据格式版本。
    pub fn parse(bytes: &[u8]) -> Result<Self, SyncError> {
        let snapshot: Self = serde_json::from_slice(bytes)
            .map_err(|err| SyncError::Format(Reason::text(format!("明文 JSON 解析失败：{err}"))))?;
        if snapshot.schema_version > MAX_SNAPSHOT_VERSION {
            return Err(SyncError::FormatTooNew {
                found: snapshot.schema_version,
                supported: MAX_SNAPSHOT_VERSION,
            });
        }
        if snapshot.schema_version < SNAPSHOT_VERSION {
            return Err(SyncError::FormatTooOld {
                found: snapshot.schema_version,
            });
        }
        Ok(snapshot)
    }

    /// 这份快照带了内置助手的内容（接口、默认接口设置、`builtin` 默认智能体）：写它必须用
    /// [`SNAPSHOT_VERSION_BUILTIN_AI`]，老客户端才会拒绝合并而不是丢数据。
    pub fn needs_builtin_ai_format(&self) -> bool {
        !self.ai_endpoints.is_empty()
            || self.settings.contains_key(BUILTIN_DEFAULT_SETTING)
            || self
                .settings
                .get("ai_default_agent")
                .and_then(Value::as_str)
                == Some("builtin")
    }

    /// 按内容算出写这份快照该用的明文格式版本：带内置助手内容用 [`SNAPSHOT_VERSION_BUILTIN_AI`]，
    /// 其次有任何带新字段的主机用 [`SNAPSHOT_VERSION_PROTOCOLS`]，否则用 [`SNAPSHOT_VERSION`]
    /// （旧版本照常能读）。
    pub fn required_version(&self) -> u32 {
        if self.needs_builtin_ai_format() {
            SNAPSHOT_VERSION_BUILTIN_AI
        } else if self.hosts.iter().any(HostRecord::needs_protocol_format) {
            SNAPSHOT_VERSION_PROTOCOLS
        } else {
            SNAPSHOT_VERSION
        }
    }

    pub fn to_json(&self) -> Result<Vec<u8>, SyncError> {
        serde_json::to_vec(self)
            .map_err(|err| SyncError::Internal(Reason::text(format!("同步包序列化失败：{err}"))))
    }

    /// 排序成规范顺序，保证同样的数据序列化出来一样（平局规则、测试都靠它）。
    pub fn sort_canonical(&mut self) {
        // 内存里的版本号跟着内容走，这样"解析出来的"和"刚合并出来的"才能比较相等。
        self.schema_version = self.required_version();
        self.groups.sort_by(|a, b| a.uid.cmp(&b.uid));
        self.hosts.sort_by(|a, b| a.uid.cmp(&b.uid));
        self.identities.sort_by(|a, b| a.uid.cmp(&b.uid));
        self.proxies.sort_by(|a, b| a.uid.cmp(&b.uid));
        self.ai_endpoints.sort_by(|a, b| a.uid.cmp(&b.uid));
        self.known_hosts.sort_by_key(|record| record.key());
        self.tombstones.sort_by(|a, b| {
            (a.entity.as_str(), a.key.as_str()).cmp(&(b.entity.as_str(), b.key.as_str()))
        });
    }

    /// 只比较业务数据（忽略快照自己的 `device_id`、`created_at`）。
    ///
    /// 已知主机记录单独按 [`KnownHostRecord::same_content`] 比：本机采集时每条记录都带"这次
    /// 采集的时间 + 本机设备号"，而合并结果对没变的记录沿用基线里的旧值，用派生的 `==` 比
    /// 会让只要有一条已知主机记录就永远"本地有变化"（每次同步都白做一次备份和整库重写）。
    /// 这两个字段是元数据（只在记录冲突时拿来定胜负），不是数据；数据 = 主机、端口、密钥类型、
    /// 公钥、`@revoked` 标记。
    pub fn same_data(&self, other: &Self) -> bool {
        self.groups == other.groups
            && self.hosts == other.hosts
            && self.identities == other.identities
            && self.proxies == other.proxies
            && self.settings == other.settings
            && self.known_hosts.len() == other.known_hosts.len()
            && self
                .known_hosts
                .iter()
                .zip(&other.known_hosts)
                .all(|(a, b)| a.same_content(b))
            && self.tombstones == other.tombstones
            && self.ai_endpoints == other.ai_endpoints
    }

    /// 记录总数（防误删的阈值用）。
    pub fn record_count(&self) -> usize {
        self.groups.len()
            + self.hosts.len()
            + self.identities.len()
            + self.proxies.len()
            + self.ai_endpoints.len()
    }

    /// 序列化后用主密码加密成同步包（明文立即清零）。
    pub fn seal(&self, password: &str) -> Result<Vec<u8>, SyncError> {
        let plaintext = zeroize::Zeroizing::new(self.to_json()?);
        super::crypto::seal(&plaintext, password)
    }

    /// 解开同步包并解析明文。版本太新在 [`super::crypto::open`] 就拒绝了。
    pub fn open(bytes: &[u8], password: &str) -> Result<Self, SyncError> {
        let plaintext = super::crypto::open(bytes, password)?;
        Self::parse(&plaintext)
    }

    /// 某一类记录的所有 uid（known_hosts 用记录键）。
    pub fn keys_of(&self, entity: &str) -> BTreeSet<String> {
        match entity {
            ENTITY_GROUP => self.groups.iter().map(|r| r.uid.clone()).collect(),
            ENTITY_HOST => self.hosts.iter().map(|r| r.uid.clone()).collect(),
            ENTITY_IDENTITY => self.identities.iter().map(|r| r.uid.clone()).collect(),
            ENTITY_PROXY => self.proxies.iter().map(|r| r.uid.clone()).collect(),
            ENTITY_KNOWN_HOST => self.known_hosts.iter().map(KnownHostRecord::key).collect(),
            ENTITY_AI_ENDPOINT => self.ai_endpoints.iter().map(|r| r.uid.clone()).collect(),
            _ => BTreeSet::new(),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod compat_tests;
