//! 主机表单里和协议有关的纯逻辑：哪些字段在什么协议下显示、切换协议时端口和用户名怎么跟着变、
//! 核心库的保存错误该落在哪个字段旁边、主机列表里的协议标识和副标题、SSH-only 能力的判定。
//!
//! 不依赖 GPUI：单测直接盯这些规则（任务卡 V2-N4 的"状态测试"）。

use zssh_core::model::{
    Host, LocalEcho, Protocol, SerialDataBits, SerialFlowControl, SerialNewline, SerialParity,
    SerialStopBits, TelnetNewline, TextEncoding,
};

use crate::i18n::{Msg, t};

/// 本文件（协议表单、主机列表标识）的界面文案。协议名 / 编码名（SSH、Telnet、SFTP、UTF-8、
/// GBK……）和数值选项（数据位、停止位）两个语言下写法相同，不进这张表。
#[derive(Clone, Copy, Debug)]
enum ProtocolMsg {
    // ---- Telnet 换行 ----
    TelnetNewlineCrLf,
    TelnetNewlineCr,
    TelnetNewlineLf,
    // ---- 本地回显 ----
    LocalEchoAuto,
    LocalEchoOn,
    LocalEchoOff,
    // ---- 串口校验位 ----
    ParityNone,
    ParityOdd,
    ParityEven,
    ParityMark,
    ParitySpace,
    // ---- 串口流控 ----
    FlowNone,
    FlowSoftware,
    FlowHardware,
    // ---- 串口换行 ----
    SerialNewlineCr,
    SerialNewlineLf,
    SerialNewlineCrLf,
    // ---- 串口设备 ----
    BaudNeedsNumber,
    NoSerialPorts,
    ChooseSerialPort,
    DeviceNotDetectedSuffix,
    SerialTag,
    SerialUnselected,
}

impl Msg for ProtocolMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::TelnetNewlineCrLf => "CR LF（回车 + 换行）",
            Self::TelnetNewlineCr => "CR（回车）",
            Self::TelnetNewlineLf => "LF（换行）",
            Self::LocalEchoAuto => "自动（跟随服务器协商）",
            Self::LocalEchoOn => "开",
            Self::LocalEchoOff => "关",
            Self::ParityNone => "无",
            Self::ParityOdd => "奇校验",
            Self::ParityEven => "偶校验",
            Self::ParityMark => "标记（Mark）",
            Self::ParitySpace => "空格（Space）",
            Self::FlowNone => "无",
            Self::FlowSoftware => "软件（XON/XOFF）",
            Self::FlowHardware => "硬件（RTS/CTS）",
            Self::SerialNewlineCr => "CR（回车）",
            Self::SerialNewlineLf => "LF（换行）",
            Self::SerialNewlineCrLf => "CR LF（回车 + 换行）",
            Self::BaudNeedsNumber => "波特率要填数字（例如 115200）",
            Self::NoSerialPorts => "未检测到串口",
            Self::ChooseSerialPort => "（请选择串口）",
            Self::DeviceNotDetectedSuffix => "（当前未检测到）",
            Self::SerialTag => "串口",
            Self::SerialUnselected => "未选择串口",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::TelnetNewlineCrLf => "CR LF (carriage return + line feed)",
            Self::TelnetNewlineCr => "CR (carriage return)",
            Self::TelnetNewlineLf => "LF (line feed)",
            Self::LocalEchoAuto => "Auto (follow server negotiation)",
            Self::LocalEchoOn => "On",
            Self::LocalEchoOff => "Off",
            Self::ParityNone => "None",
            Self::ParityOdd => "Odd",
            Self::ParityEven => "Even",
            Self::ParityMark => "Mark",
            Self::ParitySpace => "Space",
            Self::FlowNone => "None",
            Self::FlowSoftware => "Software (XON/XOFF)",
            Self::FlowHardware => "Hardware (RTS/CTS)",
            Self::SerialNewlineCr => "CR (carriage return)",
            Self::SerialNewlineLf => "LF (line feed)",
            Self::SerialNewlineCrLf => "CR LF (carriage return + line feed)",
            Self::BaudNeedsNumber => "Baud rate must be a number (for example 115200)",
            Self::NoSerialPorts => "No serial ports detected",
            Self::ChooseSerialPort => "(Select a serial port)",
            Self::DeviceNotDetectedSuffix => " (not detected right now)",
            Self::SerialTag => "Serial",
            Self::SerialUnselected => "No serial port selected",
        }
    }
}

