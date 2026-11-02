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
    assert!(settings.telnet.is_none());
    assert!(settings.is_plain_ssh());
    assert_eq!(Protocol::default(), Protocol::Ssh);
}

#[test]
fn default_ports_follow_the_protocol() {
    assert_eq!(Protocol::Ssh.default_port(), Some(22));
    assert_eq!(Protocol::Telnet.default_port(), Some(23));
    assert_eq!(Protocol::Serial.default_port(), None, "串口没有端口");
}

#[test]
fn protocol_serializes_as_lowercase_and_parses_back() {
    for (protocol, text) in [
        (Protocol::Ssh, "ssh"),
        (Protocol::Telnet, "telnet"),
        (Protocol::Serial, "serial"),
    ] {
        assert_eq!(protocol.as_str(), text);
        assert_eq!(
            serde_json::to_string(&protocol).unwrap(),
            format!("\"{text}\"")
        );
        assert_eq!(Protocol::parse(text).unwrap(), protocol);
    }
    assert!(Protocol::parse("vnc").is_err());
    assert!(Protocol::parse("rdp").is_err());
}

#[test]
fn telnet_defaults_match_the_design() {
    let options = TelnetOptions::default();
    assert_eq!(options.newline, TelnetNewline::CrLf);
    assert_eq!(options.local_echo, LocalEcho::Auto);
    assert!(options.naws, "NAWS 默认开");
    assert_eq!(options.terminal_type, "xterm-256color");
    assert_eq!(options.encoding, TextEncoding::Utf8);
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
fn options_roundtrip_through_the_column_json() {
    let mut telnet = ProtocolSettings::new(Protocol::Telnet);
    {
        let options = telnet.telnet.as_mut().unwrap();
        options.newline = TelnetNewline::Lf;
        options.local_echo = LocalEcho::Off;
        options.encoding = TextEncoding::Gbk;
        options.auto_login.enabled = true;
    }
    let json = telnet.options_json().unwrap().expect("telnet 有选项");
    assert!(json.contains("\"telnet\""), "{json}");
    let back = ProtocolSettings::from_columns("telnet", false, Some(&json)).unwrap();
    assert_eq!(back, telnet);

    assert_eq!(ProtocolSettings::default().options_json().unwrap(), None);
}

#[test]
fn from_columns_fills_defaults_and_survives_garbage_options() {
    let telnet = ProtocolSettings::from_columns("telnet", false, None).unwrap();
    assert_eq!(telnet.telnet, Some(TelnetOptions::default()));

    // 选项坏了：不报错（否则整个主机列表打不开），按默认处理。
    let broken = ProtocolSettings::from_columns("telnet", false, Some("{not json")).unwrap();
    assert_eq!(broken.telnet, Some(TelnetOptions::default()));

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

    let telnet = ProtocolSettings {
        x11_forwarding: true,
        ..ProtocolSettings::new(Protocol::Telnet)
    };
    let text = err_text(telnet.validated(&facts()));
    assert!(text.contains("X11"), "{text}");
}

#[test]
fn ssh_hosts_cannot_carry_telnet_options() {
    let ssh = ProtocolSettings {
        telnet: Some(TelnetOptions::default()),
        ..ProtocolSettings::default()
    };
    assert!(err_text(ssh.validated(&facts())).contains("Telnet 选项"));
}

#[test]
fn validation_fills_in_default_options_for_telnet() {
    let telnet = ProtocolSettings {
        protocol: Protocol::Telnet,
        ..ProtocolSettings::default()
    }
    .validated(&facts())
    .unwrap();
    assert_eq!(telnet.telnet, Some(TelnetOptions::default()));
}

#[test]
fn telnet_only_accepts_password_auth_and_password_identities() {
    let settings = ProtocolSettings::new(Protocol::Telnet);

    for method in [AuthMethod::Key, AuthMethod::Agent] {
        let bad = HostFacts {
            auth_method: method,
            ..facts()
        };
        assert!(
            err_text(settings.clone().validated(&bad)).contains("密码认证"),
            "{method:?}"
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
    // 不选凭据也行（连接时再问）。
    settings.validated(&facts()).expect("不选凭据可以");
}

#[test]
fn telnet_may_use_a_proxy() {
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
    for protocol in [Protocol::Ssh, Protocol::Telnet] {
        let settings = ProtocolSettings::new(protocol);
        let via_ssh = HostFacts {
            jump_host_protocol: Some(Protocol::Ssh),
            ..facts()
        };
        settings
            .clone()
            .validated(&via_ssh)
            .expect("经 SSH 跳板机可以");
        let bad = HostFacts {
            jump_host_protocol: Some(Protocol::Telnet),
            ..facts()
        };
        assert!(
            err_text(settings.validated(&bad)).contains("跳板机必须是 SSH"),
            "{protocol:?}"
        );
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
fn ssh_only_capabilities() {
    let ssh = ProtocolSettings::default();
    assert!(ssh.supports_sftp());
    assert!(ssh.supports_ai_exec());
    assert!(ssh.supports_server_status());
    assert!(ssh.supports_port_forward());
    assert!(ssh.supports_x11());
    assert!(!ssh.x11_enabled(), "能开不等于开了");
    assert_eq!(ssh.ai_exec_refusal(), None);

    let telnet = ProtocolSettings::new(Protocol::Telnet);
    assert!(!telnet.supports_sftp());
    assert!(!telnet.supports_ai_exec());
    assert!(!telnet.supports_server_status());
    assert!(!telnet.supports_port_forward());
    assert!(!telnet.supports_x11());
    assert!(!telnet.x11_enabled());
    let reason = telnet.ai_exec_refusal().expect("非 SSH 要给原因");
    assert!(reason.contains("Telnet"), "{reason}");
}

fn serial_with_device(device: &str) -> ProtocolSettings {
    let mut settings = ProtocolSettings::new(Protocol::Serial);
    settings.serial.as_mut().unwrap().device = device.to_owned();
    settings
}

#[test]
fn serial_defaults_match_the_design() {
    let options = SerialOptions::default();
    assert_eq!(options.device, "");
    assert_eq!(options.baud_rate, 115_200);
    assert_eq!(options.data_bits, SerialDataBits::Eight);
    assert_eq!(options.parity, SerialParity::None);
    assert_eq!(options.stop_bits, SerialStopBits::One);
    assert_eq!(options.flow_control, SerialFlowControl::None);
    assert_eq!(options.newline, SerialNewline::Cr);
    assert!(!options.local_echo, "本地回显默认关");
    assert_eq!(options.encoding, TextEncoding::Utf8);
}

#[test]
fn serial_options_roundtrip_with_readable_json() {
    let mut settings = serial_with_device("COM12");
    {
        let options = settings.serial.as_mut().unwrap();
        options.baud_rate = 250_000;
        options.data_bits = SerialDataBits::Seven;
        options.parity = SerialParity::Mark;
        options.stop_bits = SerialStopBits::Two;
        options.flow_control = SerialFlowControl::Hardware;
        options.newline = SerialNewline::CrLf;
        options.local_echo = true;
        options.encoding = TextEncoding::Gbk;
    }
    let json = settings.options_json().unwrap().expect("串口有选项");
    for expected in [
        "\"serial\"",
        "\"COM12\"",
        "\"data_bits\":\"7\"",
        "\"stop_bits\":\"2\"",
        "\"parity\":\"mark\"",
        "\"flow_control\":\"hardware\"",
    ] {
        assert!(json.contains(expected), "{expected} 不在 {json}");
    }
    let back = ProtocolSettings::from_columns("serial", false, Some(&json)).unwrap();
    assert_eq!(back, settings);

    let one_and_half = r#"{"serial":{"device":"COM1","data_bits":"5","stop_bits":"1.5"}}"#;
    let parsed = ProtocolSettings::from_columns("serial", false, Some(one_and_half)).unwrap();
    let options = parsed.serial.unwrap();
    assert_eq!(options.data_bits, SerialDataBits::Five);
    assert_eq!(options.stop_bits, SerialStopBits::OnePointFive);
}

#[test]
fn serial_validation_checks_device_baud_rate_and_bit_combinations() {
    serial_with_device("COM3")
        .validated(&facts())
        .expect("COM3 可以");
    // 设备名原样存字符串：各种写法都收。
    for device in [r"\.\COM12", "/dev/ttyUSB0", "  COM3  "] {
        let ok = serial_with_device(device).validated(&facts()).unwrap();
        assert_eq!(ok.serial.unwrap().device, device.trim());
    }
    for bad in ["", "   ", "COM\n3"] {
        assert!(
            err_text(serial_with_device(bad).validated(&facts())).contains("设备名"),
            "{bad:?}"
        );
    }
    assert!(err_text(serial_with_device(&"c".repeat(257)).validated(&facts())).contains("设备名"));
    // 没有选项的串口主机没有设备名，不能保存。
    let bare = ProtocolSettings {
        protocol: Protocol::Serial,
        ..ProtocolSettings::default()
    };
    assert!(err_text(bare.validated(&facts())).contains("设备名"));

    // 波特率允许自定义，但有范围。
    for (baud, ok) in [
        (49, false),
        (50, true),
        (74_880, true),
        (4_000_000, true),
        (4_000_001, false),
    ] {
        let mut settings = serial_with_device("COM3");
        settings.serial.as_mut().unwrap().baud_rate = baud;
        assert_eq!(settings.validated(&facts()).is_ok(), ok, "{baud}");
    }

    // 1.5 个停止位只配 5 位数据位；5 位数据位不能配 2 个停止位。
    let combo = |bits, stop| {
        let mut settings = serial_with_device("COM3");
        let options = settings.serial.as_mut().unwrap();
        options.data_bits = bits;
        options.stop_bits = stop;
        settings.validated(&facts()).is_ok()
    };
    assert!(combo(SerialDataBits::Five, SerialStopBits::OnePointFive));
    assert!(combo(SerialDataBits::Five, SerialStopBits::One));
    assert!(!combo(SerialDataBits::Five, SerialStopBits::Two));
    assert!(!combo(SerialDataBits::Eight, SerialStopBits::OnePointFive));
    assert!(combo(SerialDataBits::Eight, SerialStopBits::Two));
}

#[test]
fn serial_rejects_network_references_and_foreign_options() {
    let pw = password_identity();
    let with_identity = HostFacts {
        identity: Some(&pw),
        ..facts()
    };
    assert!(err_text(serial_with_device("COM3").validated(&with_identity)).contains("凭据"));
    let with_proxy = HostFacts {
        proxy_mode: ProxyMode::Custom,
        proxy_id: Some(1),
        ..facts()
    };
    assert!(err_text(serial_with_device("COM3").validated(&with_proxy)).contains("代理"));
    let with_jump = HostFacts {
        jump_host_protocol: Some(Protocol::Ssh),
        ..facts()
    };
    assert!(err_text(serial_with_device("COM3").validated(&with_jump)).contains("跳板机"));

    let mut x11 = serial_with_device("COM3");
    x11.x11_forwarding = true;
    assert!(err_text(x11.validated(&facts())).contains("X11"));

    let mut telnet_options = serial_with_device("COM3");
    telnet_options.telnet = Some(TelnetOptions::default());
    assert!(err_text(telnet_options.validated(&facts())).contains("Telnet 选项"));
    let ssh_with_serial = ProtocolSettings {
        serial: Some(SerialOptions::default()),
        ..ProtocolSettings::default()
    };
    assert!(err_text(ssh_with_serial.validated(&facts())).contains("串口选项"));

    // 串口不能当跳板机。
    let via_serial = HostFacts {
        jump_host_protocol: Some(Protocol::Serial),
        ..facts()
    };
    assert!(
        err_text(ProtocolSettings::default().validated(&via_serial)).contains("跳板机必须是 SSH")
    );
}

#[test]
fn serial_has_no_ssh_only_capabilities() {
    let serial = serial_with_device("COM3");
    assert!(!serial.is_plain_ssh());
    assert!(!serial.supports_sftp());
    assert!(!serial.supports_ai_exec());
    assert!(!serial.supports_server_status());
    assert!(!serial.supports_port_forward());
    assert!(!serial.supports_x11());
    assert!(!serial.x11_enabled());
    let reason = serial.ai_exec_refusal().expect("串口要给原因");
    assert!(reason.contains("串口"), "{reason}");
}
