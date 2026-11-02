use super::*;

fn password_identity() -> Identity {
    Identity {
        id: 1,
        name: "pw".to_owned(),
        kind: IdentityKind::Password,
        key_path: None,
        has_secret: true,
    }
}

fn key_identity() -> Identity {
    Identity {
        id: 2,
        name: "key".to_owned(),
        kind: IdentityKind::KeyFile,
        key_path: Some("k".to_owned()),
        has_secret: false,
    }
}

fn facts<'a>() -> HostFacts<'a> {
    HostFacts {
        username: "admin",
        auth_method: AuthMethod::Password,
        identity: None,
        proxy_mode: ProxyMode::None,
        proxy_id: None,
        jump_host_protocol: None,
    }
}

fn err_text(result: Result<ProtocolSettings, ZsshError>) -> String {
    result.expect_err("应该被拒绝").to_string()
}

#[test]
fn defaults_are_plain_ssh() {
    let settings = ProtocolSettings::default();
    assert_eq!(settings.protocol, Protocol::Ssh);
    assert!(!settings.x11_forwarding);
    assert!(settings.telnet.is_none() && settings.rdp.is_none());
    assert!(settings.is_plain_ssh());
    assert_eq!(Protocol::default(), Protocol::Ssh);
}

#[test]
fn default_ports_follow_the_protocol() {
    assert_eq!(Protocol::Ssh.default_port(), 22);
    assert_eq!(Protocol::Telnet.default_port(), 23);
    assert_eq!(Protocol::Rdp.default_port(), 3389);
}

#[test]
fn protocol_serializes_as_lowercase_and_parses_back() {
    for (protocol, text) in [
        (Protocol::Ssh, "ssh"),
        (Protocol::Telnet, "telnet"),
        (Protocol::Rdp, "rdp"),
    ] {
        assert_eq!(protocol.as_str(), text);
        assert_eq!(
            serde_json::to_string(&protocol).unwrap(),
            format!("\"{text}\"")
        );
        assert_eq!(Protocol::parse(text).unwrap(), protocol);
    }
    assert!(Protocol::parse("vnc").is_err());
}

#[test]
fn telnet_defaults_match_the_design() {
    let options = TelnetOptions::default();
    assert_eq!(options.newline, TelnetNewline::CrLf);
    assert_eq!(options.local_echo, LocalEcho::Auto);
    assert!(options.naws, "NAWS 默认开");
    assert_eq!(options.terminal_type, "xterm-256color");
    assert_eq!(options.encoding, TelnetEncoding::Utf8);
    assert!(!options.auto_login.enabled);
    assert!(options
        .auto_login
        .username_patterns
        .iter()
        .any(|p| p == "login:"));
    assert!(options
        .auto_login
        .password_patterns
        .iter()
        .any(|p| p == "password:"));
    assert!(!options.raw_tcp);
}

#[test]
fn rdp_defaults_match_the_design() {
    let options = RdpOptions::default();
    assert_eq!(options.domain, None);
    assert_eq!(options.resolution, RdpResolution::FitWindow);
    assert!(!options.multi_monitor);
    assert!(options.clipboard);
    assert!(!options.drives, "驱动器重定向默认关");
    assert_eq!(options.audio, RdpAudio::Local);
    assert!(!options.admin_session);
    assert!(options.nla, "NLA 默认开");
    assert_eq!(options.certificate_policy, RdpCertificatePolicy::Mstsc);
}

