//! 内置助手接口配置的存取（V2-A3）。
//!
//! Key 存在凭据保险库（`identities`，类型"密码"，名字"<接口名> 的 API Key"），和主机密码走
//! 同一套 DPAPI 加密、同一套同步加密；这里的结构体和日志里都不出现 Key，取 Key 要显式调
//! [`Database::ai_endpoint_api_key`]。

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::ai::agent::builtin::catalog::CapsOverride;
use crate::ai::agent::builtin::config::{
    key_identity_name, normalize_draft, AiEndpoint, AiEndpointDraft, BuiltinDefault, EndpointKind,
    OpenAiOptionsConfig,
};
use crate::error::ZsshError;
use crate::store::db::Database;
use crate::store::identities::identity_secret;
use crate::store::settings::AI_BUILTIN_DEFAULT_KEY;

const SELECT_COLUMNS: &str = "e.id, e.uid, e.name, e.kind, e.base_url, e.key_identity_id, \
     (i.secret_blob IS NOT NULL AND length(i.secret_blob) > 0), e.default_model, e.models, \
     e.overrides, e.openai_options, e.preset_id, e.created_at, e.updated_at";

const FROM_CLAUSE: &str = "FROM ai_endpoints e LEFT JOIN identities i ON i.id = e.key_identity_id";