/// 表单里能挂错误提示的位置。核心库只给一句中文原因，靠 [`locate_error`] 找到该落在哪。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FormField {
    Name,
    Address,
    Port,
    Username,
    Password,
    Identity,
    Proxy,
    Jump,
    X11,
    TerminalType,
    AutoLogin,
    Device,
    Baud,
    /// 数据位 / 停止位这一组。
    Framing,
    /// 找不到对应字段的错误：显示在表单最下面。
    General,
}

/// 保存前的校验错误，带着该显示在哪个字段旁边。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: FormField,
    pub message: String,
}

impl FieldError {
    pub fn new(field: FormField, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl PartialEq<&str> for FieldError {
    fn eq(&self, other: &&str) -> bool {
        self.message == *other
    }
}

/// 核心库保存时报的错（`ZsshError::InvalidInput` 的中文原因）该显示在哪个字段旁。
/// 按关键词找：先找更具体的（自动登录的提示规则里也有"用户名"三个字）。
pub fn locate_error(message: &str) -> FormField {
    let has = |words: &[&str]| words.iter().any(|word| message.contains(word));
    if has(&["自动登录", "用户名提示", "密码提示"]) {
        FormField::AutoLogin
    } else if has(&["X11"]) {
        FormField::X11
    } else if has(&["串口设备", "设备名"]) {
        FormField::Device
    } else if has(&["波特率"]) {
        FormField::Baud
    } else if has(&["数据位", "停止位"]) {
        FormField::Framing
    } else if has(&["终端类型"]) {
        FormField::TerminalType
    } else if has(&["跳板机"]) {
        FormField::Jump
    } else if has(&["代理"]) {
        FormField::Proxy
    } else if has(&["凭据", "密码认证", "密码凭据"]) {
        FormField::Identity
    } else if has(&["主机名不能为空"]) {
        FormField::Name
    } else if has(&["用户名"]) {
        FormField::Username
    } else if has(&["端口"]) {
        FormField::Port
    } else if has(&["地址"]) {
        FormField::Address
    } else {
        FormField::General
    }
}

// ---- 切换协议 ----

/// 新建时各协议默认的用户名：SSH 沿用 0.2.14 的 root，Telnet 可以不填，串口没有。
pub fn default_username(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Ssh => "root",
        Protocol::Telnet | Protocol::Serial => "",
    }
}

/// 各协议默认端口的文字；串口没有端口。
pub fn default_port_text(protocol: Protocol) -> Option<String> {
    protocol.default_port().map(|port| port.to_string())
}

/// 切换协议时一个"自动填的值"（端口、用户名）要不要跟着变：
/// 输入框里还是上一次自动填的值（或空的）就是用户没改过，换成新协议的默认值；
/// 用户改过的值原样保留。返回 `Some(新值)` 表示要换。
pub fn follow_default(current: &str, auto: &str, new_default: &str) -> Option<String> {
    let current = current.trim();
    ((current.is_empty() || current == auto) && current != new_default)
        .then(|| new_default.to_owned())
}

// ---- 字段显隐 ----

/// 某个协议下表单显示哪些字段。不显示的字段已填的值不清掉，保存时才按协议忽略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visible {
    pub address: bool,
    pub port: bool,
    pub username: bool,
    pub password: bool,
    /// 认证方式下拉（只有 SSH 能选私钥 / agent）。
    pub auth_method: bool,
    /// 选凭据（私钥 / ssh-agent 认证的密钥下拉；密码归主机，不选）。
    pub identity: bool,
    pub proxy: bool,
    pub jump: bool,
    pub x11: bool,
    /// 旧算法兼容、已保存的主机指纹（SSH 才有）。
    pub ssh_advanced: bool,
    pub telnet_advanced: bool,
    pub serial: bool,
    pub serial_advanced: bool,
    pub memory: bool,
}

