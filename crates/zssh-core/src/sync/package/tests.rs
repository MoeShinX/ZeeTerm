use super::*;

fn group(uid: &str, name: &str) -> GroupRecord {
    GroupRecord {
        uid: uid.to_owned(),
        name: name.to_owned(),
        parent_uid: None,
        sort: 0,
        updated_at: 10,
        device_id: "dev-a".to_owned(),
    }
}

#[test]
fn package_roundtrips_and_rejects_newer_schema_version() {
    let mut snapshot = SyncSnapshot::empty("dev-a", 1_760_000_000);
    snapshot.groups.push(group("g1", "生产"));
    let bytes = snapshot.to_json().expect("序列化");
    assert_eq!(SyncSnapshot::parse(&bytes).expect("解析"), snapshot);

    // 写出去的版本号按内容算，所以用改 JSON 的办法造"别的版本"的包。
    let with_version = |version: u32| {
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["schema_version"] = serde_json::json!(version);
        serde_json::to_vec(&value).unwrap()
    };
    let err =
        SyncSnapshot::parse(&with_version(MAX_SNAPSHOT_VERSION + 1)).expect_err("版本太新要拒绝");
    assert!(
        matches!(err, SyncError::FormatTooNew { found, supported } if found == MAX_SNAPSHOT_VERSION + 1 && supported == MAX_SNAPSHOT_VERSION),
        "{err}"
    );
    SyncSnapshot::parse(&with_version(SNAPSHOT_VERSION_PROTOCOLS)).expect("v2 本版本能读");

    let err = SyncSnapshot::parse(&with_version(0)).expect_err("版本太旧要拒绝");
    assert!(matches!(err, SyncError::FormatTooOld { found: 0 }), "{err}");
}

#[test]
fn local_key_paths_bump_the_version_and_are_not_put_on_the_wire() {
    let mut snapshot = SyncSnapshot::empty("dev-a", 1_760_000_000);
    snapshot.identities.push(IdentityRecord {
        uid: "i1".to_owned(),
        name: "key".to_owned(),
        kind: "key".to_owned(),
        key_path: Some("/home/user/.ssh/id_ed25519".to_owned()),
        secret: None,
        updated_at: 10,
        device_id: "dev-a".to_owned(),
    });
    assert_eq!(snapshot.required_version(), SNAPSHOT_VERSION_LOCAL_KEY_PATH);
    let bytes = snapshot.to_json().expect("序列化");
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["schema_version"],
        serde_json::json!(SNAPSHOT_VERSION_LOCAL_KEY_PATH)
    );
    assert!(value["identities"][0].get("key_path").is_none());
    // 旧客户端只认到 v3，必须拒绝这份包，而不是把缺字段当成 None 清空路径。
    let parsed = SyncSnapshot::parse(&bytes).expect("本版本能读");
    assert_eq!(parsed.identities[0].key_path, None);
}

#[test]
fn snapshot_seals_and_opens_with_the_master_password() {
    let mut snapshot = SyncSnapshot::empty("dev-a", 1_760_000_000);
    snapshot.groups.push(group("g1", "生产"));
    let sealed = snapshot.seal("master-password").expect("加密");
    assert_eq!(
        SyncSnapshot::open(&sealed, "master-password").expect("解密"),
        snapshot
    );
    assert!(matches!(
        SyncSnapshot::open(&sealed, "wrong").expect_err("密码错"),
        SyncError::WrongPassword
    ));
}

#[test]
fn tombstones_expire_after_ninety_days() {
    let tombstone = Tombstone {
        entity: ENTITY_HOST.to_owned(),
        key: "h1".to_owned(),
        deleted_at: 1_000_000,
        device_id: "dev-a".to_owned(),
    };
    assert!(!tombstone.is_expired(1_000_000 + TOMBSTONE_RETENTION_SECS));
    assert!(tombstone.is_expired(1_000_000 + TOMBSTONE_RETENTION_SECS + 1));
    // 本机时钟往回跳：按未过期处理，不能因此复活记录。
    assert!(!tombstone.is_expired(999_000));
}

