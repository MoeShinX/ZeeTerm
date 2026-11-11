//! 保存的密码被服务器拒绝、用户重新输入的新密码认证成功并且勾了「用这个密码更新保存的密码」之后，
//! 把新密码写回保险库。
//!
//! 核心库只在认证成功时把用户勾了保存的密码放进 [`zssh_core::ssh::Connection::remembered_secrets`]（拨号链上
//! 每一跳一项，认证失败的连接根本建不起来，所以失败的密码不会走到这里）；这里把"第几跳的新密码"对到库里对应的主机，
//! 再走主机属性对话框保存密码的同一条路径（[`crate::main_window::host_form::plan_password_update`] +
//! `apply_save`：独占的密码身份覆盖，共用的另建并改关联）。
//!
//! 日志里只写主机编号和条数，不写密码。

use zssh_core::ssh::{Connection, PromptKind};
use zssh_core::store::Database;

use crate::main_window::host_form::{apply_save, plan_password_update};

/// 一条要写回的新密码。`Debug` 手写：别让明文进日志或断言输出。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Update {
    pub host_id: i64,
    pub password: String,
}

impl std::fmt::Debug for Update {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Update")
            .field("host_id", &self.host_id)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// 一条连接上用户勾了保存的**密码**（口令不在这条路径上），按拨号链上的跳序排好：没有就是 `None`。
pub(crate) fn collect(connection: &Connection) -> Vec<Option<String>> {
    connection
        .remembered_secrets()
        .into_iter()
        .map(|secret| {
            secret
                .filter(|secret| secret.kind == PromptKind::Password)
                .map(|secret| secret.secret.clone())
        })
        .collect()
}

/// 把"第几跳的新密码"对到拨号链上的主机编号。两边跳数对不上（连接期间有人改了跳板机配置）就一个都不写：
/// 宁可这次不更新，也不能把密码写到别的主机名下。
pub(crate) fn match_updates(chain: &[i64], secrets: Vec<Option<String>>) -> Vec<Update> {
    if chain.len() != secrets.len() {
        return Vec::new();
    }
    chain
        .iter()
        .zip(secrets)
        .filter_map(|(host_id, password)| {
            password.map(|password| Update {
                host_id: *host_id,
                password,
            })
        })
        .collect()
}

/// 逐条写回。每条都重新读主机 / 凭据（同步或别的窗口刚改过的不能被旧值盖回去）。返回写成功的条数；
/// 有一条失败就带着错误文字返回，已经写好的不回滚（它们彼此独立）。
pub(crate) fn apply(db: &Database, updates: &[Update]) -> Result<usize, String> {
    let mut written = 0;
    let mut first_error = None;
    for update in updates {
        match apply_one(db, update) {
            Ok(true) => written += 1,
            Ok(false) => {}
            Err(err) => {
                first_error.get_or_insert(err);
            }
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(written),
    }
}

fn apply_one(db: &Database, update: &Update) -> Result<bool, String> {
    let to_text = |err: zssh_core::error::ZsshError| crate::error_text::error_text(&err);
    let host = db.host_get(update.host_id).map_err(to_text)?;
    let identities = db.identity_list().map_err(to_text)?;
    let hosts = db.host_list().map_err(to_text)?;
    let Some(plan) = plan_password_update(&host, &identities, &hosts, &update.password) else {
        return Ok(false);
    };
    apply_save(db, plan).map_err(|err| err.message)?;
    Ok(true)
}

/// 连接成功之后调用（在阻塞线程里：读写库是同步 IO）：`host_id` 是这条连接的目标主机，
/// `secrets` 是 [`collect`] 的结果。库打不开 / 写失败只记日志（连接已经成功，不能因此让连接失败）。
pub(crate) fn write_back(host_id: i64, secrets: Vec<Option<String>>) {
    if secrets.iter().all(Option::is_none) {
        return;
    }
    let outcome = Database::open(&crate::settings::data_dir().join("zssh.db"))
        .map_err(|err| crate::error_text::error_text(&err))
        .and_then(|db| {
            let chain: Vec<i64> = db
                .host_chain(host_id)
                .map_err(|err| crate::error_text::error_text(&err))?
                .iter()
                .map(|host| host.id)
                .collect();
            apply(&db, &match_updates(&chain, secrets))
        });
    match outcome {
        Ok(count) => tracing::info!(host_id, count, "已按用户的选择更新保存的密码"),
        Err(err) => tracing::warn!(host_id, %err, "更新保存的密码没有成功"),
    }
}

#[cfg(test)]
mod tests {
    use zssh_core::model::{AuthMethod, HostDraft, IdentityDraft, IdentityKind, ProxyMode};

    use super::*;

    fn password_host(db: &Database, name: &str, secret: &str) -> i64 {
        let identity = db
            .identity_save(&IdentityDraft {
                id: None,
                name: format!("{name} 的密码"),
                kind: IdentityKind::Password,
                key_path: None,
                secret: Some(secret.to_owned()),
                passphrase: None,
            })
            .expect("凭据");
        db.host_save(&HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: Some(identity.id),
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        })
        .expect("主机")
        .id
    }

    fn saved(db: &Database, host_id: i64) -> String {
        db.host_saved_password(host_id)
            .expect("取密码")
            .as_str()
            .to_owned()
    }

    #[test]
    fn each_new_password_goes_to_the_host_of_the_hop_it_was_typed_for() {
        // 拨号链：跳板机在前，目标在后。只有跳板机那一跳重新输入了密码。
        let chain = [11, 22];
        let updates = match_updates(&chain, vec![Some("jump-new".to_owned()), None]);
        assert_eq!(
            updates,
            [Update {
                host_id: 11,
                password: "jump-new".to_owned()
            }]
        );
        let updates = match_updates(&chain, vec![None, Some("target-new".to_owned())]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].host_id, 22);
        assert!(match_updates(&chain, vec![None, None]).is_empty());
    }