pub fn visible(protocol: Protocol) -> Visible {
    let network = protocol != Protocol::Serial;
    Visible {
        address: network,
        port: network,
        username: network,
        password: network,
        auth_method: protocol == Protocol::Ssh,
        identity: network,
        proxy: network,
        jump: network,
        x11: protocol.supports_x11(),
        ssh_advanced: protocol == Protocol::Ssh,
        telnet_advanced: protocol == Protocol::Telnet,
        serial: protocol == Protocol::Serial,
        serial_advanced: protocol == Protocol::Serial,
        // AI 助手只能在 SSH 主机上执行命令，记忆没有用处。
        memory: protocol.supports_ai_exec(),
    }
}

/// 已有主机编辑时能切换到哪些协议：串口主机和网络主机不能互转（保存会被核心库拒绝），
/// 新建没有限制。
pub fn can_switch_to(editing: Option<Protocol>, to: Protocol) -> bool {
    match editing {
        None => true,
        Some(current) => (current == Protocol::Serial) == (to == Protocol::Serial),
    }
}

// ---- 下拉里的选项和文案 ----

pub const PROTOCOLS: [Protocol; 3] = [Protocol::Ssh, Protocol::Telnet, Protocol::Serial];

pub const TELNET_NEWLINES: [TelnetNewline; 3] =
    [TelnetNewline::CrLf, TelnetNewline::Cr, TelnetNewline::Lf];

pub fn telnet_newline_label(value: TelnetNewline) -> &'static str {
    match value {
        TelnetNewline::CrLf => t(ProtocolMsg::TelnetNewlineCrLf),
        TelnetNewline::Cr => t(ProtocolMsg::TelnetNewlineCr),
        TelnetNewline::Lf => t(ProtocolMsg::TelnetNewlineLf),
    }
}

pub const LOCAL_ECHOES: [LocalEcho; 3] = [LocalEcho::Auto, LocalEcho::On, LocalEcho::Off];

pub fn local_echo_label(value: LocalEcho) -> &'static str {
    match value {
        LocalEcho::Auto => t(ProtocolMsg::LocalEchoAuto),
        LocalEcho::On => t(ProtocolMsg::LocalEchoOn),
        LocalEcho::Off => t(ProtocolMsg::LocalEchoOff),
    }
}

pub const ENCODINGS: [TextEncoding; 6] = [
    TextEncoding::Utf8,
    TextEncoding::Gbk,
    TextEncoding::Big5,
    TextEncoding::ShiftJis,
    TextEncoding::EucKr,
    TextEncoding::Latin1,
];

pub fn encoding_label(value: TextEncoding) -> &'static str {
    match value {
        TextEncoding::Utf8 => "UTF-8",
        TextEncoding::Gbk => "GBK",
        TextEncoding::Big5 => "Big5",
        TextEncoding::ShiftJis => "Shift-JIS",
        TextEncoding::EucKr => "EUC-KR",
        TextEncoding::Latin1 => "Latin-1",
    }
}

pub const DATA_BITS: [SerialDataBits; 4] = [
    SerialDataBits::Eight,
    SerialDataBits::Seven,
    SerialDataBits::Six,
    SerialDataBits::Five,
];

pub fn data_bits_label(value: SerialDataBits) -> &'static str {
    match value {
        SerialDataBits::Five => "5",
        SerialDataBits::Six => "6",
        SerialDataBits::Seven => "7",
        SerialDataBits::Eight => "8",
    }
}

pub const PARITIES: [SerialParity; 5] = [
    SerialParity::None,
    SerialParity::Odd,
    SerialParity::Even,
    SerialParity::Mark,
    SerialParity::Space,
];

