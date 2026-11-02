//! 调试用的种子数据：往（独立的）数据目录里的主机库写几台主机，供截图和联网验证用。
//! 只在 debug 构建里读环境变量（见 `dev_hooks`），写的是 `ZSSH_DATA_DIR` 指向的库，从不碰真实数据。
//!
//! - `ZSSH_SEED_TEST_HOST=<名字>`：按 `ZSSH_TEST_HOST / PORT / USER / KEY_PATH`（以及可选的
//!   `ZSSH_TEST_SOCKS5`）建一台连测试服务器的主机，起这个名字（截图里叫 `web-1`）。已存在同名主机就跳过。
//!   地址只在环境变量和库文件里，不进代码、不进日志。
//! - `ZSSH_SEED_PLACEHOLDERS=1`：建几组演示用的主机，地址一律是文档保留段（RFC 5737），
//!   不会真的连出去；主机页截图用它，画面里不会出现真实地址。

use zssh_core::model::{
    AuthMethod, GroupDraft, HostDraft, IdentityDraft, IdentityKind, ProxyDraft, ProxyKind,
    ProxyMode,
};
use zssh_core::store::Database;
use zssh_core::store::forwards::{ForwardKind, PortForwardDraft};

fn env(name: &str) -> Option<String> {
    crate::dev_hooks::var(name).filter(|value| !value.trim().is_empty())
}

/// 读取环境变量并写库；任何一步失败只返回原因，不影响启动。
pub fn seed_from_env() -> Result<(), String> {
    let test_host = env("ZSSH_SEED_TEST_HOST");
    let placeholders = env("ZSSH_SEED_PLACEHOLDERS").is_some();
    if test_host.is_none() && !placeholders {
        return Ok(());
    }
    let db = crate::hosts_demo::open_database().map_err(|err| err.to_string())?;
    if let Some(name) = test_host {
        seed_test_host(&db, &name)?;
    }
    if placeholders {
        seed_placeholders(&db)?;
    }
    Ok(())
}