impl Database {
    /// 全部接口，按创建顺序。
    pub fn ai_endpoint_list(&self) -> Result<Vec<AiEndpoint>, ZsshError> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {SELECT_COLUMNS} {FROM_CLAUSE} ORDER BY e.created_at, e.id"
            ))?;
            let rows = statement.query_map([], row_to_endpoint)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn ai_endpoint_get(&self, id: i64) -> Result<AiEndpoint, ZsshError> {
        self.with_conn(|conn| endpoint_get(conn, id))
    }

    /// 新建或修改一个接口。
    ///
    /// Key（`draft.api_key`）：`None` 不改；`Some("")` 清掉；有值就加密存进保险库（新建一条
    /// 密码凭据，或更新这个接口原来的那条）。
    pub fn ai_endpoint_save(&self, draft: &AiEndpointDraft) -> Result<AiEndpoint, ZsshError> {
        let draft = normalize_draft(draft)?;
        let models = to_json(&draft.models, "模型清单")?;
        let overrides = to_json(&draft.overrides, "能力覆盖")?;
        let options = to_json(&draft.openai_options, "接口选项")?;
        self.with_conn_write(|conn| {
            let now = crate::store::hosts::now_unix();
            let existing: Option<Option<i64>> = match draft.id {
                Some(id) => Some(
                    conn.query_row(
                        "SELECT key_identity_id FROM ai_endpoints WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or(ZsshError::NotFound {
                        entity: "ZeePilot 接口",
                        id,
                    })?,
                ),
                None => None,
            };
            let old_key_id = existing.flatten();
            let key_name = key_identity_name(&draft.name);

            let key_id = match draft.api_key.as_deref() {
                None => {
                    // 不改 Key；接口改了名，那条凭据的名字跟着改。
                    if let Some(key_id) = old_key_id {
                        conn.execute(
                            "UPDATE identities SET name = ?1, updated_at = ?2 WHERE id = ?3 AND name <> ?1",
                            params![key_name, now, key_id],
                        )?;
                    }
                    old_key_id
                }
                Some("") => {
                    if let Some(key_id) = old_key_id {
                        drop_key_identity(conn, key_id)?;
                    }
                    None
                }
                Some(key) => {
                    let blob = crate::store::secrets::protect(key.as_bytes())?;
                    let live = match old_key_id {
                        Some(key_id) => conn
                            .query_row("SELECT 1 FROM identities WHERE id = ?1", [key_id], |_| {
                                Ok(())
                            })
                            .optional()?
                            .map(|()| key_id),
                        None => None,
                    };
                    match live {
                        Some(key_id) => {
                            conn.execute(
                                "UPDATE identities SET name = ?1, kind = 'password', key_path = NULL, \
                                 secret_blob = ?2, updated_at = ?3 WHERE id = ?4",
                                params![key_name, blob, now, key_id],
                            )?;
                            Some(key_id)
                        }
                        None => {
                            conn.execute(
                                "INSERT INTO identities (uid, name, kind, key_path, secret_blob, updated_at) \
                                 VALUES (?1, ?2, 'password', NULL, ?3, ?4)",
                                params![crate::store::new_uid(), key_name, blob, now],
                            )?;
                            Some(conn.last_insert_rowid())
                        }
                    }
                }
            };

            let id = match draft.id {
                Some(id) => {
                    conn.execute(
                        "UPDATE ai_endpoints SET name = ?1, kind = ?2, base_url = ?3, \
                         key_identity_id = ?4, default_model = ?5, models = ?6, overrides = ?7, \
                         openai_options = ?8, preset_id = ?9, updated_at = ?10 WHERE id = ?11",
                        params![
                            draft.name,
                            draft.kind.as_str(),
                            draft.base_url,
                            key_id,
                            draft.default_model,
                            models,
                            overrides,
                            options,
                            draft.preset_id,
                            now,
                            id
                        ],
                    )?;
                    id
                }
                None => {
                    conn.execute(
                        "INSERT INTO ai_endpoints (uid, name, kind, base_url, key_identity_id, \
                         default_model, models, overrides, openai_options, preset_id, created_at, \
                         updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                        params![
                            crate::store::new_uid(),
                            draft.name,
                            draft.kind.as_str(),
                            draft.base_url,
                            key_id,
                            draft.default_model,
                            models,
                            overrides,
                            options,
                            draft.preset_id,
                            now
                        ],
                    )?;
                    conn.last_insert_rowid()
                }
            };
            endpoint_get(conn, id)
        })
    }

    /// 删一个接口：它的 Key 凭据一起删（凭据还被主机引用时保留），"默认接口"指着它时清掉。
    pub fn ai_endpoint_delete(&self, id: i64) -> Result<(), ZsshError> {
        self.with_conn_write(|conn| {
            let endpoint = endpoint_get(conn, id)?;
            // 先看"默认接口"指着谁（删掉这一行之后按 uid 就解析不出来了）。
            let was_default =
                read_default(conn)?.is_some_and(|current| current.endpoint_uid == endpoint.uid);
            conn.execute("DELETE FROM ai_endpoints WHERE id = ?1", [id])?;
            if let Some(key_id) = endpoint.key_identity_id {
                drop_key_identity(conn, key_id)?;
            }
            if was_default {
                conn.execute(
                    "DELETE FROM settings WHERE key = ?1",
                    [AI_BUILTIN_DEFAULT_KEY],
                )?;
            }
            Ok(())
        })
    }

    /// 被接口当作 API Key 引用的凭据 id：凭据页不显示、主机表单不能选它们。
    pub fn ai_endpoint_key_identity_ids(&self) -> Result<Vec<i64>, ZsshError> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT key_identity_id FROM ai_endpoints WHERE key_identity_id IS NOT NULL",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// 取这个接口的明文 Key（连接用）。没填 Key、凭据被删了都是 `None`。
    pub fn ai_endpoint_api_key(&self, id: i64) -> Result<Option<String>, ZsshError> {
        self.with_conn(|conn| {
            let key_id: Option<i64> = conn
                .query_row(
                    "SELECT key_identity_id FROM ai_endpoints WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(ZsshError::NotFound {
                    entity: "ZeePilot 接口",
                    id,
                })?;
            match key_id {
                Some(key_id) => identity_secret(conn, key_id),
                None => Ok(None),
            }
        })
    }

    /// "默认接口 + 默认模型"。设置里指着不存在的接口（同步来的、已删）按没设置。
    pub fn ai_builtin_default(&self) -> Result<Option<BuiltinDefault>, ZsshError> {
        self.with_conn(|conn| read_default(conn))
    }

    /// 设置默认接口和模型（`model` 为 `None` = 用接口自己的默认模型）；传 `None` 清掉。
    pub fn set_ai_builtin_default(
        &self,
        target: Option<(i64, Option<&str>)>,
    ) -> Result<(), ZsshError> {
        match target {
            None => {
                self.with_conn(|conn| {
                    conn.execute(
                        "DELETE FROM settings WHERE key = ?1",
                        [AI_BUILTIN_DEFAULT_KEY],
                    )?;
                    Ok(())
                })?;
                self.notify_changed();
                Ok(())
            }
            Some((endpoint_id, model)) => {
                let endpoint = self.ai_endpoint_get(endpoint_id)?;
                let model = model.map(str::trim).filter(|m| !m.is_empty());
                let value = serde_json::json!({
                    "endpoint_uid": endpoint.uid,
                    "model": model,
                });
                self.setting_set(AI_BUILTIN_DEFAULT_KEY, &value.to_string())
            }
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T, what: &str) -> Result<String, ZsshError> {
    serde_json::to_string(value)
        .map_err(|err| ZsshError::InvalidInput(format!("{what}序列化失败：{err}")))
}

/// 删接口的 Key 凭据；还被主机引用时保留（只是不再属于这个接口）。
fn drop_key_identity(conn: &Connection, key_id: i64) -> Result<(), ZsshError> {
    let used: i64 = conn.query_row(
        "SELECT count(*) FROM hosts WHERE identity_id = ?1",
        [key_id],
        |row| row.get(0),
    )?;
    if used == 0 {
        conn.execute("DELETE FROM identities WHERE id = ?1", [key_id])?;
    }
    Ok(())
}

fn endpoint_get(conn: &Connection, id: i64) -> Result<AiEndpoint, ZsshError> {
    conn.query_row(
        &format!("SELECT {SELECT_COLUMNS} {FROM_CLAUSE} WHERE e.id = ?1"),
        [id],
        row_to_endpoint,
    )
    .map_err(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => ZsshError::NotFound {
            entity: "ZeePilot 接口",
            id,
        },
        other => other.into(),
    })
}