pub fn parity_label(value: SerialParity) -> &'static str {
    match value {
        SerialParity::None => t(ProtocolMsg::ParityNone),
        SerialParity::Odd => t(ProtocolMsg::ParityOdd),
        SerialParity::Even => t(ProtocolMsg::ParityEven),
        SerialParity::Mark => t(ProtocolMsg::ParityMark),
        SerialParity::Space => t(ProtocolMsg::ParitySpace),
    }
}

pub const STOP_BITS: [SerialStopBits; 3] = [
    SerialStopBits::One,
    SerialStopBits::OnePointFive,
    SerialStopBits::Two,
];

pub fn stop_bits_label(value: SerialStopBits) -> &'static str {
    match value {
        SerialStopBits::One => "1",
        SerialStopBits::OnePointFive => "1.5",
        SerialStopBits::Two => "2",
    }
}

pub const FLOW_CONTROLS: [SerialFlowControl; 3] = [
    SerialFlowControl::None,
    SerialFlowControl::Software,
    SerialFlowControl::Hardware,
];

pub fn flow_control_label(value: SerialFlowControl) -> &'static str {
    match value {
        SerialFlowControl::None => t(ProtocolMsg::FlowNone),
        SerialFlowControl::Software => t(ProtocolMsg::FlowSoftware),
        SerialFlowControl::Hardware => t(ProtocolMsg::FlowHardware),
    }
}

pub const SERIAL_NEWLINES: [SerialNewline; 3] =
    [SerialNewline::Cr, SerialNewline::Lf, SerialNewline::CrLf];

pub fn serial_newline_label(value: SerialNewline) -> &'static str {
    match value {
        SerialNewline::Cr => t(ProtocolMsg::SerialNewlineCr),
        SerialNewline::Lf => t(ProtocolMsg::SerialNewlineLf),
        SerialNewline::CrLf => t(ProtocolMsg::SerialNewlineCrLf),
    }
}

// ---- Telnet 自动登录提示词 ----

/// 输入框里的提示词：逗号 / 分号 / 换行分隔，去掉空白和空项。
pub fn parse_patterns(text: &str) -> Vec<String> {
    text.split([',', '，', ';', '；', '\n'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn format_patterns(patterns: &[String]) -> String {
    patterns.join(", ")
}

// ---- 串口 ----

/// 波特率下拉里的常用值。
pub const COMMON_BAUD_RATES: [u32; 11] = [
    1200, 2400, 4800, 9600, 19200, 38400, 57600, 115_200, 230_400, 460_800, 921_600,
];

/// 输入框里的波特率文字。范围由核心库检查（保存时错误落在波特率旁）。
pub fn parse_baud(text: &str) -> Result<u32, String> {
    text.trim()
        .parse::<u32>()
        .map_err(|_| t(ProtocolMsg::BaudNeedsNumber).to_owned())
}

/// 下拉里的一个串口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortChoice {
    pub device: String,
    /// 友好名，如 `USB-SERIAL CH340 (COM3)`；取不到就是设备名。
    pub label: String,
}

/// 本机串口列表。调试构建可以用 `ZSSH_FAKE_SERIAL_PORTS` 伪造（`none` = 没有；
/// 否则 `设备名~友好名`（或 `|`，但脚本的环境变量串里 `|` 是分隔符，所以用 `~`）用 `;` 隔开），截图和自动化不依赖这台电脑上有没有串口。
pub fn list_ports() -> Vec<PortChoice> {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("ZSSH_FAKE_SERIAL_PORTS") {
        return parse_fake_ports(&raw);
    }
    match zssh_protocols::serial::list_ports() {
        Ok(entries) => entries
            .into_iter()
            .map(|entry| PortChoice {
                device: entry.device,
                label: entry.display_name,
            })
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "列串口失败，按没有串口处理");
            Vec::new()
        }
    }
}

#[cfg(debug_assertions)]
fn parse_fake_ports(raw: &str) -> Vec<PortChoice> {
    if raw.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    raw.split(';')
        .filter_map(|item| {
            let (device, label) = item.split_once(['|', '~']).unwrap_or((item, item));
            let device = device.trim();
            (!device.is_empty()).then(|| PortChoice {
                device: device.to_owned(),
                label: label.trim().to_owned(),
            })
        })
        .collect()
}