fn host_draft(name: &str, address: &str, port: u16, user: &str, group: Option<i64>) -> HostDraft {
    HostDraft {
        protocol: None,
        id: None,
        group_id: group,
        name: name.to_owned(),
        host: address.to_owned(),
        port,
        username: user.to_owned(),
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

fn seed_test_host(db: &Database, name: &str) -> Result<(), String> {
    let exists = db
        .host_list()
        .map_err(|err| err.to_string())?
        .iter()
        .any(|host| host.name == name);
    if exists {
        return Ok(());
    }
    let (Some(address), Some(user), Some(key)) = (
        env("ZSSH_TEST_HOST"),
        env("ZSSH_TEST_USER"),
        env("ZSSH_TEST_KEY_PATH"),
    ) else {
        return Err("缺少 ZSSH_TEST_HOST / ZSSH_TEST_USER / ZSSH_TEST_KEY_PATH".to_owned());
    };
    let port = env("ZSSH_TEST_PORT")
        .and_then(|raw| raw.parse::<u16>().ok())
        .unwrap_or(22);
    let identity = db
        .identity_save(&IdentityDraft {
            id: None,
            name: format!("{name}-key"),
            kind: IdentityKind::KeyFile,
            key_path: Some(key),
            secret: None,
            passphrase: None,
        })
        .map_err(|err| err.to_string())?;
    let mut draft = host_draft(name, &address, port, &user, None);
    draft.auth_method = AuthMethod::Key;
    draft.identity_id = Some(identity.id);
    // 直连不通时可以经本机的 SOCKS5 转发（测试服务器有些网络要这样）。
    if let Some(raw) = env("ZSSH_TEST_SOCKS5")
        && let Some((proxy_host, proxy_port)) = raw.rsplit_once(':')
        && let Ok(proxy_port) = proxy_port.parse::<u16>()
    {
        let proxy = db
            .proxy_save(&ProxyDraft {
                id: None,
                name: "test-proxy".to_owned(),
                kind: ProxyKind::Socks5,
                host: proxy_host.to_owned(),
                port: proxy_port,
                username: None,
                secret: None,
            })
            .map_err(|err| err.to_string())?;
        draft.proxy_mode = ProxyMode::Custom;
        draft.proxy_id = Some(proxy.id);
    }
    db.host_save(&draft).map_err(|err| err.to_string())?;
    Ok(())
}

fn seed_placeholders(db: &Database) -> Result<(), String> {
    let existing = db.host_list().map_err(|err| err.to_string())?;
    // 已经有分组就不再建（重复启动不叠加）；同名的主机（比如先建好的测试主机）也跳过。
    let existing_groups = db.group_list().map_err(|err| err.to_string())?;
    let group = |name: &str| -> Result<i64, String> {
        if let Some(found) = existing_groups.iter().find(|group| group.name == name) {
            return Ok(found.id);
        }
        db.group_save(&GroupDraft {
            id: None,
            name: name.to_owned(),
            parent_id: None,
            sort: 0,
        })
        .map(|group| group.id)
        .map_err(|err| err.to_string())
    };
    let prod = group("生产环境")?;
    let staging = group("预发布")?;
    let rows: [(&str, &str, &str, Option<i64>); 8] = [
        ("web-1", "192.0.2.11", "deploy", Some(prod)),
        ("web-2", "192.0.2.12", "deploy", Some(prod)),
        ("db-1", "192.0.2.21", "postgres", Some(prod)),
        ("cache-1", "192.0.2.31", "ops", Some(prod)),
        ("web-staging", "198.51.100.11", "deploy", Some(staging)),
        ("db-staging", "198.51.100.21", "postgres", Some(staging)),
        ("gateway", "203.0.113.5", "admin", None),
        ("backup-1", "203.0.113.9", "backup", None),
    ];
    for (name, address, user, group_id) in rows {
        if existing.iter().any(|host| host.name == name) {
            continue;
        }
        db.host_save(&host_draft(name, address, 22, user, group_id))
            .map_err(|err| err.to_string())?;
    }
    seed_placeholder_extras(db)
}

/// 凭据、代理、转发规则的演示数据：只在库里还没有转发规则时建一次。秘密一律是占位文字（不是真密码），
/// 地址都是文档保留段，不会真的连出去。
fn seed_placeholder_extras(db: &Database) -> Result<(), String> {
    // 已经有转发规则就是建过了（测试主机的凭据会先占着凭据表，不能拿它判断）。
    if !db.forward_list().map_err(|err| err.to_string())?.is_empty() {
        return Ok(());
    }
    let password = db
        .identity_save(&IdentityDraft {
            id: None,
            name: "运维账号".to_owned(),
            kind: IdentityKind::Password,
            key_path: None,
            secret: Some("placeholder-not-a-real-password".to_owned()),
            passphrase: None,
        })
        .map_err(|err| err.to_string())?;
    db.identity_save(&IdentityDraft {
        id: None,
        name: "部署密钥".to_owned(),
        kind: IdentityKind::KeyFile,
        key_path: Some(r"C:\keys\deploy_ed25519".to_owned()),
        secret: None,
        passphrase: None,
    })
    .map_err(|err| err.to_string())?;
    db.identity_save(&IdentityDraft {
        id: None,
        name: "本机 agent".to_owned(),
        kind: IdentityKind::Agent,
        key_path: None,
        secret: None,
        passphrase: None,
    })
    .map_err(|err| err.to_string())?;
    let office = db
        .proxy_save(&ProxyDraft {
            id: None,
            name: "办公室代理".to_owned(),
            kind: ProxyKind::Socks5,
            host: "198.51.100.1".to_owned(),
            port: 1080,
            username: None,
            secret: None,
        })
        .map_err(|err| err.to_string())?;
    db.proxy_save(&ProxyDraft {
        id: None,
        name: "出口 HTTP 代理".to_owned(),
        kind: ProxyKind::Http,
        host: "198.51.100.2".to_owned(),
        port: 3128,
        username: Some("proxy-user".to_owned()),
        secret: Some("placeholder-proxy-secret".to_owned()),
    })
    .map_err(|err| err.to_string())?;
    // 让 web-1 用上密码凭据、db-1 走办公室代理，主机详情 / 属性面板里有东西可看。
    let hosts = db.host_list().map_err(|err| err.to_string())?;
    if let Some(web) = hosts.iter().find(|host| host.name == "web-1") {
        let mut draft = host_draft(&web.name, &web.host, web.port, &web.username, web.group_id);
        draft.id = Some(web.id);
        draft.identity_id = Some(password.id);
        draft.tags = vec!["线上".to_owned(), "nginx".to_owned()];
        db.host_save(&draft).map_err(|err| err.to_string())?;
        db.forward_save(&PortForwardDraft {
            id: None,
            host_id: web.id,
            name: "本地网页".to_owned(),
            kind: ForwardKind::Local,
            bind_host: "127.0.0.1".to_owned(),
            bind_port: 18080,
            dest_host: "localhost".to_owned(),
            dest_port: 80,
            auto_start: false,
        })
        .map_err(|err| err.to_string())?;
    }
    if let Some(db1) = hosts.iter().find(|host| host.name == "db-1") {
        let mut draft = host_draft(&db1.name, &db1.host, db1.port, &db1.username, db1.group_id);
        draft.id = Some(db1.id);
        draft.proxy_mode = ProxyMode::Custom;
        draft.proxy_id = Some(office.id);
        draft.jump_host_id = hosts
            .iter()
            .find(|host| host.name == "gateway")
            .map(|host| host.id);
        db.host_save(&draft).map_err(|err| err.to_string())?;
        db.forward_save(&PortForwardDraft {
            id: None,
            host_id: db1.id,
            name: "数据库".to_owned(),
            kind: ForwardKind::Local,
            bind_host: "127.0.0.1".to_owned(),
            bind_port: 15432,
            dest_host: "localhost".to_owned(),
            dest_port: 5432,
            auto_start: true,
        })
        .map_err(|err| err.to_string())?;
        db.forward_save(&PortForwardDraft {
            id: None,
            host_id: db1.id,
            name: String::new(),
            kind: ForwardKind::Remote,
            bind_host: "0.0.0.0".to_owned(),
            bind_port: 9000,
            dest_host: "localhost".to_owned(),
            dest_port: 9000,
            auto_start: false,
        })
        .map_err(|err| err.to_string())?;
    }
    Ok(())
}