fn read_default(conn: &Connection) -> Result<Option<BuiltinDefault>, ZsshError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [AI_BUILTIN_DEFAULT_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(None);
    };
    let Some(uid) = value.get("endpoint_uid").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let id: Option<i64> = conn
        .query_row("SELECT id FROM ai_endpoints WHERE uid = ?1", [uid], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(id.map(|endpoint_id| BuiltinDefault {
        endpoint_id,
        endpoint_uid: uid.to_owned(),
        model,
    }))
}

fn row_to_endpoint(row: &Row<'_>) -> rusqlite::Result<AiEndpoint> {
    let kind: String = row.get(3)?;
    let models: String = row.get(8)?;
    let overrides: String = row.get(9)?;
    let options: String = row.get(10)?;
    let name: String = row.get(2)?;
    // JSON 列读不出来（被外部改坏）按默认值处理：一个接口的选项坏了不该让整张列表打不开。
    let models = serde_json::from_str::<Vec<String>>(&models).unwrap_or_else(|_| {
        tracing::warn!(endpoint = %name, "接口的模型清单读不出来，按空处理");
        Vec::new()
    });
    let overrides = serde_json::from_str::<BTreeMap<String, CapsOverride>>(&overrides)
        .unwrap_or_else(|_| {
            tracing::warn!(endpoint = %name, "接口的能力覆盖读不出来，按空处理");
            BTreeMap::new()
        });
    let openai_options = serde_json::from_str::<OpenAiOptionsConfig>(&options).unwrap_or_default();
    Ok(AiEndpoint {
        id: row.get(0)?,
        uid: row.get(1)?,
        name,
        kind: crate::store::parse_column(&kind, EndpointKind::parse)?,
        base_url: row.get(4)?,
        key_identity_id: row.get(5)?,
        has_key: row.get::<_, Option<bool>>(6)?.unwrap_or(false),
        default_model: row.get(7)?,
        models,
        overrides,
        openai_options,
        preset_id: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

#[cfg(test)]
mod tests;