/// 设备下拉按钮上的字：能在列表里找到就显示友好名；找不到（拔掉了 / 手填的）显示设备名并说明；
/// 还没填显示提示。
pub fn device_button_label(device: &str, ports: &[PortChoice]) -> String {
    let device = device.trim();
    if device.is_empty() {
        return if ports.is_empty() {
            t(ProtocolMsg::NoSerialPorts).to_owned()
        } else {
            t(ProtocolMsg::ChooseSerialPort).to_owned()
        };
    }
    match ports
        .iter()
        .find(|port| port.device.eq_ignore_ascii_case(device))
    {
        Some(port) => port.label.clone(),
        None => format!("{device}{}", t(ProtocolMsg::DeviceNotDetectedSuffix)),
    }
}

// ---- 主机列表里的标识 ----

/// 协议标识的文字（卡片、列表、树上的小标签）。
pub fn protocol_tag(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Ssh => "SSH",
        Protocol::Telnet => "Telnet",
        Protocol::Serial => t(ProtocolMsg::SerialTag),
    }
}

/// 主机的一行说明：SSH / Telnet 是 `user@host:port`（Telnet 没填用户名就省掉 `user@`），
/// 串口是设备名和波特率（没有地址、端口、用户名）。
pub fn host_detail(host: &Host) -> String {
    match host.protocol.protocol {
        Protocol::Serial => {
            let options = host.protocol.serial.clone().unwrap_or_default();
            if options.device.trim().is_empty() {
                format!(
                    "{} · {}",
                    t(ProtocolMsg::SerialUnselected),
                    options.baud_rate
                )
            } else {
                format!("{} · {}", options.device, options.baud_rate)
            }
        }
        Protocol::Ssh | Protocol::Telnet if host.username.trim().is_empty() => {
            format!("{}:{}", host.host, host.port)
        }
        Protocol::Ssh | Protocol::Telnet => {
            format!("{}@{}:{}", host.username, host.host, host.port)
        }
    }
}

/// 搜索主机时要匹配的"地址"：串口用设备名。
pub fn host_search_address(host: &Host) -> String {
    match host.protocol.protocol {
        Protocol::Serial => host
            .protocol
            .serial
            .as_ref()
            .map(|options| options.device.clone())
            .unwrap_or_default(),
        _ => host.host.clone(),
    }
}

// ---- SSH-only 能力 ----

/// 悬停说明：这一项只有 SSH 主机能用。
pub const SSH_ONLY_HINT: &str = "仅 SSH 主机可用";

/// 一台主机上的各项功能是否可用（`None` = 主机不存在，按可用处理，由各入口自己报"找不到"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// 是 SSH 主机（公钥安装这类直接走 SSH 通道的功能看它）。
    pub ssh: bool,
    pub sftp: bool,
    pub ai_exec: bool,
    pub server_status: bool,
    pub port_forward: bool,
    pub x11: bool,
    pub send_break: bool,
}

impl Caps {
    pub const ALL: Self = Self {
        ssh: true,
        sftp: true,
        ai_exec: true,
        server_status: true,
        port_forward: true,
        x11: true,
        send_break: false,
    };

    pub fn of(protocol: Protocol) -> Self {
        Self {
            ssh: protocol == Protocol::Ssh,
            sftp: protocol.supports_sftp(),
            ai_exec: protocol.supports_ai_exec(),
            server_status: protocol.supports_server_status(),
            port_forward: protocol.supports_port_forward(),
            x11: protocol.supports_x11(),
            send_break: protocol == Protocol::Serial,
        }
    }
}

#[cfg(test)]
mod tests {
    use zssh_core::model::{AuthMethod, ProtocolSettings, ProxyMode, SerialOptions};

    use super::*;