#[test]
fn options_roundtrip_through_the_column_json() {
    let mut telnet = ProtocolSettings::new(Protocol::Telnet);
    {
        let options = telnet.telnet.as_mut().unwrap();
        options.newline = TelnetNewline::Lf;
        options.local_echo = LocalEcho::Off;
        options.encoding = TelnetEncoding::Gbk;
        options.auto_login.enabled = true;
    }
    let json = telnet.options_json().unwrap().expect("telnet 有选项");
    assert!(
        json.contains("\"telnet\"") && !json.contains("\"rdp\""),
        "{json}"
    );
    let back = ProtocolSettings::from_columns("telnet", false, Some(&json)).unwrap();
    assert_eq!(back, telnet);

    let mut rdp = ProtocolSettings::new(Protocol::Rdp);
    {
        let options = rdp.rdp.as_mut().unwrap();
        options.domain = Some("corp".to_owned());
        options.resolution = RdpResolution::Fixed {
            width: 1920,
            height: 1080,
        };
        options.admin_session = true;
        options.audio = RdpAudio::None;
    }
    let json = rdp.options_json().unwrap().expect("rdp 有选项");
    assert!(json.contains("\"fixed\""), "分辨率模式用小写蛇形：{json}");
    let back = ProtocolSettings::from_columns("rdp", false, Some(&json)).unwrap();
    assert_eq!(back, rdp);

    assert_eq!(ProtocolSettings::default().options_json().unwrap(), None);
}

