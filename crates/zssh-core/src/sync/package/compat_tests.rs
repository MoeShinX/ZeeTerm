//! 同步格式兼容：Z-SSH 0.2.14 读到带协议字段（Telnet / 串口 / X11）的包会怎样。
//!
//! 0.2.14 的解析逻辑（`sync/package.rs`，与 ZeeTerm 分叉时逐字节相同）冻结在 [`legacy_0214`]
//! 里当"旧客户端"用：它的 `HostRecord` 没有 `deny_unknown_fields`，所以遇到不认识的字段会
//! 丢掉，再原样写回云端——只靠版本号才能让它拒绝。真实的 0.2.14 代码上跑过同样的场景
//! （见 V2-N0 报告），这里的冻结副本是为了让这个结论一直有回归测试守着。

use serde_json::{json, Value};

use super::*;

/// Z-SSH 0.2.14 同步包解析逻辑的冻结副本（只留字段会影响结论的主机记录；其余实体用 `Value`
/// 透传，它们的处理和这里要证明的事无关）。
mod legacy_0214 {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    pub const SNAPSHOT_VERSION: u32 = 1;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct HostRecord {
        pub uid: String,
        #[serde(default)]
        pub group_uid: Option<String>,
        pub name: String,
        pub host: String,
        pub port: u16,
        pub username: String,
        pub auth_method: String,
        #[serde(default)]
        pub identity_uid: Option<String>,
        #[serde(default)]
        pub proxy_uid: Option<String>,
        #[serde(default)]
        pub jump_host_uid: Option<String>,
        #[serde(default)]
        pub proxy_mode: String,
        #[serde(default)]
        pub tags: Vec<String>,
        #[serde(default)]
        pub sort: i64,
        #[serde(default)]
        pub legacy_algorithms: bool,
        #[serde(default)]
        pub created_at: i64,
        #[serde(default)]
        pub updated_at: i64,
        #[serde(default)]
        pub device_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SyncSnapshot {
        pub schema_version: u32,
        pub device_id: String,
        pub created_at: i64,
        #[serde(default)]
        pub groups: Vec<Value>,
        #[serde(default)]
        pub hosts: Vec<HostRecord>,
        #[serde(default)]
        pub identities: Vec<Value>,
        #[serde(default)]
        pub proxies: Vec<Value>,
        #[serde(default)]
        pub settings: BTreeMap<String, Value>,
        #[serde(default)]
        pub known_hosts: Vec<Value>,
        #[serde(default)]
        pub tombstones: Vec<Value>,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum ParseError {
        TooNew { found: u32, supported: u32 },
        TooOld { found: u32 },
        Format,
    }

    /// 0.2.14 的 `SyncSnapshot::parse`。
    pub fn parse(bytes: &[u8]) -> Result<SyncSnapshot, ParseError> {
        let snapshot: SyncSnapshot =
            serde_json::from_slice(bytes).map_err(|_| ParseError::Format)?;
        if snapshot.schema_version > SNAPSHOT_VERSION {
            return Err(ParseError::TooNew {
                found: snapshot.schema_version,
                supported: SNAPSHOT_VERSION,
            });
        }
        if snapshot.schema_version < SNAPSHOT_VERSION {
            return Err(ParseError::TooOld {
                found: snapshot.schema_version,
            });
        }
        Ok(snapshot)
    }
}

fn host(uid: &str, name: &str) -> HostRecord {
    HostRecord {
        uid: uid.to_owned(),
        group_uid: None,
        name: name.to_owned(),
        host: format!("{name}.example.internal"),
        port: 22,
        username: "root".to_owned(),
        auth_method: "password".to_owned(),
        identity_uid: None,
        proxy_uid: None,
        jump_host_uid: None,
        proxy_mode: "default".to_owned(),
        tags: vec!["t".to_owned()],
        sort: 0,
        legacy_algorithms: false,
        created_at: 1,
        updated_at: 10,
        device_id: "dev-a".to_owned(),
        protocol: "ssh".to_owned(),
        x11_forwarding: false,
        protocol_options: None,
    }
}

fn telnet_host() -> HostRecord {
    HostRecord {
        protocol: "telnet".to_owned(),
        port: 23,
        protocol_options: Some(json!({ "telnet": { "newline": "lf", "future_key": 1 } })),
        ..host("h-telnet", "switch-1")
    }
}

fn serial_host() -> HostRecord {
    HostRecord {
        protocol: "serial".to_owned(),
        host: String::new(),
        port: 0,
        username: String::new(),
        protocol_options: Some(json!({ "serial": { "device": "COM3", "baud_rate": 9600 } })),
        ..host("h-serial", "console-1")
    }
}

fn x11_host() -> HostRecord {
    HostRecord {
        x11_forwarding: true,
        ..host("h-x11", "gui-1")
    }
}

fn snapshot_with(hosts: Vec<HostRecord>) -> SyncSnapshot {
    let mut snapshot = SyncSnapshot::empty("dev-a", 1_760_000_000);
    snapshot.hosts = hosts;
    snapshot.sort_canonical();
    snapshot
}

fn json_of(snapshot: &SyncSnapshot) -> Value {
    serde_json::from_slice(&snapshot.to_json().expect("序列化")).expect("JSON")
}

const NEW_KEYS: [&str; 3] = ["protocol", "x11_forwarding", "protocol_options"];

#[test]
fn legacy_client_refuses_every_package_that_carries_new_fields() {
    for (label, hosts) in [
        ("telnet", vec![host("h1", "web-1"), telnet_host()]),
        ("serial", vec![serial_host()]),
        ("x11 only", vec![x11_host()]),
    ] {
        let snapshot = snapshot_with(hosts);
        let bytes = snapshot.to_json().unwrap();
        assert_eq!(
            json_of(&snapshot)["schema_version"],
            crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH,
            "{label}"
        );
        assert_eq!(
            legacy_0214::parse(&bytes),
            Err(legacy_0214::ParseError::TooNew {
                found: crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH,
                supported: 1
            }),
            "{label}：0.2.14 必须拒绝，而不是丢字段"
        );
        // 新版自己当然读得回来，一个字段不少。
        assert_eq!(
            SyncSnapshot::parse(&bytes).expect("新版能读"),
            snapshot,
            "{label}"
        );
    }
}

#[test]
fn plain_ssh_packages_use_the_local_path_boundary_and_old_clients_refuse() {
    let mut snapshot = snapshot_with(vec![host("h1", "web-1"), host("h2", "web-2")]);
    snapshot.groups.push(GroupRecord {
        uid: "g1".to_owned(),
        name: "生产".to_owned(),
        parent_uid: None,
        sort: 0,
        updated_at: 10,
        device_id: "dev-a".to_owned(),
    });
    snapshot.sort_canonical();
    let value = json_of(&snapshot);
    assert_eq!(
        value["schema_version"],
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH,
        "新版统一写 local-path 边界，不能让旧客户端误读缺路径的包"
    );
    for host in value["hosts"].as_array().unwrap() {
        for key in NEW_KEYS {
            assert!(host.get(key).is_none(), "普通 SSH 主机不该出现 {key}");
        }
    }

    // 旧客户端必须拒绝，而不是把缺失的 v4 字段当成不存在再写回。
    assert!(matches!(
        legacy_0214::parse(&snapshot.to_json().unwrap()),
        Err(legacy_0214::ParseError::TooNew { found, supported })
            if found == crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH && supported == 1
    ));
}

#[test]
fn without_the_version_bump_the_legacy_client_would_silently_drop_the_fields() {
    // 这条测试是"为什么必须升版本"的证据：把包的版本号改回 1，0.2.14 就会放行并丢字段。
    let snapshot = snapshot_with(vec![telnet_host(), serial_host(), x11_host()]);
    let mut value = json_of(&snapshot);
    value["schema_version"] = json!(1);
    let legacy =
        legacy_0214::parse(&serde_json::to_vec(&value).unwrap()).expect("版本号是 1 就被放行");
    assert_eq!(legacy.hosts.len(), 3, "主机本身还在");
    let rewritten = serde_json::to_value(&legacy).unwrap();
    for host in rewritten["hosts"].as_array().unwrap() {
        for key in NEW_KEYS {
            assert!(host.get(key).is_none(), "{key} 被静默丢掉了");
        }
    }
}

#[test]
fn new_client_reads_packages_written_by_the_legacy_client() {
    let legacy = legacy_0214::SyncSnapshot {
        schema_version: 1,
        device_id: "old".to_owned(),
        created_at: 5,
        groups: Vec::new(),
        hosts: vec![legacy_0214::HostRecord {
            uid: "h1".to_owned(),
            group_uid: None,
            name: "web-1".to_owned(),
            host: "web-1.example.internal".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: "password".to_owned(),
            identity_uid: None,
            proxy_uid: None,
            jump_host_uid: None,
            proxy_mode: "default".to_owned(),
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: true,
            created_at: 1,
            updated_at: 2,
            device_id: "old".to_owned(),
        }],
        identities: Vec::new(),
        proxies: Vec::new(),
        settings: Default::default(),
        known_hosts: Vec::new(),
        tombstones: Vec::new(),
    };
    let bytes = serde_json::to_vec(&legacy).unwrap();
    let parsed = SyncSnapshot::parse(&bytes).expect("新版读老格式");
    let host = &parsed.hosts[0];
    assert_eq!(host.protocol, "ssh");
    assert!(!host.x11_forwarding);
    assert_eq!(host.protocol_options, None);
    assert!(host.legacy_algorithms, "老字段原样");
    assert!(!host.needs_protocol_format());
    // 读入旧包后，新版再写会统一升到 local-path 边界。
    assert_eq!(
        parsed.required_version(),
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH
    );
}

#[test]
fn new_to_new_roundtrip_keeps_every_field_including_unknown_option_keys() {
    let snapshot = snapshot_with(vec![
        host("h1", "web-1"),
        telnet_host(),
        serial_host(),
        x11_host(),
    ]);
    let bytes = snapshot.to_json().unwrap();
    let parsed = SyncSnapshot::parse(&bytes).unwrap();
    assert_eq!(parsed, snapshot);
    assert_eq!(
        parsed.schema_version,
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH
    );
    let telnet = parsed.hosts.iter().find(|h| h.uid == "h-telnet").unwrap();
    assert_eq!(
        telnet.protocol_options.as_ref().unwrap()["telnet"]["future_key"],
        1
    );
    // 再写一遍字节一致。
    assert_eq!(parsed.to_json().unwrap(), bytes);
}

#[test]
fn the_version_never_drops_below_the_local_path_boundary() {
    // 私钥路径不在线上传输，解析后字段就是 None；版本不能因此降回旧值，
    // 否则旧客户端会重新接受缺路径的包。这里模拟“内容变少”也必须保持 v4。
    let mut snapshot = snapshot_with(vec![host("h1", "web-1"), telnet_host()]);
    assert_eq!(
        snapshot.required_version(),
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH
    );
    snapshot.hosts.retain(|h| h.protocol == "ssh");
    snapshot.sort_canonical();
    assert_eq!(
        snapshot.schema_version,
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH
    );
    assert!(matches!(
        legacy_0214::parse(&snapshot.to_json().unwrap()),
        Err(legacy_0214::ParseError::TooNew { .. })
    ));

    let mut snapshot = snapshot_with(vec![x11_host()]);
    snapshot.hosts[0].x11_forwarding = false;
    assert_eq!(
        snapshot.required_version(),
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH
    );
}

#[test]
fn protocol_fields_take_part_in_the_merge_comparison() {
    let plain = host("h1", "web-1");
    let mut changed = plain.clone();
    changed.x11_forwarding = true;
    assert!(!plain.same_content(&changed));
    let mut changed = plain.clone();
    changed.protocol = "telnet".to_owned();
    assert!(!plain.same_content(&changed));
    let mut changed = plain.clone();
    changed.protocol_options = Some(json!({}));
    assert!(!plain.same_content(&changed));
    assert!(plain.same_content(&plain.clone()));
}

// ---------- 内置助手接口（V2-A3）----------

fn endpoint_record() -> AiEndpointRecord {
    AiEndpointRecord {
        uid: "e1".to_owned(),
        name: "主力".to_owned(),
        kind: "openai_chat".to_owned(),
        base_url: "https://api.example.com/v1".to_owned(),
        key_identity_uid: None,
        default_model: None,
        models: Vec::new(),
        overrides: json!({}),
        openai_options: json!({}),
        preset_id: None,
        created_at: 1,
        updated_at: 10,
        device_id: "dev-a".to_owned(),
    }
}

#[test]
fn legacy_client_refuses_a_package_with_builtin_assistant_content() {
    // 接口、默认接口设置、builtin 默认智能体：三种内容都让包升到 3，0.2.14 一律拒绝。
    let mut with_endpoint = snapshot_with(vec![host("h1", "web-1")]);
    with_endpoint.ai_endpoints.push(endpoint_record());
    let mut with_default = snapshot_with(vec![host("h1", "web-1")]);
    with_default.settings.insert(
        "ai_builtin_default".to_owned(),
        json!({"endpoint_uid": "e1", "model": null}),
    );
    let mut with_agent = snapshot_with(vec![host("h1", "web-1")]);
    with_agent
        .settings
        .insert("ai_default_agent".to_owned(), json!("builtin"));
    for (label, mut snapshot) in [
        ("接口", with_endpoint),
        ("默认接口", with_default),
        ("默认智能体", with_agent),
    ] {
        snapshot.sort_canonical();
        let bytes = snapshot.to_json().unwrap();
        assert_eq!(
            json_of(&snapshot)["schema_version"],
            crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH,
            "{label}"
        );
        assert_eq!(
            legacy_0214::parse(&bytes),
            Err(legacy_0214::ParseError::TooNew {
                found: crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH,
                supported: 1
            }),
            "{label}：0.2.14 必须拒绝，而不是丢数据"
        );
        // 只认到 v2 的 ZeeTerm（有协议字段、没有内置助手）同样拒绝。
        assert!(
            snapshot.required_version() > SNAPSHOT_VERSION_PROTOCOLS,
            "{label}：v2 客户端读到更高的版本号会拒绝"
        );
        assert_eq!(
            SyncSnapshot::parse(&bytes).expect("新版能读"),
            snapshot,
            "{label}"
        );
    }
}

#[test]
fn without_the_version_bump_the_legacy_client_would_drop_the_endpoints() {
    // "为什么必须升版本"的证据：版本号改回 1，0.2.14 放行，再写回去接口就没了。
    let mut snapshot = snapshot_with(vec![host("h1", "web-1")]);
    snapshot.ai_endpoints.push(endpoint_record());
    let mut value = json_of(&snapshot);
    value["schema_version"] = json!(1);
    let legacy =
        legacy_0214::parse(&serde_json::to_vec(&value).unwrap()).expect("版本号是 1 就被放行");
    let rewritten = serde_json::to_value(&legacy).unwrap();
    assert!(rewritten.get("ai_endpoints").is_none(), "接口被静默丢掉了");
}

#[test]
fn plain_packages_stay_byte_identical_to_the_old_format() {
    let snapshot = snapshot_with(vec![host("h1", "web-1")]);
    let value = json_of(&snapshot);
    assert_eq!(
        value["schema_version"],
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH
    );
    assert!(value.get("ai_endpoints").is_none());
    assert!(matches!(
        legacy_0214::parse(&snapshot.to_json().unwrap()),
        Err(legacy_0214::ParseError::TooNew { .. })
    ));
}

#[test]
fn builtin_content_roundtrips_and_keeps_unknown_option_keys() {
    let mut snapshot = snapshot_with(vec![host("h1", "web-1"), telnet_host()]);
    let mut endpoint = endpoint_record();
    endpoint.openai_options = json!({"max_completion_tokens": "on", "future": [1, 2]});
    snapshot.ai_endpoints.push(endpoint);
    snapshot.sort_canonical();
    assert_eq!(
        snapshot.schema_version,
        crate::sync::package::SNAPSHOT_VERSION_LOCAL_KEY_PATH,
        "新版统一写 local-path 边界"
    );
    let bytes = snapshot.to_json().unwrap();
    let parsed = SyncSnapshot::parse(&bytes).unwrap();
    assert_eq!(parsed, snapshot);
    assert_eq!(
        parsed.ai_endpoints[0].openai_options["future"],
        json!([1, 2])
    );
    assert_eq!(parsed.to_json().unwrap(), bytes);
}
