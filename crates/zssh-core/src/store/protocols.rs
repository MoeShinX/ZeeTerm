//! 主机的协议设置（`hosts` 表的 `protocol` / `x11_forwarding` / `protocol_options` 三列）。
//!
//! 和 [`crate::model::Host`] 分开读写：老界面代码只认 `Host`，协议设置按主机 id 单独取。

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{reason_keys as rk, Entity, Reason, ZsshError};
use crate::model::{HostFacts, Protocol, ProtocolSettings};
use crate::store::db::Database;
use crate::store::hosts::{host_get, hosts_referencing, now_unix};

impl Database {
    /// 一台主机的协议设置。主机不存在报 `NotFound`。
    pub fn host_protocol_get(&self, host_id: i64) -> Result<ProtocolSettings, ZsshError> {
        self.with_conn(|conn| protocol_get(conn, host_id))
    }

    /// 全部主机的协议设置，按主机 id 索引（主机列表一次取完，不用逐台查）。
    pub fn host_protocol_map(&self) -> Result<HashMap<i64, ProtocolSettings>, ZsshError> {
        self.with_conn(|conn| {
            let mut statement =
                conn.prepare("SELECT id, protocol, x11_forwarding, protocol_options FROM hosts")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            let mut map = HashMap::new();
            for row in rows {
                let (id, protocol, x11, options) = row?;
                map.insert(
                    id,
                    ProtocolSettings::from_columns(&protocol, x11 != 0, options.as_deref())?,
                );
            }
            Ok(map)
        })
    }

    /// 只改一台已有主机的协议设置（主机的其他字段不动），按这台主机现在的认证 / 代理 /
    /// 跳板机校验组合。返回整理过的设置。
    pub fn host_protocol_set(
        &self,
        host_id: i64,
        settings: &ProtocolSettings,
    ) -> Result<ProtocolSettings, ZsshError> {
        self.with_conn_write(|conn| {
            let host = host_get(conn, host_id)?;
            // 串口主机的 host / port / username 是占位值，网络主机的是真实值：互相转换会留下
            // 不合法的另一半，要换类型请新建主机。
            let current = protocol_get(conn, host_id)?.protocol;
            if (current == Protocol::Serial) != (settings.protocol == Protocol::Serial) {
                return Err(ZsshError::InvalidInput(Reason::new(
                    rk::HOST_TYPE_SWITCH,
                    "串口主机和网络主机（SSH / Telnet）不能互相转换，请新建一台主机",
                )));
            }
            let identity = match host.identity_id {
                Some(id) => Some(crate::store::identities::identity_get(conn, id)?),
                None => None,
            };
            let jump_host_protocol = match host.jump_host_id {
                Some(id) => Some(protocol_get(conn, id)?.protocol),
                None => None,
            };
            if settings.protocol != Protocol::Ssh {
                let users = hosts_referencing(conn, "jump_host_id", host_id)?;
                if !users.is_empty() {
                    return Err(ZsshError::InvalidInput(
                        Reason::new(
                            rk::HOST_JUMP_NOT_SSH_IN_USE,
                            format!(
                                "这台主机正被 {} 用作跳板机，只有 SSH 主机能当跳板机",
                                users.join("、")
                            ),
                        )
                        .with("users", users.join(", ")),
                    ));
                }
            } else if host.username.trim().is_empty() {
                return Err(ZsshError::InvalidInput(Reason::new(
                    rk::SSH_USER_REQUIRED,
                    "SSH 主机的用户名不能为空，请先填用户名",
                )));
            }
            let settings = settings.clone().validated(&HostFacts {
                username: &host.username,
                auth_method: host.auth_method,
                identity: identity.as_ref(),
                proxy_mode: host.proxy_mode,
                proxy_id: host.proxy_id,
                jump_host_protocol,
            })?;
            write_protocol(conn, host_id, &settings)?;
            Ok(settings)
        })
    }

    /// 这台主机不能当 AI 的执行目标时，返回原因（给模型和用户看）；SSH 主机返回 `None`。
    pub fn host_ai_exec_refusal(&self, host_id: i64) -> Result<Option<String>, ZsshError> {
        Ok(self.host_protocol_get(host_id)?.ai_exec_refusal())
    }
}

/// 读一台主机的协议设置。
pub(crate) fn protocol_get(conn: &Connection, host_id: i64) -> Result<ProtocolSettings, ZsshError> {
    let row = conn
        .query_row(
            "SELECT protocol, x11_forwarding, protocol_options FROM hosts WHERE id = ?1",
            [host_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    match row {
        Some((protocol, x11, options)) => {
            ProtocolSettings::from_columns(&protocol, x11 != 0, options.as_deref())
        }
        None => Err(ZsshError::NotFound {
            entity: Entity::Host,
            id: host_id,
        }),
    }
}

/// 写三列并刷新 `updated_at`（同步按它判断这条记录改过）。调用方已经校验过。
pub(crate) fn write_protocol(
    conn: &Connection,
    host_id: i64,
    settings: &ProtocolSettings,
) -> Result<(), ZsshError> {
    let options = settings.options_json()?;
    let changed = conn.execute(
        "UPDATE hosts SET protocol = ?1, x11_forwarding = ?2, protocol_options = ?3, \
         updated_at = ?4 WHERE id = ?5",
        params![
            settings.protocol.as_str(),
            i64::from(settings.x11_forwarding),
            options,
            now_unix(),
            host_id
        ],
    )?;
    if changed == 0 {
        return Err(ZsshError::NotFound {
            entity: Entity::Host,
            id: host_id,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