    fn host(protocol: ProtocolSettings, username: &str) -> Host {
        Host {
            id: 1,
            group_id: None,
            name: "h".to_owned(),
            host: "192.0.2.7".to_owned(),
            port: 23,
            username: username.to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
            protocol,
        }
    }

    #[test]
    fn the_port_follows_the_protocol_default_only_while_the_user_has_not_touched_it() {
        // 还是自动填的 22 → 换成 Telnet 的 23。
        assert_eq!(follow_default("22", "22", "23"), Some("23".to_owned()));
        // 空的也算没改过。
        assert_eq!(follow_default("", "22", "23"), Some("23".to_owned()));
        assert_eq!(follow_default("  ", "22", "23"), Some("23".to_owned()));
        // 用户改成 2222：不动。
        assert_eq!(follow_default("2222", "22", "23"), None);
        // 本来就是目标值：不用换。
        assert_eq!(follow_default("23", "23", "23"), None);
        // 用户手填的恰好等于新默认值：也不用换。
        assert_eq!(follow_default("23", "22", "23"), None);
    }

    #[test]
    fn default_ports_and_users_per_protocol() {
        assert_eq!(default_port_text(Protocol::Ssh).as_deref(), Some("22"));
        assert_eq!(default_port_text(Protocol::Telnet).as_deref(), Some("23"));
        assert_eq!(default_port_text(Protocol::Serial), None);
        assert_eq!(default_username(Protocol::Ssh), "root");
        assert_eq!(default_username(Protocol::Telnet), "");
    }

    #[test]
    fn each_protocol_shows_only_its_own_fields() {
        let ssh = visible(Protocol::Ssh);
        assert!(ssh.address && ssh.port && ssh.username && ssh.auth_method && ssh.x11);
        assert!(ssh.proxy && ssh.jump && ssh.ssh_advanced && ssh.memory);
        assert!(!ssh.telnet_advanced && !ssh.serial);

        let telnet = visible(Protocol::Telnet);
        assert!(telnet.address && telnet.port && telnet.username && telnet.password);
        assert!(telnet.identity && telnet.proxy && telnet.jump && telnet.telnet_advanced);
        assert!(
            !telnet.auth_method && !telnet.x11 && !telnet.ssh_advanced && !telnet.memory,
            "Telnet 只有密码认证，没有 X11 / 旧算法 / AI 记忆"
        );

        let serial = visible(Protocol::Serial);
        assert!(serial.serial && serial.serial_advanced);
        assert!(
            !serial.address
                && !serial.port
                && !serial.username
                && !serial.password
                && !serial.identity
                && !serial.proxy
                && !serial.jump
                && !serial.x11
                && !serial.memory,
            "串口不显示地址 / 端口 / 用户名 / 身份 / 代理 / 跳板机"
        );
    }

    #[test]
    fn a_saved_host_cannot_switch_between_serial_and_network() {
        assert!(can_switch_to(None, Protocol::Serial));
        assert!(can_switch_to(Some(Protocol::Ssh), Protocol::Telnet));
        assert!(can_switch_to(Some(Protocol::Telnet), Protocol::Ssh));
        assert!(!can_switch_to(Some(Protocol::Ssh), Protocol::Serial));
        assert!(!can_switch_to(Some(Protocol::Serial), Protocol::Telnet));
        assert!(can_switch_to(Some(Protocol::Serial), Protocol::Serial));
    }

