use crate::model::{
    AuthMethod, HostDraft, IdentityDraft, IdentityKind, Protocol, ProtocolSettings, ProxyDraft,
    ProxyKind, ProxyMode, SerialParity, TelnetNewline,
};
use crate::store::Database;

fn memory_db() -> Database {
    Database::open_in_memory().expect("内存库")
}

fn draft(name: &str) -> HostDraft {
    HostDraft {
        id: None,
        group_id: None,
        name: name.to_owned(),
        host: "example.internal".to_owned(),
        port: 22,
        username: "deploy".to_owned(),
        auth_method: AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::None,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

fn password_identity(db: &Database) -> i64 {
    db.identity_save(&IdentityDraft {
        id: None,
        name: "pw".to_owned(),
        kind: IdentityKind::Password,
        key_path: None,
        secret: Some("s3cret".to_owned()),
        passphrase: None,
    })
    .expect("密码凭据")
    .id
}

fn key_identity(db: &Database) -> i64 {
    db.identity_save(&IdentityDraft {
        id: None,
        name: "key".to_owned(),
        kind: IdentityKind::KeyFile,
        key_path: Some("k".to_owned()),
        secret: None,
        passphrase: None,
    })
    .expect("私钥凭据")
    .id
}

fn err_text<T>(result: Result<T, crate::error::ZsshError>) -> String {
    match result {
        Ok(_) => panic!("应该被拒绝"),
        Err(err) => err.to_string(),
    }
}

fn row_columns(db: &Database, id: i64) -> (String, i64, Option<String>) {
    db.with_conn(|conn| {
        Ok(conn.query_row(
            "SELECT protocol, x11_forwarding, protocol_options FROM hosts WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    })
    .expect("读三列")
}

fn exec(db: &Database, sql: &str, params: impl rusqlite::Params) {
    db.with_conn(|conn| {
        conn.execute(sql, params)?;
        Ok(())
    })
    .expect("直接改库");
}

#[test]
fn existing_hosts_become_plain_ssh_after_the_migration() {
    let dir = crate::test_util::TestDir::new("test-protocol-migration");
    let path = dir.join("legacy.db");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("legacy-v6.db");
    std::fs::copy(&fixture, &path).expect("复制老库");

    let db = Database::open(&path).expect("老库升级");
    assert_eq!(db.schema_version().unwrap(), 12);
    let hosts = db.host_list().unwrap();
    assert!(!hosts.is_empty(), "老数据要留着");
    let map = db.host_protocol_map().unwrap();
    assert_eq!(map.len(), hosts.len());
    for host in &hosts {
        let settings = &map[&host.id];
        assert!(settings.is_plain_ssh(), "{}: {settings:?}", host.name);
        let (protocol, x11, options) = row_columns(&db, host.id);
        assert_eq!((protocol.as_str(), x11, options), ("ssh", 0, None));
    }
    // 老字段原样（引用关系没被迁移弄坏）。
    let web = db.host_get(2).unwrap();
    assert_eq!(web.name, "web-1");
    assert_eq!(web.jump_host_id, Some(1));
    assert!(web.legacy_algorithms);
}

#[test]
fn upgrading_a_version_eleven_database_keeps_hosts_and_adds_the_columns() {
    let dir = crate::test_util::TestDir::new("test-protocol-v11");
    let path = dir.join("v11.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("建库");
        for (version, sql) in crate::store::db::MIGRATIONS.iter().take(11) {
            conn.execute_batch(sql).expect("跑 1-11 号迁移");
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .expect("记版本号");
        }
        conn.execute(
            "INSERT INTO hosts (uid, name, host, port, username, auth_method, sort, created_at, \
             updated_at, proxy_mode, legacy_algorithms) \
             VALUES ('u-1', 'old', 'h', 22, 'root', 'password', 0, 1, 1, 'default', 0)",
            [],
        )
        .expect("老主机");
    }
    let db = Database::open(&path).expect("升级");
    assert_eq!(db.schema_version().unwrap(), 12);
    let host = &db.host_list().unwrap()[0];
    assert_eq!(host.name, "old");
    assert!(db.host_protocol_get(host.id).unwrap().is_plain_ssh());
    // 再打开一次（迁移不重复跑）。
    drop(db);
    Database::open(&path).expect("重复打开");
}

#[test]
fn plain_save_creates_a_plain_ssh_host() {
    let db = memory_db();
    let host = db.host_save(&draft("web-1")).unwrap();
    assert!(db.host_protocol_get(host.id).unwrap().is_plain_ssh());
    assert_eq!(row_columns(&db, host.id).2, None);
}

#[test]
fn telnet_host_roundtrips_with_its_options() {
    let db = memory_db();
    let identity = password_identity(&db);
    let mut settings = ProtocolSettings::new(Protocol::Telnet);
    {
        let options = settings.telnet.as_mut().unwrap();
        options.newline = TelnetNewline::Lf;
        options.auto_login.enabled = true;
    }
    let mut request = draft("switch-1");
    request.port = 23;
    request.identity_id = Some(identity);
    request.proxy_mode = ProxyMode::Default;

    let (host, saved) = db.host_save_with_protocol(&request, &settings).unwrap();
    assert_eq!(host.port, 23);
    assert_eq!(saved, settings);
    assert_eq!(db.host_protocol_get(host.id).unwrap(), settings);
    let (protocol, x11, options) = row_columns(&db, host.id);
    assert_eq!((protocol.as_str(), x11), ("telnet", 0));
    assert!(options.expect("有选项").contains("\"newline\":\"lf\""));
}

#[test]
fn telnet_allows_a_blank_username_but_ssh_does_not() {
    let db = memory_db();
    let mut request = draft("switch-2");
    request.username = String::new();
    let (host, _) = db
        .host_save_with_protocol(&request, &ProtocolSettings::new(Protocol::Telnet))
        .unwrap();
    assert_eq!(host.username, "");

    let mut ssh = draft("web-1");
    ssh.username = " ".to_owned();
    assert!(err_text(db.host_save(&ssh)).contains("用户名"));
    assert!(
        err_text(db.host_save_with_protocol(&ssh, &ProtocolSettings::default())).contains("用户名")
    );
}

fn serial_settings(device: &str) -> ProtocolSettings {
    let mut settings = ProtocolSettings::new(Protocol::Serial);
    settings.serial.as_mut().unwrap().device = device.to_owned();
    settings
}

#[test]
fn serial_host_roundtrips_and_stores_placeholders_for_the_network_columns() {
    let db = memory_db();
    let mut settings = serial_settings("COM3");
    {
        let options = settings.serial.as_mut().unwrap();
        options.baud_rate = 9600;
        options.parity = SerialParity::Even;
        options.local_echo = true;
    }
    // 界面带来的默认值（地址 / 端口 / 用户名 / 默认代理策略）不算数，落库的是占位值。
    let mut request = draft("console-1");
    request.host = "ignored.example.internal".to_owned();
    request.port = 22;
    request.proxy_mode = ProxyMode::Default;
    let (host, saved) = db.host_save_with_protocol(&request, &settings).unwrap();
    assert_eq!(saved, settings);
    assert_eq!(
        (host.host.as_str(), host.port, host.username.as_str()),
        ("", 0, "")
    );
    assert_eq!(host.proxy_mode, ProxyMode::None);
    assert_eq!(host.auth_method, AuthMethod::Password);
    assert_eq!(db.host_protocol_get(host.id).unwrap(), settings);
    let options = row_columns(&db, host.id).2.expect("有选项");
    assert!(
        options.contains("\"serial\"") && options.contains("\"COM3\""),
        "{options}"
    );

    // 老入口再存（界面不知道串口，带着一堆默认值）：协议设置不变，占位值不被冲掉。
    let mut edit = draft("console-1-renamed");
    edit.id = Some(host.id);
    let renamed = db.host_save(&edit).unwrap();
    assert_eq!(renamed.name, "console-1-renamed");
    assert_eq!((renamed.host.as_str(), renamed.port), ("", 0));
    assert_eq!(db.host_protocol_get(host.id).unwrap(), settings);
}

#[test]
fn serial_host_rejects_identity_proxy_jump_host_and_bad_options() {
    let db = memory_db();
    let pw = password_identity(&db);
    let proxy = db
        .proxy_save(&ProxyDraft {
            id: None,
            name: "p".to_owned(),
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".to_owned(),
            port: 1080,
            username: None,
            secret: None,
        })
        .unwrap();
    let bastion = db.host_save(&draft("bastion")).unwrap();
    let settings = serial_settings("COM3");
    let before = db.host_list().unwrap().len();

    let mut request = draft("c");
    request.identity_id = Some(pw);
    assert!(err_text(db.host_save_with_protocol(&request, &settings)).contains("凭据"));
    let mut request = draft("c");
    request.proxy_mode = ProxyMode::Custom;
    request.proxy_id = Some(proxy.id);
    assert!(err_text(db.host_save_with_protocol(&request, &settings)).contains("代理"));
    let mut request = draft("c");
    request.jump_host_id = Some(bastion.id);
    assert!(err_text(db.host_save_with_protocol(&request, &settings)).contains("跳板机"));

    // 没填设备名 / 选项不合法。
    assert!(err_text(
        db.host_save_with_protocol(&draft("c"), &ProtocolSettings::new(Protocol::Serial))
    )
    .contains("设备名"));
    let mut bad = serial_settings("COM3");
    bad.serial.as_mut().unwrap().baud_rate = 0;
    assert!(err_text(db.host_save_with_protocol(&draft("c"), &bad)).contains("波特率"));
    assert_eq!(db.host_list().unwrap().len(), before, "全部整条不落库");
}

#[test]
fn serial_and_network_hosts_cannot_be_converted_into_each_other() {
    let db = memory_db();
    let ssh = db.host_save(&draft("web")).unwrap();
    let (serial, _) = db
        .host_save_with_protocol(&draft("console"), &serial_settings("COM3"))
        .unwrap();
    assert!(
        err_text(db.host_protocol_set(ssh.id, &serial_settings("COM4"))).contains("不能互相转换")
    );
    assert!(
        err_text(db.host_protocol_set(serial.id, &ProtocolSettings::new(Protocol::Telnet)))
            .contains("不能互相转换")
    );
    // 串口改串口选项可以。
    let mut other = serial_settings("COM9");
    other.serial.as_mut().unwrap().baud_rate = 57_600;
    db.host_protocol_set(serial.id, &other).unwrap();
    assert_eq!(db.host_protocol_get(serial.id).unwrap(), other);
    // 串口主机没有网络登录信息。
    assert!(err_text(db.host_login(serial.id)).contains("串口"));
}

#[test]
fn a_serial_host_cannot_be_a_jump_host() {
    let db = memory_db();
    let (serial, _) = db
        .host_save_with_protocol(&draft("console"), &serial_settings("COM3"))
        .unwrap();
    let mut request = draft("inner");
    request.jump_host_id = Some(serial.id);
    assert!(err_text(db.host_save(&request)).contains("跳板机必须是 SSH"));
}

#[test]
fn x11_roundtrips_on_ssh_and_is_rejected_elsewhere() {
    let db = memory_db();
    let on = ProtocolSettings {
        x11_forwarding: true,
        ..ProtocolSettings::default()
    };
    let (host, _) = db.host_save_with_protocol(&draft("gui-1"), &on).unwrap();
    assert!(db.host_protocol_get(host.id).unwrap().x11_enabled());
    assert_eq!(row_columns(&db, host.id).1, 1);

    let telnet = ProtocolSettings {
        x11_forwarding: true,
        ..ProtocolSettings::new(Protocol::Telnet)
    };
    assert!(err_text(db.host_save_with_protocol(&draft("t"), &telnet)).contains("X11"));
    assert!(err_text(db.host_protocol_set(host.id, &telnet)).contains("X11"));
    assert!(
        db.host_protocol_get(host.id).unwrap().x11_enabled(),
        "校验失败不能改动已有设置"
    );
}

#[test]
fn invalid_combination_saves_nothing() {
    let db = memory_db();
    let key = key_identity(&db);
    let mut request = draft("bad");
    request.auth_method = AuthMethod::Key;
    request.identity_id = Some(key);
    let before = db.host_list().unwrap().len();
    let text =
        err_text(db.host_save_with_protocol(&request, &ProtocolSettings::new(Protocol::Telnet)));
    assert!(text.contains("密码认证"), "{text}");
    assert_eq!(
        db.host_list().unwrap().len(),
        before,
        "不合法的组合整条不落库"
    );
}

#[test]
fn plain_save_keeps_the_stored_protocol_but_rechecks_the_combination() {
    let db = memory_db();
    let telnet = ProtocolSettings::new(Protocol::Telnet);
    let (host, _) = db.host_save_with_protocol(&draft("sw-1"), &telnet).unwrap();

    // 只改名字：协议设置原样。
    let mut edit = draft("sw-1-renamed");
    edit.id = Some(host.id);
    db.host_save(&edit).expect("改名");
    assert_eq!(db.host_protocol_get(host.id).unwrap(), telnet);

    // 借旧入口把 Telnet 主机改成私钥认证：必须被拒。
    let key = key_identity(&db);
    let mut bad = edit;
    bad.auth_method = AuthMethod::Key;
    bad.identity_id = Some(key);
    assert!(err_text(db.host_save(&bad)).contains("密码"));
}

#[test]
fn saving_does_not_rewrite_options_when_the_protocol_is_untouched() {
    // 选项里有这一版不认识的键：只改名字不能把它洗掉。
    let db = memory_db();
    let (host, _) = db
        .host_save_with_protocol(&draft("t"), &ProtocolSettings::new(Protocol::Telnet))
        .unwrap();
    let raw = r#"{"telnet":{"newline":"lf","future_flag":true}}"#;
    exec(
        &db,
        "UPDATE hosts SET protocol_options = ?1 WHERE id = ?2",
        rusqlite::params![raw, host.id],
    );
    let mut edit = draft("t2");
    edit.id = Some(host.id);
    db.host_save(&edit).unwrap();
    assert_eq!(row_columns(&db, host.id).2.as_deref(), Some(raw));
}

#[test]
fn telnet_hosts_can_jump_through_ssh_hosts_only() {
    let db = memory_db();
    let bastion = db.host_save(&draft("bastion")).unwrap();
    let telnet_host = db
        .host_save_with_protocol(
            &draft("old-switch"),
            &ProtocolSettings::new(Protocol::Telnet),
        )
        .unwrap()
        .0;

    // 经 SSH 跳板机：Telnet / SSH 都行。
    for (name, settings) in [
        ("t-via", ProtocolSettings::new(Protocol::Telnet)),
        ("s-via", ProtocolSettings::default()),
    ] {
        let mut request = draft(name);
        request.jump_host_id = Some(bastion.id);
        let (host, _) = db.host_save_with_protocol(&request, &settings).unwrap();
        let chain = db.host_chain(host.id).unwrap();
        assert_eq!(chain.len(), 2, "{name}");
        assert_eq!(chain[0].id, bastion.id);
    }

    // 经 Telnet 主机跳：保存时拒绝。
    let mut request = draft("via-telnet");
    request.jump_host_id = Some(telnet_host.id);
    assert!(err_text(db.host_save(&request)).contains("跳板机必须是 SSH"));
    assert!(err_text(
        db.host_save_with_protocol(&request, &ProtocolSettings::new(Protocol::Telnet))
    )
    .contains("跳板机必须是 SSH"));
}

#[test]
fn a_host_used_as_jump_host_cannot_stop_being_ssh() {
    let db = memory_db();
    let bastion = db.host_save(&draft("bastion")).unwrap();
    let mut inner = draft("inner");
    inner.jump_host_id = Some(bastion.id);
    db.host_save(&inner).unwrap();

    let text = err_text(db.host_protocol_set(bastion.id, &ProtocolSettings::new(Protocol::Telnet)));
    assert!(text.contains("inner") && text.contains("跳板机"), "{text}");
    let mut edit = draft("bastion");
    edit.id = Some(bastion.id);
    let text = err_text(db.host_save_with_protocol(&edit, &serial_settings("COM1")));
    assert!(text.contains("跳板机"), "{text}");
    assert!(db.host_protocol_get(bastion.id).unwrap().is_plain_ssh());
}

#[test]
fn host_chain_refuses_a_non_ssh_hop_that_slipped_in() {
    // 同步来的 / 手改的数据绕过了保存时的校验：拨号链这一道兜底。
    let db = memory_db();
    let bastion = db.host_save(&draft("bastion")).unwrap();
    let mut inner = draft("inner");
    inner.jump_host_id = Some(bastion.id);
    let inner = db.host_save(&inner).unwrap();
    exec(
        &db,
        "UPDATE hosts SET protocol = 'telnet' WHERE id = ?1",
        [bastion.id],
    );
    assert!(err_text(db.host_chain(inner.id)).contains("跳板机必须是 SSH"));
}

#[test]
fn protocol_set_validates_against_the_stored_host_and_bumps_updated_at() {
    let db = memory_db();
    let host = db.host_save(&draft("h")).unwrap();
    exec(
        &db,
        "UPDATE hosts SET updated_at = 5 WHERE id = ?1",
        [host.id],
    );

    // 这台主机是跟随默认代理。
    exec(
        &db,
        "UPDATE hosts SET proxy_mode = 'default' WHERE id = ?1",
        [host.id],
    );
    assert!(err_text(db.host_protocol_set(
        host.id,
        &ProtocolSettings {
            x11_forwarding: true,
            ..ProtocolSettings::new(Protocol::Telnet)
        }
    ))
    .contains("X11"));
    assert_eq!(
        db.host_get(host.id).unwrap().updated_at,
        5,
        "失败不改 updated_at"
    );

    // Telnet 可以走默认代理。
    db.host_protocol_set(host.id, &ProtocolSettings::new(Protocol::Telnet))
        .unwrap();
    assert!(
        db.host_get(host.id).unwrap().updated_at > 5,
        "同步靠 updated_at 看到改动"
    );

    assert!(matches!(
        db.host_protocol_set(9999, &ProtocolSettings::default()),
        Err(crate::error::ZsshError::NotFound { .. })
    ));
}

#[test]
fn ai_exec_refusal_only_for_non_ssh_hosts() {
    let db = memory_db();
    let ssh = db.host_save(&draft("web")).unwrap();
    let serial = db
        .host_save_with_protocol(&draft("console"), &serial_settings("COM3"))
        .unwrap()
        .0;
    let telnet = db
        .host_save_with_protocol(&draft("sw"), &ProtocolSettings::new(Protocol::Telnet))
        .unwrap()
        .0;
    assert_eq!(db.host_ai_exec_refusal(ssh.id).unwrap(), None);
    assert!(db
        .host_ai_exec_refusal(serial.id)
        .unwrap()
        .unwrap()
        .contains("串口"));
    assert!(db
        .host_ai_exec_refusal(telnet.id)
        .unwrap()
        .unwrap()
        .contains("Telnet"));
}
