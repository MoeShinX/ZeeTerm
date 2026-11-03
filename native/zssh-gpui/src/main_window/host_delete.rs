//! 删除主机时一并清掉它的密码（V2-U1 第 8 条）。
//!
//! 规则：
//! - 只动**密码类**凭据（身份库里的密码身份，包括保存主机时给它建的「<名称> 的密码」，V2-U11 起密码归主机）；
//!   **密钥一律不动**：密钥文件、内联私钥、ssh-agent 凭据都不在删除范围里；
//! - 这条密码还被**别的主机**用着就保留，确认框里写明「仍被 N 个主机使用，保留」。
//!   代理不会引用身份库里的密码（代理自己带一份密码，和主机删除无关），所以这里不用管代理；
//!   删主机也从来不会删它用的代理（代理是共享的）；
//! - 先删主机、再逐条删独占的密码：`identity_delete` 自己还会再查一遍有没有别人在引用，
//!   万一在两步之间被别处引用了就保留，不会误删；
//! - 确认框列出会一起删除的密码名称（`confirm_message` 用 [`DeletePlan`]）。
//!
//! 纯函数 [`plan`] 只读已经取出来的主机 / 凭据列表；[`delete_host_with_passwords`] 做实际的删除。

use zssh_core::model::{Host, Identity, IdentityKind};
use zssh_core::store::Database;

/// 一条会被一起删除的密码。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removable {
    pub id: i64,
    pub name: String,
}

/// 一条因为还被别的主机用着而保留的密码。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub name: String,
    /// 还有几台别的主机在用它。
    pub other_hosts: usize,
}

/// 删这台主机时对它的凭据怎么处理。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeletePlan {
    /// 独占的密码：一起删。
    pub remove: Vec<Removable>,
    /// 共享的密码：保留。
    pub keep: Vec<Kept>,
    /// 它用的密钥类凭据（不动）。
    pub keys: Vec<String>,
}

/// 算出删 `host_id` 时凭据的去留。主机或凭据对不上（已经被删了、列表过期）就当没有。
pub fn plan(host_id: i64, hosts: &[Host], identities: &[Identity]) -> DeletePlan {
    let mut plan = DeletePlan::default();
    let Some(host) = hosts.iter().find(|host| host.id == host_id) else {
        return plan;
    };
    let Some(identity_id) = host.identity_id else {
        return plan;
    };
    let Some(identity) = identities.iter().find(|item| item.id == identity_id) else {
        return plan;
    };
    match identity.kind {
        IdentityKind::Password => {
            let others = hosts
                .iter()
                .filter(|other| other.id != host_id && other.identity_id == Some(identity_id))
                .count();
            if others == 0 {
                plan.remove.push(Removable {
                    id: identity.id,
                    name: identity.name.clone(),
                });
            } else {
                plan.keep.push(Kept {
                    name: identity.name.clone(),
                    other_hosts: others,
                });
            }
        }
        // 密钥文件、内联私钥、ssh-agent：一律不动。
        IdentityKind::KeyFile | IdentityKind::KeyInline | IdentityKind::Agent => {
            plan.keys.push(identity.name.clone());
        }
    }
    plan
}

/// 确认框里关于凭据的几句话（没有要说的返回空）。
pub fn confirm_lines(plan: &DeletePlan) -> Vec<String> {
    let mut lines = Vec::new();
    if !plan.remove.is_empty() {
        let names: Vec<String> = plan
            .remove
            .iter()
            .map(|item| format!("「{}」", item.name))
            .collect();
        lines.push(format!(
            "它的密码{}没有别的主机在用，会一起删除。",
            names.join("、")
        ));
    }
    for kept in &plan.keep {
        lines.push(format!(
            "密码「{}」仍被 {} 台别的主机使用，保留。",
            kept.name, kept.other_hosts
        ));
    }
    if !plan.keys.is_empty() {
        let names: Vec<String> = plan.keys.iter().map(|name| format!("「{name}」")).collect();
        lines.push(format!("它用的密钥{}不会被删除。", names.join("、")));
    }
    lines
}

/// 实际删除的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    /// 一起删掉的密码名称。
    pub passwords: Vec<String>,
    /// 删密码时遇到的问题（主机已经删掉了，只是某条密码没删成）。
    pub problems: Vec<String>,
}