    #[test]
    fn core_errors_land_next_to_the_field_they_are_about() {
        let cases = [
            ("主机名不能为空", FormField::Name),
            ("地址不能为空", FormField::Address),
            (
                "地址里不能有空白或控制字符：只能是域名、IPv4 或 IPv6 地址",
                FormField::Address,
            ),
            ("端口要在 1-65535 之间", FormField::Port),
            ("用户名不能为空", FormField::Username),
            ("开了自动登录，用户名不能为空", FormField::AutoLogin),
            (
                "开了自动登录，用户名提示和密码提示都至少要有一条匹配规则",
                FormField::AutoLogin,
            ),
            ("用户名提示最多 8 条", FormField::AutoLogin),
            (
                "终端类型要是 1-64 个可见的 ASCII 字符（例如 xterm-256color）",
                FormField::TerminalType,
            ),
            ("请填串口设备名（例如 COM3）", FormField::Device),
            (
                "串口设备名最多 256 个字符，不能含控制字符",
                FormField::Device,
            ),
            ("波特率要在 50-4000000 之间", FormField::Baud),
            ("5 位数据位不能配 2 个停止位", FormField::Framing),
            ("1.5 个停止位只能配 5 位数据位", FormField::Framing),
            (
                "跳板机必须是 SSH 主机，当前选的是 Telnet 主机",
                FormField::Jump,
            ),
            ("串口主机不能选代理", FormField::Proxy),
            (
                "凭据「x」不是密码凭据，Telnet 主机只能选密码凭据",
                FormField::Identity,
            ),
            (
                "Telnet 主机只能用密码认证（可以不选凭据，连接时再问）",
                FormField::Identity,
            ),
            (
                "X11 转发只能用在 SSH 主机上，Telnet 主机不能开",
                FormField::X11,
            ),
            ("数据库坏了", FormField::General),
        ];
        for (message, field) in cases {
            assert_eq!(locate_error(message), field, "{message}");
        }
    }

    /// 真的让核心库报错，确认它的原文都能被 `locate_error` 找到字段（不会因为改了措辞悄悄落到最下面）。
    #[test]
    fn real_core_validation_errors_are_all_located() {
        use zssh_core::model::{HostDraft, SerialDataBits, SerialStopBits};
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");
        let draft = |name: &str, settings: Option<ProtocolSettings>| HostDraft {
            protocol: settings,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.7".to_owned(),
            port: 23,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        };
        let err = |request: HostDraft| db.host_save(&request).unwrap_err().to_string();

        let mut telnet = ProtocolSettings::new(Protocol::Telnet);
        telnet.telnet.as_mut().unwrap().terminal_type = "bad type".to_owned();
        assert_eq!(
            locate_error(&err(draft("a", Some(telnet)))),
            FormField::TerminalType
        );

        let mut login = ProtocolSettings::new(Protocol::Telnet);
        let options = login.telnet.as_mut().unwrap();
        options.auto_login.enabled = true;
        options.auto_login.username_patterns.clear();
        assert_eq!(
            locate_error(&err(draft("b", Some(login)))),
            FormField::AutoLogin
        );

        let serial = |edit: &dyn Fn(&mut SerialOptions)| {
            let mut settings = ProtocolSettings::new(Protocol::Serial);
            let options = settings.serial.as_mut().unwrap();
            options.device = "COM3".to_owned();
            edit(options);
            Some(settings)
        };
        assert_eq!(
            locate_error(&err(draft("c", serial(&|o| o.device.clear())))),
            FormField::Device
        );
        assert_eq!(
            locate_error(&err(draft("d", serial(&|o| o.baud_rate = 7)))),
            FormField::Baud
        );
        assert_eq!(
            locate_error(&err(draft(
                "e",
                serial(&|o| {
                    o.data_bits = SerialDataBits::Five;
                    o.stop_bits = SerialStopBits::Two;
                })
            ))),
            FormField::Framing
        );

        let mut x11 = ProtocolSettings::new(Protocol::Telnet);
        x11.x11_forwarding = true;
        assert_eq!(locate_error(&err(draft("f", Some(x11)))), FormField::X11);

        let mut bad_port = draft("g", None);
        bad_port.port = 0;
        assert_eq!(locate_error(&err(bad_port)), FormField::Port);
        assert_eq!(locate_error(&err(draft("  ", None))), FormField::Name);
        let mut bad_address = draft("h", None);
        bad_address.host = "a b".to_owned();
        assert_eq!(locate_error(&err(bad_address)), FormField::Address);
    }