    #[test]
    fn a_chain_that_changed_while_connecting_writes_nothing() {
        // 跳数对不上：不知道哪一跳对应哪台主机，宁可不写。
        assert!(match_updates(&[11, 22, 33], vec![Some("x".to_owned()), None]).is_empty());
        assert!(match_updates(&[11], vec![None, Some("x".to_owned())]).is_empty());
    }

    #[test]
    fn debug_output_never_shows_the_password() {
        let update = Update {
            host_id: 3,
            password: "hunter2-secret".to_owned(),
        };
        assert!(!format!("{update:?}").contains("hunter2"));
        assert!(!format!("{update:#?}").contains("hunter2"));
    }

    #[test]
    fn applying_updates_changes_only_the_named_hosts_vault_password() {
        let db = Database::open_in_memory().expect("内存库");
        let jump = password_host(&db, "jump", "jump-old");
        let target = password_host(&db, "target", "target-old");

        let written = apply(
            &db,
            &match_updates(&[jump, target], vec![Some("jump-new".to_owned()), None]),
        )
        .expect("写回");
        assert_eq!(written, 1);
        assert_eq!(saved(&db, jump), "jump-new");
        assert_eq!(saved(&db, target), "target-old", "没重新输入的那一跳不动");
    }

    #[test]
    fn no_updates_means_the_vault_is_not_touched() {
        let db = Database::open_in_memory().expect("内存库");
        let host = password_host(&db, "web", "old");
        assert_eq!(apply(&db, &[]), Ok(0));
        assert_eq!(saved(&db, host), "old");
    }

    #[test]
    fn a_host_that_vanished_reports_an_error_but_other_updates_still_apply() {
        let db = Database::open_in_memory().expect("内存库");
        let host = password_host(&db, "web", "old");
        let updates = [
            Update {
                host_id: 99_999,
                password: "x".to_owned(),
            },
            Update {
                host_id: host,
                password: "new".to_owned(),
            },
        ];
        assert!(apply(&db, &updates).is_err());
        assert_eq!(saved(&db, host), "new");
    }
}