#[test]
fn canonical_order_does_not_depend_on_insertion_order() {
    let mut a = SyncSnapshot::empty("dev-a", 1);
    a.groups.push(group("b", "乙"));
    a.groups.push(group("a", "甲"));
    a.hosts.push(HostRecord {
        uid: "h2".to_owned(),
        group_uid: None,
        name: "web-2".to_owned(),
        host: "web2.example.internal".to_owned(),
        port: 22,
        username: "root".to_owned(),
        auth_method: "password".to_owned(),
        identity_uid: None,
        proxy_uid: None,
        jump_host_uid: None,
        proxy_mode: "default".to_owned(),
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
        protocol: "ssh".to_owned(),
        x11_forwarding: false,
        protocol_options: None,
        created_at: 1,
        updated_at: 1,
        device_id: "dev-a".to_owned(),
    });
    let mut b = a.clone();
    b.groups.reverse();
    a.sort_canonical();
    b.sort_canonical();
    assert_eq!(a.to_json().unwrap(), b.to_json().unwrap());
    assert_eq!(a.record_count(), 3);
}

#[test]
fn content_comparison_ignores_timestamps_and_device() {
    let a = group("g1", "生产");
    let mut b = a.clone();
    b.updated_at = 999;
    b.device_id = "dev-b".to_owned();
    assert!(a.same_content(&b), "时间和设备不参与“内容改没改”的判断");
    b.name = "测试".to_owned();
    assert!(!a.same_content(&b));
}

#[test]
fn old_packages_without_new_fields_still_parse() {
    // 只带必填字段的最小包：新增的可选字段要有默认值，不能因为缺字段就解不开。
    let json = r#"{
        "schema_version": 1,
        "device_id": "dev-a",
        "created_at": 1,
        "groups": [{"uid": "g1", "name": "生产"}],
        "hosts": [],
        "identities": [],
        "proxies": [],
        "settings": {}
    }"#;
    let snapshot =
        SyncSnapshot::parse(json.as_bytes()).expect("缺 known_hosts/tombstones 也要能解析");
    assert_eq!(snapshot.groups.len(), 1);
    assert!(snapshot.known_hosts.is_empty());
    assert!(snapshot.tombstones.is_empty());
}

#[test]
fn package_records_do_not_leak_secrets_in_debug_output() {
    // 同步包里的凭据 / 代理记录带明文 secret，Debug 输出不能带它（R-8 第 11 节）。
    let identity = IdentityRecord {
        uid: "i1".to_owned(),
        name: "prod".to_owned(),
        kind: "password".to_owned(),
        key_path: None,
        secret: Some("s3cret-identity".to_owned()),
        updated_at: 1,
        device_id: "dev-a".to_owned(),
    };
    let debug = format!("{identity:?}");
    assert!(!debug.contains("s3cret"), "{debug}");
    assert!(debug.contains("***"), "{debug}");
    assert!(debug.contains("prod"), "非密码字段照常显示：{debug}");

    let proxy = ProxyRecord {
        uid: "p1".to_owned(),
        name: "corp".to_owned(),
        kind: "socks5".to_owned(),
        host: "proxy.example.com".to_owned(),
        port: 1080,
        username: Some("deploy".to_owned()),
        secret: Some("s3cret-proxy".to_owned()),
        updated_at: 1,
        device_id: "dev-a".to_owned(),
    };
    let debug = format!("{proxy:?}");
    assert!(!debug.contains("s3cret"), "{debug}");
    assert!(debug.contains("***"), "{debug}");
    assert!(debug.contains("proxy.example.com"), "{debug}");
}

#[test]
fn a_host_without_a_proxy_mode_follows_the_default_proxy() {
    let mut snapshot = SyncSnapshot::empty("dev-a", 1);
    for (uid, mode) in [("h1", "default"), ("h2", "none"), ("h3", "custom")] {
        snapshot.hosts.push(HostRecord {
            uid: uid.to_owned(),
            group_uid: None,
            name: uid.to_owned(),
            host: "web.example.internal".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: "password".to_owned(),
            identity_uid: None,
            proxy_uid: None,
            jump_host_uid: None,
            proxy_mode: mode.to_owned(),
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            protocol: "ssh".to_owned(),
            x11_forwarding: false,
            protocol_options: None,
            created_at: 1,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        });
    }
    let bytes = snapshot.to_json().expect("序列化");
    // 写了的值原样读回。
    let back = SyncSnapshot::parse(&bytes).expect("解析");
    let modes: Vec<&str> = back.hosts.iter().map(|h| h.proxy_mode.as_str()).collect();
    assert_eq!(modes, ["default", "none", "custom"]);
    // 没写这个字段的包（旧版本 / 别的来源）：落成「跟随默认代理」，不是空串。
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    for host in value["hosts"].as_array_mut().unwrap() {
        host.as_object_mut().unwrap().remove("proxy_mode");
    }
    let stripped = SyncSnapshot::parse(&serde_json::to_vec(&value).unwrap()).expect("解析");
    assert!(stripped.hosts.iter().all(|h| h.proxy_mode == "default"));
}
