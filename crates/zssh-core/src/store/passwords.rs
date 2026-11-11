//! 认证后保存密码：绑定拨号前的主机链，在同一个写事务里校验并更新，避免改链或改凭据时串写。

use rusqlite::{params, Connection};

use super::{hosts::host_get, Database};
use crate::{
    error::ZsshError,
    model::{AuthMethod, Host},
};

/// 只在内存中持有的拨号身份快照。凭据只保留加密状态，不提供 Debug 或序列化。
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordSnapshot {
    target_id: i64,
    hops: Vec<Hop>,
}

impl PasswordSnapshot {
    pub fn target_id(&self) -> i64 {
        self.target_id
    }
    pub fn hosts(&self) -> impl ExactSizeIterator<Item = &Host> {
        self.hops.iter().map(|hop| &hop.host)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Hop {
    host: Host,
    uid: String,
    identity: Option<Credential>,
}

#[derive(Clone, PartialEq, Eq)]
struct Credential {
    uid: String,
    kind: String,
    key_path: Option<String>,
    secret_blob: Option<Vec<u8>>,
}

/// 一跳认证成功且用户明确选择保存的密码；新建凭据时的名字由界面按语言提供。
pub struct PasswordReplacement {
    password: String,
    name: String,
}

impl PasswordReplacement {
    pub fn new(password: String, name: String) -> Self {
        Self { password, name }
    }
}

impl std::fmt::Debug for PasswordReplacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordReplacement")
            .field("password", &"<redacted>")
            .finish()
    }
}

fn capture(conn: &Connection, target_id: i64) -> Result<Option<PasswordSnapshot>, ZsshError> {
    let mut hops = Vec::new();
    let mut next = Some(target_id);
    while let Some(id) = next {
        // 连接准备会给环路和过深的链出正式错误；这里不为无法对应的链生成可回写的授权。
        if hops.len() >= super::hosts::MAX_CHAIN_HOPS
            || hops.iter().any(|hop: &Hop| hop.host.id == id)
        {
            return Ok(None);
        }
        let host = host_get(conn, id)?;
        let uid = conn.query_row("SELECT uid FROM hosts WHERE id = ?1", [id], |row| {
            row.get(0)
        })?;
        let identity = host
            .identity_id
            .map(|identity_id| {
                conn.query_row(
                    "SELECT uid, kind, key_path, secret_blob FROM identities WHERE id = ?1",
                    [identity_id],
                    |row| {
                        Ok(Credential {
                            uid: row.get(0)?,
                            kind: row.get(1)?,
                            key_path: row.get(2)?,
                            secret_blob: row.get(3)?,
                        })
                    },
                )
            })
            .transpose()?;
        next = host.jump_host_id;
        hops.push(Hop {
            host,
            uid,
            identity,
        });
    }
    hops.reverse();
    Ok(Some(PasswordSnapshot { target_id, hops }))
}

impl Database {
    /// 准备拨号材料前后各采集一次；二者一致才允许认证后的密码回写。
    pub fn password_writeback_snapshot(
        &self,
        target_id: i64,
    ) -> Result<Option<PasswordSnapshot>, ZsshError> {
        self.with_conn(|conn| {
            // 只读事务保证整条链来自同一版本，同时允许 WAL 下的其他写入继续进行。
            let tx = conn.transaction()?;
            let snapshot = capture(&tx, target_id)?;
            tx.commit()?;
            Ok(snapshot)
        })
    }

    /// 任一跳身份/凭据已变化就整批跳过；验证与写入共用 IMMEDIATE 事务，其他连接不能插入修改。
    /// 仅更新秘密和必要的凭据关联，不用旧主机草稿覆盖数据库的新字段。
    pub fn save_authenticated_passwords(
        &self,
        expected: &PasswordSnapshot,
        replacements: &[Option<PasswordReplacement>],
    ) -> Result<usize, ZsshError> {
        if expected.hops.len() != replacements.len() || replacements.iter().all(Option::is_none) {
            return Ok(0);
        }
        let count = self.with_tx(|conn| {
            if capture(conn, expected.target_id)?.as_ref() != Some(expected) { return Ok(0); }
            let now = super::hosts::now_unix();
            let mut written = 0;
            for (hop, replacement) in expected.hops.iter().zip(replacements) {
                let Some(replacement) = replacement else { continue };
                if hop.host.auth_method != AuthMethod::Password || replacement.password.is_empty() { continue; }
                let blob = super::secrets::protect(replacement.password.as_bytes())?;
                let exclusive = match (hop.host.identity_id, &hop.identity) {
                    (Some(id), Some(identity)) if identity.kind == "password" => {
                        let others: i64 = conn.query_row("SELECT count(*) FROM hosts WHERE identity_id = ?1 AND id <> ?2", params![id, hop.host.id], |row| row.get(0))?;
                        (others == 0).then_some(id)
                    }
                    _ => None,
                };
                if let Some(id) = exclusive {
                    conn.execute("UPDATE identities SET secret_blob = ?1, updated_at = ?2 WHERE id = ?3", params![blob, now, id])?;
                } else {
                    conn.execute("INSERT INTO identities (uid, name, kind, key_path, secret_blob, updated_at) VALUES (?1, ?2, 'password', NULL, ?3, ?4)", params![super::new_uid(), replacement.name, blob, now])?;
                    conn.execute("UPDATE hosts SET identity_id = ?1, updated_at = ?2 WHERE id = ?3", params![conn.last_insert_rowid(), now, hop.host.id])?;
                }
                written += 1;
            }
            Ok(written)
        })?;
        if count > 0 {
            self.notify_changed();
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests;