    #[test]
    fn patterns_round_trip_through_the_text_box() {
        assert_eq!(
            parse_patterns("login:, Username: ；user:\n  "),
            vec!["login:", "Username:", "user:"]
        );
        assert!(parse_patterns(" , ").is_empty());
        let list = vec!["login:".to_owned(), "user:".to_owned()];
        assert_eq!(parse_patterns(&format_patterns(&list)), list);
    }

    #[test]
    fn baud_text_must_be_a_number() {
        assert_eq!(parse_baud(" 9600 "), Ok(9600));
        assert!(parse_baud("").is_err());
        assert!(parse_baud("115k").is_err());
        assert!(parse_baud("-1").is_err());
        assert!(COMMON_BAUD_RATES.contains(&115_200));
    }

    #[test]
    fn the_device_button_names_the_port_or_says_none_was_found() {
        let ports = vec![PortChoice {
            device: "COM3".to_owned(),
            label: "USB Serial Adapter (COM3)".to_owned(),
        }];
        assert_eq!(
            device_button_label("COM3", &ports),
            "USB Serial Adapter (COM3)"
        );
        assert_eq!(
            device_button_label("com3", &ports),
            "USB Serial Adapter (COM3)"
        );
        assert_eq!(device_button_label("", &ports), "（请选择串口）");
        assert_eq!(device_button_label("", &[]), "未检测到串口");
        assert_eq!(device_button_label("COM9", &ports), "COM9（当前未检测到）");
        assert_eq!(device_button_label("COM9", &[]), "COM9（当前未检测到）");
    }

    #[test]
    fn list_rows_show_the_protocol_and_serial_rows_show_device_and_baud() {
        assert_eq!(protocol_tag(Protocol::Ssh), "SSH");
        assert_eq!(protocol_tag(Protocol::Telnet), "Telnet");
        assert_eq!(protocol_tag(Protocol::Serial), "串口");

        let ssh = host(ProtocolSettings::default(), "ops");
        assert_eq!(host_detail(&ssh), "ops@192.0.2.7:23");

        let telnet = host(ProtocolSettings::new(Protocol::Telnet), "");
        assert_eq!(host_detail(&telnet), "192.0.2.7:23", "没填用户名就不写 @");
        let telnet = host(ProtocolSettings::new(Protocol::Telnet), "admin");
        assert_eq!(host_detail(&telnet), "admin@192.0.2.7:23");

        let mut settings = ProtocolSettings::new(Protocol::Serial);
        {
            let options = settings.serial.as_mut().unwrap();
            options.device = "COM3".to_owned();
            options.baud_rate = 9600;
        }
        let mut serial = host(settings.clone(), "");
        serial.host = String::new();
        serial.port = 0;
        assert_eq!(host_detail(&serial), "COM3 · 9600");
        assert_eq!(host_search_address(&serial), "COM3");
        settings.serial.as_mut().unwrap().device.clear();
        assert_eq!(host_detail(&host(settings, "")), "未选择串口 · 9600");
    }

    #[test]
    fn ssh_only_features_are_greyed_for_telnet_and_serial() {
        let ssh = Caps::of(Protocol::Ssh);
        assert!(
            ssh.ssh && ssh.sftp && ssh.ai_exec && ssh.server_status && ssh.port_forward && ssh.x11
        );
        assert!(!ssh.send_break);
        for protocol in [Protocol::Telnet, Protocol::Serial] {
            let caps = Caps::of(protocol);
            assert!(
                !caps.sftp
                    && !caps.ai_exec
                    && !caps.server_status
                    && !caps.port_forward
                    && !caps.x11,
                "{protocol:?}"
            );
        }
        assert!(Caps::of(Protocol::Serial).send_break);
        assert!(!Caps::of(Protocol::Telnet).send_break);
        assert_eq!(SSH_ONLY_HINT, "仅 SSH 主机可用");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn fake_port_lists_cover_none_and_named_ports() {
        assert!(parse_fake_ports("none").is_empty());
        let ports = parse_fake_ports("COM3|USB Serial Adapter (COM3); COM4");
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].label, "USB Serial Adapter (COM3)");
        assert_eq!(ports[1].label, "COM4");
    }
}