/// 删主机，并一起删它独占的密码。主机删不掉就整体失败（什么也没动）。
pub fn delete_host_with_passwords(db: &Database, host_id: i64) -> Result<Deleted, String> {
    let hosts = db.host_list().map_err(|err| err.to_string())?;
    let identities = db.identity_list().map_err(|err| err.to_string())?;
    let plan = plan(host_id, &hosts, &identities);
    db.host_delete(host_id).map_err(|err| err.to_string())?;
    let mut deleted = Deleted::default();
    for item in plan.remove {
        match db.identity_delete(item.id) {
            Ok(()) => deleted.passwords.push(item.name),
            // 两步之间被别处引用了：保留，不算错误。
            Err(zssh_core::error::ZsshError::IdentityInUse { .. }) => {}
            Err(err) => deleted
                .problems
                .push(format!("密码「{}」没删掉：{err}", item.name)),
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use zssh_core::model::{
        AuthMethod, HostDraft, IdentityDraft, IdentityKind, ProxyDraft, ProxyKind, ProxyMode,
    };
    use zssh_core::store::Database;

    use super::{confirm_lines, delete_host_with_passwords, plan};

    fn identity(db: &Database, name: &str, kind: IdentityKind, secret: Option<&str>) -> i64 {
        db.identity_save(&IdentityDraft {
            id: None,
            name: name.to_owned(),
            kind,
            key_path: (kind == IdentityKind::KeyFile).then(|| "keys/id_test".to_owned()),
            secret: secret.map(str::to_owned),
            passphrase: None,
        })
        .expect("凭据")
        .id
    }

    fn host(db: &Database, name: &str, auth: AuthMethod, identity_id: Option<i64>) -> i64 {
        host_with_proxy(db, name, auth, identity_id, None)
    }

    fn host_with_proxy(
        db: &Database,
        name: &str,
        auth: AuthMethod,
        identity_id: Option<i64>,
        proxy_id: Option<i64>,
    ) -> i64 {
        db.host_save(&HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: auth,
            identity_id,
            proxy_id,
            jump_host_id: None,
            proxy_mode: if proxy_id.is_some() {
                ProxyMode::Custom
            } else {
                ProxyMode::Default
            },
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        })
        .expect("主机")
        .id
    }

    fn names(db: &Database) -> Vec<String> {
        db.identity_list()
            .expect("列表")
            .into_iter()
            .map(|item| item.name)
            .collect()
    }

    #[test]
    fn an_exclusive_password_is_deleted_with_its_host_and_its_secret_goes_too() {
        let db = Database::open_in_memory().expect("内存库");
        let pw = identity(&db, "web-1 的密码", IdentityKind::Password, Some("hunter2"));
        let other = identity(&db, "别的密码", IdentityKind::Password, Some("x"));
        let web = host(&db, "web-1", AuthMethod::Password, Some(pw));
        host(&db, "db-1", AuthMethod::Password, Some(other));

        let hosts = db.host_list().expect("主机");
        let identities = db.identity_list().expect("凭据");
        let plan = plan(web, &hosts, &identities);
        assert_eq!(plan.remove.len(), 1);
        assert_eq!(plan.remove[0].name, "web-1 的密码");
        assert!(plan.keep.is_empty() && plan.keys.is_empty());
        assert_eq!(
            confirm_lines(&plan),
            ["它的密码「web-1 的密码」没有别的主机在用，会一起删除。"]
        );

        let deleted = delete_host_with_passwords(&db, web).expect("删除");
        assert_eq!(deleted.passwords, ["web-1 的密码"]);
        assert!(deleted.problems.is_empty());
        assert_eq!(names(&db), ["别的密码"], "只删了独占的那条");
        assert!(db.host_get(web).is_err(), "主机也没了");
        // 密文跟着凭据一起没了：再去取就是找不到。
        assert!(db.identity_saved_password(pw).is_err());
    }

    #[test]
    fn a_shared_password_stays_until_the_last_host_using_it_goes() {
        let db = Database::open_in_memory().expect("内存库");
        let pw = identity(&db, "共用密码", IdentityKind::Password, Some("shared"));
        let a = host(&db, "a", AuthMethod::Password, Some(pw));
        let b = host(&db, "b", AuthMethod::Password, Some(pw));
        let c = host(&db, "c", AuthMethod::Password, Some(pw));

        let hosts = db.host_list().expect("主机");
        let identities = db.identity_list().expect("凭据");
        let plan_a = plan(a, &hosts, &identities);
        assert!(plan_a.remove.is_empty());
        assert_eq!(plan_a.keep.len(), 1);
        assert_eq!(plan_a.keep[0].other_hosts, 2);
        assert_eq!(
            confirm_lines(&plan_a),
            ["密码「共用密码」仍被 2 台别的主机使用，保留。"]
        );

        let first = delete_host_with_passwords(&db, a).expect("删第一台");
        assert!(first.passwords.is_empty());
        assert_eq!(names(&db), ["共用密码"], "还有 b、c 在用：保留");
        delete_host_with_passwords(&db, b).expect("删第二台");
        assert_eq!(names(&db), ["共用密码"], "还有 c 在用：保留");
        let last = delete_host_with_passwords(&db, c).expect("删最后一台");
        assert_eq!(last.passwords, ["共用密码"], "最后一个用的人走了才一起删");
        assert!(names(&db).is_empty());
    }

    #[test]
    fn keys_are_never_touched_whatever_kind_they_are() {
        let db = Database::open_in_memory().expect("内存库");
        let file = identity(&db, "密钥文件", IdentityKind::KeyFile, None);
        let inline = identity(
            &db,
            "内联私钥",
            IdentityKind::KeyInline,
            Some("placeholder-inline-key-material"),
        );
        let agent = identity(&db, "agent", IdentityKind::Agent, None);
        let hosts_and_keys = [
            (host(&db, "k1", AuthMethod::Key, Some(file)), "密钥文件"),
            (host(&db, "k2", AuthMethod::Key, Some(inline)), "内联私钥"),
            (host(&db, "k3", AuthMethod::Agent, Some(agent)), "agent"),
        ];
        for (host_id, key_name) in hosts_and_keys {
            let hosts = db.host_list().expect("主机");
            let identities = db.identity_list().expect("凭据");
            let plan = plan(host_id, &hosts, &identities);
            assert!(plan.remove.is_empty(), "{key_name} 不能进删除名单");
            assert_eq!(plan.keys, [key_name]);
            assert_eq!(
                confirm_lines(&plan),
                [format!("它用的密钥「{key_name}」不会被删除。")]
            );
            let deleted = delete_host_with_passwords(&db, host_id).expect("删除");
            assert!(deleted.passwords.is_empty());
        }
        let mut left = names(&db);
        left.sort();
        assert_eq!(left, ["agent", "内联私钥", "密钥文件"], "三条都还在");
    }

    #[test]
    fn a_host_without_a_vault_entry_just_goes() {
        let db = Database::open_in_memory().expect("内存库");
        let lonely = host(&db, "lonely", AuthMethod::Password, None);
        let hosts = db.host_list().expect("主机");
        let plan = plan(lonely, &hosts, &[]);
        assert_eq!(plan, super::DeletePlan::default());
        assert!(confirm_lines(&plan).is_empty());
        assert!(
            delete_host_with_passwords(&db, lonely)
                .expect("删除")
                .passwords
                .is_empty()
        );
    }

    #[test]
    fn a_stale_list_changes_nothing() {
        let db = Database::open_in_memory().expect("内存库");
        let pw = identity(&db, "p", IdentityKind::Password, Some("x"));
        let web = host(&db, "web", AuthMethod::Password, Some(pw));
        let hosts = db.host_list().expect("主机");
        // 列表里没有这台主机（已经被删了）：什么也不删。
        assert_eq!(
            plan(web + 100, &hosts, &db.identity_list().expect("凭据")),
            super::DeletePlan::default()
        );
        // 凭据列表里找不到它引用的凭据：当没有。
        assert_eq!(plan(web, &hosts, &[]), super::DeletePlan::default());
    }

    #[test]
    fn deleting_a_host_never_deletes_its_proxy_or_the_proxys_own_password() {
        let db = Database::open_in_memory().expect("内存库");
        let proxy = db
            .proxy_save(&ProxyDraft {
                id: None,
                name: "公司代理".to_owned(),
                kind: ProxyKind::Socks5,
                host: "198.51.100.7".to_owned(),
                port: 1080,
                username: Some("u".to_owned()),
                secret: Some("proxy-pass".to_owned()),
            })
            .expect("代理");
        let pw = identity(&db, "主机密码", IdentityKind::Password, Some("pw"));
        let web = host_with_proxy(&db, "web", AuthMethod::Password, Some(pw), Some(proxy.id));
        let deleted = delete_host_with_passwords(&db, web).expect("删除");
        assert_eq!(deleted.passwords, ["主机密码"]);
        let proxies = db.proxy_list().expect("代理列表");
        assert_eq!(proxies.len(), 1, "代理是共享的，不跟着删");
        assert!(proxies[0].has_secret, "代理自己的密码还在");
    }

    #[test]
    fn deleting_a_missing_host_is_an_error_and_touches_no_password() {
        let db = Database::open_in_memory().expect("内存库");
        let pw = identity(&db, "p", IdentityKind::Password, Some("x"));
        host(&db, "web", AuthMethod::Password, Some(pw));
        assert!(delete_host_with_passwords(&db, 9999).is_err());
        assert_eq!(names(&db), ["p"]);
    }
}
