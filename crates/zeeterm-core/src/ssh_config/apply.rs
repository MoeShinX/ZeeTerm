//! 落库：把计划里勾选的主机连同要新建的私钥凭据，放在**一个事务**里建好。

use crate::error::ZeetermError;
use crate::model::{AuthMethod, HostDraft, IdentityDraft, IdentityKind, ProxyMode};
use crate::store::db::Database;

use super::plan::{ImportPlan, ImportRow, JumpTarget, PlanAuth};

/// 导入的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOutcome {
    pub hosts_created: usize,
    pub identities_created: usize,
}

impl Database {
    /// 按计划新建主机（`group_id` 是统一放进的分组，`None` 不分组）。
    ///
    /// 要么全部建好，要么什么都不留：任何一台建失败（比如它引用的跳板机在计划之后被删了）整个事务回滚，
    /// 连已经建好的凭据一起撤销。提交成功之后才通知同步层“数据变了”，且只通知一次。
    ///
    /// 私钥凭据只记路径（`key_file`），这里不会去打开那个文件。
    pub fn import_ssh_hosts(
        &self,
        rows: &[ImportRow],
        plan: &ImportPlan,
        group_id: Option<i64>,
    ) -> Result<ImportOutcome, ZeetermError> {
        self.with_conn_write(|conn| {
            let mut identity_ids = Vec::with_capacity(plan.identities.len());
            for identity in &plan.identities {
                let saved = Self::identity_save_in(
                    conn,
                    &IdentityDraft {
                        id: None,
                        name: identity.name.clone(),
                        kind: IdentityKind::KeyFile,
                        key_path: Some(identity.path.clone()),
                        secret: None,
                        passphrase: None,
                    },
                )?;
                identity_ids.push(saved.id);
            }

            // 行下标 → 新建出来的主机 id。`plan.order` 保证跳板机排在依赖它的主机前面。
            let mut created = std::collections::HashMap::new();
            for &index in &plan.order {
                let (Some(row), Some(row_plan)) = (rows.get(index), plan.rows.get(index)) else {
                    continue;
                };
                let jump_host_id = match &row_plan.jump {
                    JumpTarget::None => None,
                    JumpTarget::Existing { id, .. } => Some(*id),
                    // 计划算出来的顺序里跳板机一定在前；万一对不上就别设，不要凭空造一个引用。
                    JumpTarget::Row(jump) => created.get(jump).copied(),
                };
                let (auth_method, identity_id) = match &row_plan.auth {
                    PlanAuth::Password => (AuthMethod::Password, None),
                    PlanAuth::NewKey(position) => {
                        (AuthMethod::Key, identity_ids.get(*position).copied())
                    }
                    PlanAuth::ExistingKey(id) => (AuthMethod::Key, Some(*id)),
                };
                let draft = HostDraft {
                    id: None,
                    group_id,
                    name: row.alias.clone(),
                    host: row.address.clone(),
                    port: row.port,
                    username: row.username.clone(),
                    auth_method,
                    identity_id,
                    proxy_id: None,
                    jump_host_id,
                    // 和新建主机表单一致：直连的主机跟随默认代理，设了跳板机的主机不用代理。
                    proxy_mode: if jump_host_id.is_some() {
                        ProxyMode::None
                    } else {
                        ProxyMode::Default
                    },
                    tags: Vec::new(),
                    sort: 0,
                    legacy_algorithms: false,
                    protocol: None,
                };
                let (host, _) = Self::host_save_in(conn, &draft, None)?;
                created.insert(index, host.id);
            }
            Ok(ImportOutcome {
                hosts_created: created.len(),
                identities_created: identity_ids.len(),
            })
        })
    }
}
