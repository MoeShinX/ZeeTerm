//! 主机的增删改查，以及连接前的凭据解析。

use rusqlite::{params, Connection, Row};

use crate::error::ZsshError;
use crate::model::{
    AuthMethod, Host, HostDraft, HostFacts, Identity, IdentityKind, Protocol, ProtocolSettings,
    ProxyMode,
};
use crate::ssh::auth::{AuthConfig, KeySource};
use crate::store::db::Database;

/// 拨号链最多几跳（含目标那一跳）。设计文档 4.2 定的是 5 级跳板机。
pub const MAX_CHAIN_HOPS: usize = 5;

/// 连接一台主机需要的全部信息。
///
/// **不要把这个结构体交给前端**：`auth` 里可能有明文密码或私钥内容。
#[derive(Debug, Clone)]
pub struct HostLogin {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// 认证方式和保险库里已经有的材料（缺的到连接时问用户）
    pub auth: AuthConfig,
    pub legacy_algorithms: bool,
}

impl Database {
    pub fn host_list(&self) -> Result<Vec<Host>, ZsshError> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, group_id, name, host, port, username, auth_method, identity_id, \
                 proxy_id, jump_host_id, proxy_mode, tags, sort, created_at, updated_at, \
                 legacy_algorithms, protocol, x11_forwarding, protocol_options FROM hosts ORDER BY sort, name COLLATE NOCASE",
            )?;
            let rows = statement.query_map([], row_to_host)?;
            let mut hosts = Vec::new();
            for row in rows {
                hosts.push(row?);
            }
            Ok(hosts)
        })
    }

    pub fn host_get(&self, id: i64) -> Result<Host, ZsshError> {
        self.with_conn(|conn| host_get(conn, id))
    }

    /// 新建或修改一台主机，返回落库之后的结果。
    ///
    /// 协议设置看 [`HostDraft::protocol`]：`None` 时新建的主机是普通 SSH，修改已有主机时协议设置
    /// **保持库里的不变**，但要按新的认证 / 代理 / 跳板机重新校验一遍组合（不能借这个入口把一台
    /// Telnet 主机改成带私钥认证）；`Some` 时按它写入。
    pub fn host_save(&self, draft: &HostDraft) -> Result<Host, ZsshError> {
        self.host_save_inner(draft, draft.protocol.as_ref())
            .map(|(host, _)| host)
    }

    /// 同 [`Database::host_save`]，同时写入协议设置（一个事务，组合不合法整体不落库）。
    /// 返回落库之后的主机和整理过的协议设置。
    pub fn host_save_with_protocol(
        &self,
        draft: &HostDraft,
        settings: &ProtocolSettings,
    ) -> Result<(Host, ProtocolSettings), ZsshError> {
        self.host_save_inner(draft, Some(settings))
    }

    fn host_save_inner(
        &self,
        draft: &HostDraft,
        requested: Option<&ProtocolSettings>,
    ) -> Result<(Host, ProtocolSettings), ZsshError> {
        self.with_conn_write(|conn| {
            // 没指定协议设置：沿用库里的（新建就是普通 SSH）。
            let stored = match (requested, draft.id) {
                (None, Some(id)) => crate::store::protocols::protocol_get(conn, id)?,
                _ => ProtocolSettings::default(),
            };
            let settings = requested.unwrap_or(&stored);
            validate_host(draft, settings.protocol)?;
            // 串口主机没有地址 / 端口 / 用户名：这几列存固定的占位值，界面带来的默认值不算数。
            // 凭据 / 代理 / 跳板机是用户明确选的东西，选了就报错（见 check_serial_refs）。
            let serial_draft;
            let draft = if settings.protocol == Protocol::Serial {
                crate::model::check_serial_refs(
                    draft.identity_id.is_some(),
                    draft.proxy_id.is_some(),
                    draft.jump_host_id.is_some(),
                )?;
                serial_draft = serial_placeholder_draft(draft);
                &serial_draft
            } else {
                draft
            };

            if let Some(group_id) = draft.group_id {
                crate::store::groups::ensure_group_exists(conn, group_id)?;
            }
            let mut identity = None;
            if let Some(identity_id) = draft.identity_id {
                crate::store::identities::ensure_identity_exists(conn, identity_id)?;
                check_not_ai_key(conn, draft.id, identity_id)?;
                let found = crate::store::identities::identity_get(conn, identity_id)?;
                check_identity_matches(draft.auth_method, &found)?;
                identity = Some(found);
            }
            if let Some(proxy_id) = draft.proxy_id {
                crate::store::proxies::ensure_proxy_exists(conn, proxy_id)?;
            }
            let mut jump_host_protocol = None;
            if let Some(jump_host_id) = draft.jump_host_id {
                if Some(jump_host_id) == draft.id {
                    return Err(ZsshError::InvalidInput("跳板机不能是主机自己".to_owned()));
                }
                host_get(conn, jump_host_id)?;
                jump_host_protocol =
                    Some(crate::store::protocols::protocol_get(conn, jump_host_id)?.protocol);
            }
            if let Some(id) = draft.id {
                if settings.protocol != Protocol::Ssh {
                    let users = hosts_referencing(conn, "jump_host_id", id)?;
                    if !users.is_empty() {
                        return Err(ZsshError::InvalidInput(format!(
                            "这台主机正被 {} 用作跳板机，只有 SSH 主机能当跳板机",
                            users.join("、")
                        )));
                    }
                }
            }
            // 串口主机的 host / port / username 是占位值，网络主机的是真实值：互相转换会把真实值
            // 悄悄冲成占位值（或反过来留下不合法的另一半），要换类型请新建主机。
            if let (Some(id), Some(_)) = (draft.id, requested) {
                let current = crate::store::protocols::protocol_get(conn, id)?.protocol;
                if (current == Protocol::Serial) != (settings.protocol == Protocol::Serial) {
                    return Err(ZsshError::InvalidInput(
                        "串口主机和网络主机（SSH / Telnet）不能互相转换，请新建一台主机".to_owned(),
                    ));
                }
            }
            let settings = settings.clone().validated(&HostFacts {
                username: &draft.username,
                auth_method: draft.auth_method,
                identity: identity.as_ref(),
                proxy_mode: draft.proxy_mode,
                proxy_id: draft.proxy_id,
                jump_host_protocol,
            })?;

            let tags = serde_json::to_string(&draft.tags)
                .map_err(|err| ZsshError::InvalidInput(format!("标签序列化失败：{err}")))?;
            let legacy = i64::from(draft.legacy_algorithms);
            let now = now_unix();

            let saved_id = match draft.id {
                Some(id) => {
                    // 顺序（`sort`）不由「保存主机」决定：前端传来的 `draft.sort` 只是打开面板时
                    // 那一刻的旧值，用它会把之后在树里调好的顺序冲掉（UI-9）。已有的主机
                    // 一律保持库里的 `sort`；换了分组就置 0，排到新分组这一层的末尾。
                    let (old_group, old_sort): (Option<i64>, i64) = conn
                        .query_row(
                            "SELECT group_id, sort FROM hosts WHERE id = ?1",
                            [id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .map_err(|err| match err {
                            rusqlite::Error::QueryReturnedNoRows => ZsshError::NotFound {
                                entity: "主机",
                                id,
                            },
                            other => other.into(),
                        })?;
                    let sort = if old_group == draft.group_id { old_sort } else { 0 };
                    let changed = conn.execute(
                        "UPDATE hosts SET group_id = ?1, name = ?2, host = ?3, port = ?4, \
                         username = ?5, auth_method = ?6, identity_id = ?7, proxy_id = ?8, \
                         jump_host_id = ?9, proxy_mode = ?10, tags = ?11, sort = ?12, \
                         updated_at = ?13, legacy_algorithms = ?14 WHERE id = ?15",
                        params![
                            draft.group_id,
                            draft.name.trim(),
                            draft.host.trim(),
                            draft.port,
                            draft.username.trim(),
                            draft.auth_method.as_str(),
                            draft.identity_id,
                            draft.proxy_id,
                            draft.jump_host_id,
                            draft.proxy_mode.as_str(),
                            tags,
                            sort,
                            now,
                            legacy,
                            id,
                        ],
                    )?;
                    if changed == 0 {
                        return Err(ZsshError::NotFound {
                            entity: "主机", id
                        });
                    }
                    id
                }
                None => {
                    conn.execute(
                        "INSERT INTO hosts (uid, group_id, name, host, port, username, auth_method, \
                         identity_id, proxy_id, jump_host_id, proxy_mode, tags, sort, created_at, \
                         updated_at, legacy_algorithms) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15)",
                        params![
                            crate::store::new_uid(),
                            draft.group_id,
                            draft.name.trim(),
                            draft.host.trim(),
                            draft.port,
                            draft.username.trim(),
                            draft.auth_method.as_str(),
                            draft.identity_id,
                            draft.proxy_id,
                            draft.jump_host_id,
                            draft.proxy_mode.as_str(),
                            tags,
                            draft.sort,
                            now,
                            legacy,
                        ],
                    )?;
                    conn.last_insert_rowid()
                }
            };
            // 没指定协议设置时列保持原样（新建的主机靠列默认值就是普通 SSH）：
            // 不把库里的选项经"读成结构体再写回"洗一遍，免得丢掉这一版不认识的键。
            if requested.is_some() {
                crate::store::protocols::write_protocol(conn, saved_id, &settings)?;
            }
            Ok((host_get(conn, saved_id)?, settings))
        })
    }

    pub fn host_delete(&self, id: i64) -> Result<(), ZsshError> {
        self.with_conn_write(|conn| {
            let changed = conn.execute("DELETE FROM hosts WHERE id = ?1", [id])?;
            if changed == 0 {
                return Err(ZsshError::NotFound {
                    entity: "主机", id
                });
            }
            Ok(())
        })
    }

    /// 从目标主机沿 `jump_host_id` 展开成拨号链。
    ///
    /// 返回的顺序是**从我们这边开始连的顺序**：最外层的跳板机在前，目标在最后。
    /// 检测环路（A→B→A）并限制最多 [`MAX_CHAIN_HOPS`] 级（设计文档 4.2）。
    pub fn host_chain(&self, target_id: i64) -> Result<Vec<Host>, ZsshError> {
        self.with_conn(|conn| {
            // 从目标往上收集
            let mut upwards = vec![host_get(conn, target_id)?];
            let mut seen = std::collections::HashSet::new();
            seen.insert(target_id);

            while let Some(jump_id) = upwards.last().and_then(|host| host.jump_host_id) {
                if !seen.insert(jump_id) {
                    return Err(ZsshError::InvalidInput(format!(
                        "跳板机配置成环了：主机 id {jump_id} 重复出现"
                    )));
                }
                let jump = host_get(conn, jump_id)?;
                // 保存时已经拦了非 SSH 跳板机；这里再兜一道，防旧数据 / 同步来的数据绕过。
                let protocol = crate::store::protocols::protocol_get(conn, jump_id)?.protocol;
                if protocol != Protocol::Ssh {
                    return Err(ZsshError::InvalidInput(format!(
                        "跳板机「{}」是 {} 主机，跳板机必须是 SSH 主机",
                        jump.name,
                        protocol.label()
                    )));
                }
                upwards.push(jump);
                if upwards.len() > MAX_CHAIN_HOPS {
                    return Err(ZsshError::InvalidInput(format!(
                        "跳板机最多 {MAX_CHAIN_HOPS} 级，这台主机配得太深了"
                    )));
                }
            }

            // 反过来：最外层跳板机在前，目标在最后
            upwards.reverse();
            Ok(upwards)
        })
    }

    /// 连接前的凭据解析：把主机引用的材料从保险库里解出来（设计文档 4.3）。
    ///
    /// 只有后端用，返回值不回传前端。缺的密码 / 口令不在这里报错，留给连接时弹窗问。
    pub fn host_login(&self, id: i64) -> Result<HostLogin, ZsshError> {
        self.with_conn(|conn| {
            let host = host_get(conn, id)?;
            if crate::store::protocols::protocol_get(conn, id)?.protocol == Protocol::Serial {
                return Err(ZsshError::InvalidInput(
                    "串口主机没有网络登录信息".to_owned(),
                ));
            }

            let auth = resolve_auth(conn, host.auth_method, host.identity_id, &host.name)?;

            Ok(HostLogin {
                host: host.host,
                port: host.port,
                username: host.username,
                auth,
                legacy_algorithms: host.legacy_algorithms,
            })
        })
    }
}

/// 把认证方式 + 关联的凭据解析成连接要用的认证材料（从保险库里取出秘密）。
///
/// `label` 只用在报错里（「主机「xx」选了私钥认证但没选凭据」）。缺的密码 / 口令不在这里报错，
/// 留给连接时问用户。
/// AI 接口的 API Key 凭据（被 `ai_endpoints.key_identity_id` 引用的）不能给主机当密码 / 密钥：
/// 它的生命周期跟着接口走，删接口时凭据一起删。主机原来就在用它的（接口删除时因被引用而保留的）
/// 不受影响，只拦新选。
fn check_not_ai_key(
    conn: &rusqlite::Connection,
    host_id: Option<i64>,
    identity_id: i64,
) -> Result<(), ZsshError> {
    use rusqlite::OptionalExtension;
    let is_key: bool = conn
        .query_row(
            "SELECT 1 FROM ai_endpoints WHERE key_identity_id = ?1 LIMIT 1",
            [identity_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !is_key {
        return Ok(());
    }
    if let Some(host_id) = host_id {
        let current: Option<Option<i64>> = conn
            .query_row(
                "SELECT identity_id FROM hosts WHERE id = ?1",
                [host_id],
                |row| row.get(0),
            )
            .optional()?;
        if current.flatten() == Some(identity_id) {
            return Ok(());
        }
    }
    Err(ZsshError::InvalidInput(
        "这条凭据是内置助手接口的 API Key，不能给主机用。".to_owned(),
    ))
}

fn resolve_auth(
    conn: &Connection,
    method: AuthMethod,
    identity_id: Option<i64>,
    label: &str,
) -> Result<AuthConfig, ZsshError> {
    Ok(match method {
        AuthMethod::Password => match identity_id {
            Some(identity_id) => {
                // 只有密码类凭据的秘密才能当 SSH 密码发出去：私钥类凭据的秘密是私钥口令
                // 或整段私钥，发给服务器就等于交出了私钥。旧数据或同步来的数据可能是这种
                // 组合，连接时也要拦（R-10 H2）。
                let identity = crate::store::identities::identity_get(conn, identity_id)?;
                check_identity_matches(AuthMethod::Password, &identity)?;
                let secret = crate::store::identities::identity_secret(conn, identity_id)?;
                AuthConfig::password(secret)
            }
            // 没配凭据：连的时候问用户
            None => AuthConfig::password(None),
        },

        AuthMethod::Key => {
            let identity_id = identity_id.ok_or_else(|| {
                ZsshError::InvalidInput(format!("主机「{label}」选了私钥认证但没选凭据"))
            })?;
            let identity = crate::store::identities::identity_get(conn, identity_id)?;
            let secret = crate::store::identities::identity_secret(conn, identity_id)?;

            let source = match identity.kind {
                IdentityKind::KeyFile => {
                    let path = identity
                        .key_path
                        .clone()
                        .filter(|path| !path.trim().is_empty())
                        .ok_or_else(|| {
                            ZsshError::InvalidInput(format!(
                                "凭据「{}」没有填私钥文件路径",
                                identity.name
                            ))
                        })?;
                    KeySource::File {
                        path: path.into(),
                        passphrase: secret,
                    }
                }
                IdentityKind::KeyInline => {
                    let inline = crate::store::identities::InlineKey::parse(
                        secret.as_deref().unwrap_or_default(),
                    );
                    if inline.key.trim().is_empty() {
                        return Err(ZsshError::InvalidInput(format!(
                            "凭据「{}」里没有私钥内容",
                            identity.name
                        )));
                    }
                    KeySource::Inline {
                        pem: inline.key,
                        passphrase: inline.passphrase,
                    }
                }
                IdentityKind::Password | IdentityKind::Agent => {
                    return Err(ZsshError::InvalidInput(format!(
                        "凭据「{}」不是私钥凭据，不能用来做私钥认证",
                        identity.name
                    )))
                }
            };

            AuthConfig::key(source)
        }

        AuthMethod::Agent => AuthConfig::agent(),
    })
}

impl Database {
    /// 表单里还没保存的登录设置也要能连一下（「测试连接」）：按认证方式 + 选的凭据解出认证材料。
    /// 规则和 [`Database::host_login`] 完全一样（同一个函数）。`label` 是报错里的主机名。
    pub fn auth_for_login(
        &self,
        method: AuthMethod,
        identity_id: Option<i64>,
        label: &str,
    ) -> Result<AuthConfig, ZsshError> {
        self.with_conn(|conn| resolve_auth(conn, method, identity_id, label))
    }
}

/// 引用了这个凭据或代理的主机名，被引用时用来告诉用户先改哪些主机。
pub(crate) fn hosts_referencing(
    conn: &Connection,
    column: &str,
    id: i64,
) -> Result<Vec<String>, ZsshError> {
    // column 只可能是调用方写死的常量（identity_id / proxy_id），不来自输入
    let sql = format!("SELECT name FROM hosts WHERE {column} = ?1 ORDER BY name COLLATE NOCASE");
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([id], |row| row.get::<_, String>(0))?;
    let mut names = Vec::new();
    for row in rows {
        names.push(row?);
    }
    Ok(names)
}

pub(crate) fn host_get(conn: &Connection, id: i64) -> Result<Host, ZsshError> {
    conn.query_row(
        "SELECT id, group_id, name, host, port, username, auth_method, identity_id, \
         proxy_id, jump_host_id, proxy_mode, tags, sort, created_at, updated_at, \
         legacy_algorithms, protocol, x11_forwarding, protocol_options FROM hosts WHERE id = ?1",
        [id],
        row_to_host,
    )
    .map_err(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => ZsshError::NotFound {
            entity: "主机", id
        },
        other => other.into(),
    })
}

/// 认证方式和凭据类型要配套：密码认证只能用密码凭据，私钥认证只能用私钥凭据。
/// agent 认证不读凭据里的秘密，不限制。
pub(crate) fn check_identity_matches(
    method: AuthMethod,
    identity: &Identity,
) -> Result<(), ZsshError> {
    let ok = match method {
        AuthMethod::Password => identity.kind == IdentityKind::Password,
        AuthMethod::Key => matches!(
            identity.kind,
            IdentityKind::KeyFile | IdentityKind::KeyInline
        ),
        AuthMethod::Agent => true,
    };
    if ok {
        return Ok(());
    }
    Err(ZsshError::InvalidInput(match method {
        AuthMethod::Password => format!(
            "凭据「{}」不是密码凭据，不能用来做密码认证（它的内容会被当成密码发给服务器）",
            identity.name
        ),
        _ => format!("凭据「{}」不是私钥凭据，不能用来做私钥认证", identity.name),
    }))
}

fn validate_host(draft: &HostDraft, protocol: Protocol) -> Result<(), ZsshError> {
    if draft.name.trim().is_empty() {
        return Err(ZsshError::InvalidInput("主机名不能为空".to_owned()));
    }
    if protocol == Protocol::Serial {
        return Ok(());
    }
    // 地址会原样写进 known_hosts：`*`、逗号、空格之类会让那一行匹配到别的主机。
    crate::ssh::known_hosts::check_host_address(draft.host.trim())
        .map_err(ZsshError::InvalidInput)?;
    if draft.port == 0 {
        return Err(ZsshError::InvalidInput("端口要在 1-65535 之间".to_owned()));
    }
    // Telnet 可以不自动登录，允许不填用户名；SSH 一直要填。
    if protocol == Protocol::Ssh && draft.username.trim().is_empty() {
        return Err(ZsshError::InvalidInput("用户名不能为空".to_owned()));
    }
    Ok(())
}

/// 串口主机落库用的占位值：host / username 空、端口 0、密码认证（列是 NOT NULL，认证方式有
/// CHECK）、不用代理。
fn serial_placeholder_draft(draft: &HostDraft) -> HostDraft {
    HostDraft {
        host: String::new(),
        port: 0,
        username: String::new(),
        auth_method: AuthMethod::Password,
        proxy_mode: ProxyMode::None,
        legacy_algorithms: false,
        ..draft.clone()
    }
}

fn row_to_host(row: &Row<'_>) -> rusqlite::Result<Host> {
    let auth_method: String = row.get(6)?;
    let proxy_mode: String = row.get(10)?;
    let tags: Option<String> = row.get(11)?;
    Ok(Host {
        id: row.get(0)?,
        group_id: row.get(1)?,
        name: row.get(2)?,
        host: row.get(3)?,
        port: row.get(4)?,
        username: row.get(5)?,
        auth_method: crate::store::parse_column(&auth_method, AuthMethod::parse)?,
        identity_id: row.get(7)?,
        proxy_id: row.get(8)?,
        jump_host_id: row.get(9)?,
        proxy_mode: crate::store::parse_column(&proxy_mode, ProxyMode::parse)?,
        tags: tags
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default(),
        sort: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        legacy_algorithms: row.get::<_, i64>(15)? != 0,
        protocol: {
            let kind: String = row.get(16)?;
            let x11: i64 = row.get(17)?;
            let options: Option<String> = row.get(18)?;
            crate::store::parse_column(&kind, |kind| {
                ProtocolSettings::from_columns(kind, x11 != 0, options.as_deref())
            })?
        },
    })
}

/// Unix 秒。
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}