#[test]
fn from_columns_fills_defaults_and_survives_garbage_options() {
    let telnet = ProtocolSettings::from_columns("telnet", false, None).unwrap();
    assert_eq!(telnet.telnet, Some(TelnetOptions::default()));
    assert!(telnet.rdp.is_none());

    // 选项坏了：不报错（否则整个主机列表打不开），按默认处理。
    let rdp = ProtocolSettings::from_columns("rdp", false, Some("{not json")).unwrap();
    assert_eq!(rdp.rdp, Some(RdpOptions::default()));

    // 协议名认不出来才是真错误。
    assert!(ProtocolSettings::from_columns("vnc", false, None).is_err());

    // 只写了一部分字段的选项（比如以后版本新增了字段）也能读，缺的用默认。
    let partial =
        ProtocolSettings::from_columns("telnet", false, Some(r#"{"telnet":{"newline":"lf"}}"#))
            .unwrap();
    let options = partial.telnet.unwrap();
    assert_eq!(options.newline, TelnetNewline::Lf);
    assert!(options.naws);
}

#[test]
fn x11_is_only_allowed_on_ssh() {
    let ssh = ProtocolSettings {
        x11_forwarding: true,
        ..ProtocolSettings::default()
    };
    let ssh = ssh.validated(&facts()).expect("SSH 可以开 X11");
    assert!(ssh.x11_enabled());

    for protocol in [Protocol::Telnet, Protocol::Rdp] {
        let mut settings = ProtocolSettings::new(protocol);
        settings.x11_forwarding = true;
        let text = err_text(settings.validated(&facts()));
        assert!(text.contains("X11"), "{text}");
    }
}

#[test]
fn options_must_match_the_protocol() {
    let ssh = ProtocolSettings {
        telnet: Some(TelnetOptions::default()),
        ..ProtocolSettings::default()
    };
    assert!(err_text(ssh.validated(&facts())).contains("Telnet 选项"));

    let ssh = ProtocolSettings {
        rdp: Some(RdpOptions::default()),
        ..ProtocolSettings::default()
    };
    assert!(err_text(ssh.validated(&facts())).contains("RDP 选项"));

    let mut telnet = ProtocolSettings::new(Protocol::Telnet);
    telnet.rdp = Some(RdpOptions::default());
    assert!(err_text(telnet.validated(&facts())).contains("RDP 选项"));

    let mut rdp = ProtocolSettings::new(Protocol::Rdp);
    rdp.telnet = Some(TelnetOptions::default());
    assert!(err_text(rdp.validated(&facts())).contains("Telnet 选项"));
}

#[test]
fn validation_fills_in_default_options_for_the_protocol() {
    let telnet = ProtocolSettings {
        protocol: Protocol::Telnet,
        ..ProtocolSettings::default()
    }
    .validated(&facts())
    .unwrap();
    assert_eq!(telnet.telnet, Some(TelnetOptions::default()));

    let rdp = ProtocolSettings {
        protocol: Protocol::Rdp,
        ..ProtocolSettings::default()
    }
    .validated(&facts())
    .unwrap();
    assert_eq!(rdp.rdp, Some(RdpOptions::default()));
}

#[test]
fn telnet_and_rdp_only_accept_password_auth_and_password_identities() {
    for protocol in [Protocol::Telnet, Protocol::Rdp] {
        let settings = ProtocolSettings::new(protocol);

        for method in [AuthMethod::Key, AuthMethod::Agent] {
            let bad = HostFacts {
                auth_method: method,
                ..facts()
            };
            assert!(
                err_text(settings.clone().validated(&bad)).contains("密码认证"),
                "{protocol:?} {method:?}"
            );
        }

        let key = key_identity();
        let bad = HostFacts {
            identity: Some(&key),
            ..facts()
        };
        assert!(err_text(settings.clone().validated(&bad)).contains("密码凭据"));

        let pw = password_identity();
        let good = HostFacts {
            identity: Some(&pw),
            ..facts()
        };
        settings.clone().validated(&good).expect("密码凭据可以");
        // 不选凭据也行（RDP 交给 mstsc 问，Telnet 连接时问）。
        settings.validated(&facts()).expect("不选凭据可以");
    }
}

#[test]
fn rdp_rejects_any_proxy_but_telnet_allows_it() {
    let rdp = ProtocolSettings::new(Protocol::Rdp);
    for (mode, proxy_id) in [
        (ProxyMode::Default, None),
        (ProxyMode::Custom, Some(3)),
        (ProxyMode::None, Some(3)),
    ] {
        let bad = HostFacts {
            proxy_mode: mode,
            proxy_id,
            ..facts()
        };
        let text = err_text(rdp.clone().validated(&bad));
        assert!(text.contains("代理"), "{mode:?}: {text}");
    }

    let telnet = ProtocolSettings::new(Protocol::Telnet);
    for (mode, proxy_id) in [
        (ProxyMode::Default, None),
        (ProxyMode::Custom, Some(3)),
        (ProxyMode::None, None),
    ] {
        let good = HostFacts {
            proxy_mode: mode,
            proxy_id,
            ..facts()
        };
        telnet.clone().validated(&good).expect("Telnet 可以走代理");
    }
}

#[test]
fn jump_host_must_be_ssh() {
    for protocol in [Protocol::Ssh, Protocol::Telnet, Protocol::Rdp] {
        let settings = ProtocolSettings::new(protocol);
        let via_ssh = HostFacts {
            jump_host_protocol: Some(Protocol::Ssh),
            ..facts()
        };
        settings
            .clone()
            .validated(&via_ssh)
            .expect("经 SSH 跳板机可以");
        for bad_jump in [Protocol::Telnet, Protocol::Rdp] {
            let bad = HostFacts {
                jump_host_protocol: Some(bad_jump),
                ..facts()
            };
            assert!(
                err_text(settings.clone().validated(&bad)).contains("跳板机必须是 SSH"),
                "{protocol:?} 经 {bad_jump:?}"
            );
        }
    }
}

#[test]
fn telnet_option_values_are_checked() {
    let mut bad = ProtocolSettings::new(Protocol::Telnet);
    bad.telnet.as_mut().unwrap().terminal_type = "xterm 256".to_owned();
    assert!(err_text(bad.validated(&facts())).contains("终端类型"));

    let mut bad = ProtocolSettings::new(Protocol::Telnet);
    bad.telnet.as_mut().unwrap().terminal_type = "  ".to_owned();
    assert!(err_text(bad.validated(&facts())).contains("终端类型"));

    // 自动登录开着，规则不能是空的，用户名也不能空。
    let mut bad = ProtocolSettings::new(Protocol::Telnet);
    {
        let login = &mut bad.telnet.as_mut().unwrap().auto_login;
        login.enabled = true;
        login.username_patterns = vec!["  ".to_owned()];
    }
    assert!(err_text(bad.validated(&facts())).contains("匹配规则"));

    let mut bad = ProtocolSettings::new(Protocol::Telnet);
    bad.telnet.as_mut().unwrap().auto_login.enabled = true;
    let anonymous = HostFacts {
        username: "  ",
        ..facts()
    };
    assert!(err_text(bad.validated(&anonymous)).contains("用户名不能为空"));

    // 规则条数 / 长度 / 控制字符有上限。
    let mut bad = ProtocolSettings::new(Protocol::Telnet);
    bad.telnet.as_mut().unwrap().auto_login.password_patterns =
        (0..9).map(|n| format!("p{n}:")).collect();
    assert!(err_text(bad.validated(&facts())).contains("最多"));
    let mut bad = ProtocolSettings::new(Protocol::Telnet);
    bad.telnet.as_mut().unwrap().auto_login.password_patterns = vec!["a\nb".to_owned()];
    assert!(err_text(bad.validated(&facts())).contains("控制字符"));

    // 整理：首尾空白去掉、空规则丢掉。
    let mut ok = ProtocolSettings::new(Protocol::Telnet);
    {
        let options = ok.telnet.as_mut().unwrap();
        options.terminal_type = " vt100 ".to_owned();
        options.auto_login.username_patterns = vec![" login: ".to_owned(), String::new()];
    }
    let ok = ok.validated(&facts()).unwrap().telnet.unwrap();
    assert_eq!(ok.terminal_type, "vt100");
    assert_eq!(ok.auto_login.username_patterns, vec!["login:".to_owned()]);
}

#[test]
fn rdp_option_values_are_checked() {
    for (width, height) in [(199, 600), (800, 199), (8193, 600), (800, 8193), (0, 0)] {
        let mut bad = ProtocolSettings::new(Protocol::Rdp);
        bad.rdp.as_mut().unwrap().resolution = RdpResolution::Fixed { width, height };
        assert!(
            err_text(bad.validated(&facts())).contains("固定分辨率"),
            "{width}x{height}"
        );
    }
    let mut ok = ProtocolSettings::new(Protocol::Rdp);
    ok.rdp.as_mut().unwrap().resolution = RdpResolution::Fixed {
        width: 200,
        height: 8192,
    };
    ok.validated(&facts()).expect("边界值可以");

    let mut bad = ProtocolSettings::new(Protocol::Rdp);
    bad.rdp.as_mut().unwrap().domain = Some("corp\nfull address:s:evil".to_owned());
    assert!(err_text(bad.validated(&facts())).contains("域"));

    let mut bad = ProtocolSettings::new(Protocol::Rdp);
    bad.rdp.as_mut().unwrap().domain = Some("d".repeat(256));
    assert!(err_text(bad.validated(&facts())).contains("域"));

    // 空域当没填；有内容的去首尾空白。
    let mut blank = ProtocolSettings::new(Protocol::Rdp);
    blank.rdp.as_mut().unwrap().domain = Some("  ".to_owned());
    assert_eq!(blank.validated(&facts()).unwrap().rdp.unwrap().domain, None);
    let mut named = ProtocolSettings::new(Protocol::Rdp);
    named.rdp.as_mut().unwrap().domain = Some(" corp ".to_owned());
    assert_eq!(
        named.validated(&facts()).unwrap().rdp.unwrap().domain,
        Some("corp".to_owned())
    );
}

#[test]
fn ssh_only_capabilities() {
    let ssh = ProtocolSettings::default();
    assert!(ssh.supports_sftp());
    assert!(ssh.supports_ai_exec());
    assert!(ssh.supports_server_status());
    assert!(ssh.supports_port_forward());
    assert!(ssh.supports_x11());
    assert!(!ssh.x11_enabled(), "能开不等于开了");
    assert_eq!(ssh.ai_exec_refusal(), None);

    for protocol in [Protocol::Telnet, Protocol::Rdp] {
        let settings = ProtocolSettings::new(protocol);
        assert!(!settings.supports_sftp(), "{protocol:?}");
        assert!(!settings.supports_ai_exec(), "{protocol:?}");
        assert!(!settings.supports_server_status(), "{protocol:?}");
        assert!(!settings.supports_port_forward(), "{protocol:?}");
        assert!(!settings.supports_x11(), "{protocol:?}");
        assert!(!settings.x11_enabled(), "{protocol:?}");
        let reason = settings.ai_exec_refusal().expect("非 SSH 要给原因");
        assert!(reason.contains(protocol.label()), "{reason}");
    }
}
