//! 主机协议（SSH / Telnet / 串口）和各协议的选项，以及"协议 + 其他字段"组合的校验。
//!
//! 协议字段**不放在 [`super::Host`] / [`super::HostDraft`] 里**：这两个结构体在界面代码里到处
//! 用字面量构造，加字段会让界面一起改。协议设置单独成 [`ProtocolSettings`]，按主机 id 存在
//! `hosts` 表的 `protocol` / `x11_forwarding` / `protocol_options` 三列里（见 `store::protocols`）。
//! 老主机没有这些列的值时就是 SSH 默认，所以老代码路径照常工作。

use serde::{Deserialize, Serialize};

use super::{AuthMethod, Identity, IdentityKind, ProxyMode, DEFAULT_SSH_PORT};
use crate::error::ZsshError;

/// Telnet 默认端口。
pub const DEFAULT_TELNET_PORT: u16 = 23;

/// 终端类型（TTYPE）的默认值。
pub const DEFAULT_TELNET_TERMINAL_TYPE: &str = "xterm-256color";

/// 自动登录的模式条数、单条长度上限：这些规则会在每一行输出上匹配，不能无限多。
const MAX_PATTERNS: usize = 8;
const MAX_PATTERN_CHARS: usize = 64;
const MAX_TERMINAL_TYPE_CHARS: usize = 64;
/// 串口设备名长度上限。
const MAX_SERIAL_DEVICE_CHARS: usize = 256;
/// 波特率的合理范围（自定义值也要在里面）。
const MIN_SERIAL_BAUD_RATE: u32 = 50;
const MAX_SERIAL_BAUD_RATE: u32 = 4_000_000;
/// 串口波特率默认值。
pub const DEFAULT_SERIAL_BAUD_RATE: u32 = 115_200;

/// 主机的连接协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Ssh,
    Telnet,
    /// 本机串口。没有地址 / 端口 / 用户名 / 凭据 / 代理 / 跳板机，设备名在 [`SerialOptions`] 里。
    Serial,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Telnet => "telnet",
            Self::Serial => "serial",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ZsshError> {
        match raw {
            "ssh" => Ok(Self::Ssh),
            "telnet" => Ok(Self::Telnet),
            "serial" => Ok(Self::Serial),
            other => Err(ZsshError::InvalidInput(format!("未知的连接协议：{other}"))),
        }
    }

    /// 给用户看的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Ssh => "SSH",
            Self::Telnet => "Telnet",
            Self::Serial => "串口",
        }
    }

    /// 新建主机时按协议填的默认端口（已有主机不动）。
    /// 串口没有端口，返回 `None`。
    pub fn default_port(self) -> Option<u16> {
        match self {
            Self::Ssh => Some(DEFAULT_SSH_PORT),
            Self::Telnet => Some(DEFAULT_TELNET_PORT),
            Self::Serial => None,
        }
    }

    /// 只有 SSH 主机有 SFTP（文件面板、传输、远端编辑）。
    pub fn supports_sftp(self) -> bool {
        self == Self::Ssh
    }

    /// 只有 SSH 主机能让 AI 助手执行命令（工具桥靠 SSH 的 exec 通道和终端）。
    pub fn supports_ai_exec(self) -> bool {
        self == Self::Ssh
    }

    /// 服务器状态（CPU / 内存 / 磁盘…）是在 SSH 连接上跑命令采的。
    pub fn supports_server_status(self) -> bool {
        self == Self::Ssh
    }

    /// 端口转发（本地 / 远程 / 动态）是 SSH 的能力。
    pub fn supports_port_forward(self) -> bool {
        self == Self::Ssh
    }

    /// X11 转发是 SSH 的能力；这里只说"这种协议能不能开"，开没开看
    /// [`ProtocolSettings::x11_forwarding`]。
    pub fn supports_x11(self) -> bool {
        self == Self::Ssh
    }
}

/// Telnet 回车键发送什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelnetNewline {
    Cr,
    #[default]
    CrLf,
    Lf,
}

/// Telnet 本地回显。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalEcho {
    /// 按服务器协商的 ECHO 选项决定
    #[default]
    Auto,
    On,
    Off,
}

/// Telnet / 串口字节流的字符编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextEncoding {
    #[default]
    Utf8,
    Gbk,
    Big5,
    ShiftJis,
    EucKr,
    Latin1,
}

/// Telnet 自动登录：在输出里匹配"用户名 / 密码提示"，匹配到就把用户名 / 密码发过去。
///
/// 用普通子串（不区分大小写）而不是正则：规则每行输出都要匹配，正则可能被构造成很慢。
/// 密码取自主机绑定的密码凭据，这里**不存任何明文**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelnetAutoLogin {
    pub enabled: bool,
    pub username_patterns: Vec<String>,
    pub password_patterns: Vec<String>,
}

impl Default for TelnetAutoLogin {
    fn default() -> Self {
        Self {
            enabled: false,
            username_patterns: ["login:", "username:", "user name:", "user:"]
                .map(str::to_owned)
                .to_vec(),
            password_patterns: ["password:", "passwd:", "passcode:"]
                .map(str::to_owned)
                .to_vec(),
        }
    }
}

/// Telnet 选项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelnetOptions {
    pub newline: TelnetNewline,
    pub local_echo: LocalEcho,
    /// 发送窗口大小（NAWS）
    pub naws: bool,
    /// 回应 TTYPE 协商用的终端类型
    pub terminal_type: String,
    pub encoding: TextEncoding,
    pub auto_login: TelnetAutoLogin,
    /// 纯 TCP：不做任何 Telnet 协商（连的是裸 TCP 服务时用）
    pub raw_tcp: bool,
}

impl Default for TelnetOptions {
    fn default() -> Self {
        Self {
            newline: TelnetNewline::default(),
            local_echo: LocalEcho::default(),
            naws: true,
            terminal_type: DEFAULT_TELNET_TERMINAL_TYPE.to_owned(),
            encoding: TextEncoding::default(),
            auto_login: TelnetAutoLogin::default(),
            raw_tcp: false,
        }
    }
}

/// 串口数据位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SerialDataBits {
    #[serde(rename = "5")]
    Five,
    #[serde(rename = "6")]
    Six,
    #[serde(rename = "7")]
    Seven,
    #[default]
    #[serde(rename = "8")]
    Eight,
}

/// 串口校验位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SerialParity {
    #[default]
    None,
    Odd,
    Even,
    Mark,
    Space,
}

/// 串口停止位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SerialStopBits {
    #[default]
    #[serde(rename = "1")]
    One,
    #[serde(rename = "1.5")]
    OnePointFive,
    #[serde(rename = "2")]
    Two,
}

/// 串口流控。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SerialFlowControl {
    #[default]
    None,
    /// XON/XOFF
    Software,
    /// RTS/CTS
    Hardware,
}

/// 串口回车键发送什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SerialNewline {
    /// 串口控制台（路由器 / 交换机 / 单片机）几乎都认回车 CR
    #[default]
    Cr,
    Lf,
    CrLf,
}

/// 串口选项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SerialOptions {
    /// 设备名，原样存字符串：`COM3`、`\.\COM12`、`/dev/ttyUSB0`……插拔会变，不存枚举。
    pub device: String,
    /// 波特率，允许任意自定义数值
    pub baud_rate: u32,
    pub data_bits: SerialDataBits,
    pub parity: SerialParity,
    pub stop_bits: SerialStopBits,
    pub flow_control: SerialFlowControl,
    pub newline: SerialNewline,
    /// 本地回显，默认关（设备一般自己回显）
    pub local_echo: bool,
    pub encoding: TextEncoding,
}

impl Default for SerialOptions {
    fn default() -> Self {
        Self {
            device: String::new(),
            baud_rate: DEFAULT_SERIAL_BAUD_RATE,
            data_bits: SerialDataBits::default(),
            parity: SerialParity::default(),
            stop_bits: SerialStopBits::default(),
            flow_control: SerialFlowControl::default(),
            newline: SerialNewline::default(),
            local_echo: false,
            encoding: TextEncoding::default(),
        }
    }
}

/// 一台主机的协议设置。`Default` 就是"普通 SSH 主机"。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtocolSettings {
    pub protocol: Protocol,
    /// X11 转发，只对 SSH 有意义
    pub x11_forwarding: bool,
    /// 只有 `protocol == Telnet` 时才有
    pub telnet: Option<TelnetOptions>,
    /// 只有 `protocol == Serial` 时才有
    pub serial: Option<SerialOptions>,
}

/// 校验"协议 + 主机其他字段"组合时要看的主机信息。
#[derive(Debug, Clone, Copy)]
pub struct HostFacts<'a> {
    pub username: &'a str,
    pub auth_method: AuthMethod,
    /// 主机绑定的凭据（已从库里读出来）
    pub identity: Option<&'a Identity>,
    pub proxy_mode: ProxyMode,
    pub proxy_id: Option<i64>,
    /// 跳板机的协议；没有跳板机是 `None`
    pub jump_host_protocol: Option<Protocol>,
}

impl ProtocolSettings {
    /// 新建一台这个协议的主机用的默认设置（带上该协议的默认选项）。
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            x11_forwarding: false,
            telnet: (protocol == Protocol::Telnet).then(TelnetOptions::default),
            serial: (protocol == Protocol::Serial).then(SerialOptions::default),
        }
    }

    /// 是不是"普通 SSH 主机"（没有任何新字段的值）。同步靠它决定要不要用新格式。
    pub fn is_plain_ssh(&self) -> bool {
        self.protocol == Protocol::Ssh
            && !self.x11_forwarding
            && self.telnet.is_none()
            && self.serial.is_none()
    }

    pub fn supports_sftp(&self) -> bool {
        self.protocol.supports_sftp()
    }

    pub fn supports_ai_exec(&self) -> bool {
        self.protocol.supports_ai_exec()
    }

    pub fn supports_server_status(&self) -> bool {
        self.protocol.supports_server_status()
    }

    pub fn supports_port_forward(&self) -> bool {
        self.protocol.supports_port_forward()
    }

    /// 这种协议能不能开 X11 转发。
    pub fn supports_x11(&self) -> bool {
        self.protocol.supports_x11()
    }

    /// 这台主机连接时要不要开 X11 转发（能开且用户开了）。
    pub fn x11_enabled(&self) -> bool {
        self.protocol.supports_x11() && self.x11_forwarding
    }

    /// 给 AI 工具桥看的：这台主机不能当 AI 的执行目标时，返回给模型 / 用户看的原因。
    pub fn ai_exec_refusal(&self) -> Option<String> {
        if self.supports_ai_exec() {
            return None;
        }
        Some(format!(
            "这是一台 {} 主机，AI 助手只能在 SSH 主机上执行命令，不能操作它",
            self.protocol.label()
        ))
    }

    /// 校验并整理：选项和协议对得上、值在范围内、组合合法；返回整理后的设置
    /// （补上该协议缺的默认选项、去掉首尾空白）。
    pub fn validated(mut self, facts: &HostFacts<'_>) -> Result<Self, ZsshError> {
        let protocol = self.protocol;
        if self.x11_forwarding && !protocol.supports_x11() {
            return Err(invalid(format!(
                "X11 转发只能用在 SSH 主机上，{} 主机不能开",
                protocol.label()
            )));
        }
        if protocol != Protocol::Telnet && self.telnet.is_some() {
            return Err(invalid(format!(
                "{} 主机不能带 Telnet 选项",
                protocol.label()
            )));
        }
        if protocol != Protocol::Serial && self.serial.is_some() {
            return Err(invalid(format!("{} 主机不能带串口选项", protocol.label())));
        }

        match protocol {
            Protocol::Ssh => {}
            Protocol::Telnet => check_password_only(protocol, facts)?,
            Protocol::Serial => check_serial_has_no_network_refs(facts)?,
        }
        if let Some(jump) = facts.jump_host_protocol {
            if jump != Protocol::Ssh {
                return Err(invalid(format!(
                    "跳板机必须是 SSH 主机，当前选的是 {} 主机",
                    jump.label()
                )));
            }
        }

        match protocol {
            Protocol::Ssh => {}
            Protocol::Telnet => {
                let mut options = self.telnet.take().unwrap_or_default();
                validate_telnet(&mut options, facts)?;
                self.telnet = Some(options);
            }
            Protocol::Serial => {
                let mut options = self.serial.take().unwrap_or_default();
                validate_serial(&mut options)?;
                self.serial = Some(options);
            }
        }
        Ok(self)
    }

    /// 存进 `hosts.protocol_options` 的 JSON；没有任何协议选项是 `None`。
    pub fn options_json(&self) -> Result<Option<String>, ZsshError> {
        if self.telnet.is_none() && self.serial.is_none() {
            return Ok(None);
        }
        let stored = StoredOptions {
            telnet: self.telnet.clone(),
            serial: self.serial.clone(),
        };
        serde_json::to_string(&stored)
            .map(Some)
            .map_err(|err| invalid(format!("协议选项序列化失败：{err}")))
    }

    /// 从三列还原。选项 JSON 读不了时按默认处理并记一条日志：不能因为一台主机的选项坏了
    /// 让整个主机列表打不开。
    pub fn from_columns(
        protocol: &str,
        x11_forwarding: bool,
        options: Option<&str>,
    ) -> Result<Self, ZsshError> {
        let protocol = Protocol::parse(protocol)?;
        let stored = options
            .filter(|raw| !raw.trim().is_empty())
            .and_then(|raw| match serde_json::from_str::<StoredOptions>(raw) {
                Ok(stored) => Some(stored),
                Err(err) => {
                    tracing::warn!(%err, "主机的协议选项读不出来，按默认值处理");
                    None
                }
            })
            .unwrap_or_default();
        Ok(Self {
            protocol,
            x11_forwarding,
            telnet: (protocol == Protocol::Telnet).then(|| stored.telnet.unwrap_or_default()),
            serial: (protocol == Protocol::Serial).then(|| stored.serial.unwrap_or_default()),
        })
    }
}

/// `protocol_options` 列里的 JSON 形状。
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    telnet: Option<TelnetOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial: Option<SerialOptions>,
}

fn invalid(message: String) -> ZsshError {
    ZsshError::InvalidInput(message)
}

/// 串口主机不连网络：凭据、代理、跳板机都没有意义，选了就报错（而不是悄悄忽略——
/// 用户以为经跳板机连，其实什么都没发生）。主机地址 / 端口 / 用户名 / 认证方式 / 代理策略
/// 是界面默认会带着的值，由存储层统一整理成固定的占位值（见 `store::hosts`）。
fn check_serial_has_no_network_refs(facts: &HostFacts<'_>) -> Result<(), ZsshError> {
    check_serial_refs(
        facts.identity.is_some(),
        facts.proxy_id.is_some(),
        facts.jump_host_protocol.is_some(),
    )
}

/// 串口主机不能选凭据 / 代理 / 跳板机（存储层在查库之前先用它拦，错误信息比"凭据类型不对"清楚）。
pub fn check_serial_refs(identity: bool, proxy: bool, jump_host: bool) -> Result<(), ZsshError> {
    if identity {
        return Err(invalid("串口主机不能选凭据".to_owned()));
    }
    if proxy {
        return Err(invalid("串口主机不能选代理".to_owned()));
    }
    if jump_host {
        return Err(invalid("串口主机不能选跳板机".to_owned()));
    }
    Ok(())
}

/// Telnet 的认证：不能用私钥 / agent，凭据只能是密码类。
fn check_password_only(protocol: Protocol, facts: &HostFacts<'_>) -> Result<(), ZsshError> {
    if facts.auth_method != AuthMethod::Password {
        return Err(invalid(format!(
            "{} 主机只能用密码认证（可以不选凭据，连接时再问）",
            protocol.label()
        )));
    }
    if let Some(identity) = facts.identity {
        if identity.kind != IdentityKind::Password {
            return Err(invalid(format!(
                "凭据「{}」不是密码凭据，{} 主机只能选密码凭据",
                identity.name,
                protocol.label()
            )));
        }
    }
    Ok(())
}

fn validate_telnet(options: &mut TelnetOptions, facts: &HostFacts<'_>) -> Result<(), ZsshError> {
    let terminal_type = options.terminal_type.trim().to_owned();
    if terminal_type.is_empty()
        || terminal_type.chars().count() > MAX_TERMINAL_TYPE_CHARS
        || !terminal_type.chars().all(|c| c.is_ascii_graphic())
    {
        return Err(invalid(format!(
            "终端类型要是 1-{MAX_TERMINAL_TYPE_CHARS} 个可见的 ASCII 字符（例如 {DEFAULT_TELNET_TERMINAL_TYPE}）"
        )));
    }
    options.terminal_type = terminal_type;

    let login = &mut options.auto_login;
    login.username_patterns = clean_patterns("用户名提示", &login.username_patterns)?;
    login.password_patterns = clean_patterns("密码提示", &login.password_patterns)?;
    if login.enabled {
        if login.username_patterns.is_empty() || login.password_patterns.is_empty() {
            return Err(invalid(
                "开了自动登录，用户名提示和密码提示都至少要有一条匹配规则".to_owned(),
            ));
        }
        if facts.username.trim().is_empty() {
            return Err(invalid("开了自动登录，用户名不能为空".to_owned()));
        }
    }
    Ok(())
}

fn validate_serial(options: &mut SerialOptions) -> Result<(), ZsshError> {
    let device = options.device.trim().to_owned();
    if device.is_empty() {
        return Err(invalid("请填串口设备名（例如 COM3）".to_owned()));
    }
    if device.chars().count() > MAX_SERIAL_DEVICE_CHARS || device.chars().any(char::is_control) {
        return Err(invalid(format!(
            "串口设备名最多 {MAX_SERIAL_DEVICE_CHARS} 个字符，不能含控制字符"
        )));
    }
    options.device = device;

    if !(MIN_SERIAL_BAUD_RATE..=MAX_SERIAL_BAUD_RATE).contains(&options.baud_rate) {
        return Err(invalid(format!(
            "波特率要在 {MIN_SERIAL_BAUD_RATE}-{MAX_SERIAL_BAUD_RATE} 之间"
        )));
    }
    // Win32 的 DCB：1.5 个停止位只配 5 位数据位，5 位数据位也不能配 2 个停止位。
    match (options.data_bits, options.stop_bits) {
        (SerialDataBits::Five, SerialStopBits::Two) => {
            return Err(invalid("5 位数据位不能配 2 个停止位".to_owned()));
        }
        (bits, SerialStopBits::OnePointFive) if bits != SerialDataBits::Five => {
            return Err(invalid("1.5 个停止位只能配 5 位数据位".to_owned()));
        }
        _ => {}
    }
    Ok(())
}

fn clean_patterns(what: &str, patterns: &[String]) -> Result<Vec<String>, ZsshError> {
    if patterns.len() > MAX_PATTERNS {
        return Err(invalid(format!("{what}最多 {MAX_PATTERNS} 条")));
    }
    let mut cleaned = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        if pattern.chars().count() > MAX_PATTERN_CHARS || pattern.chars().any(char::is_control) {
            return Err(invalid(format!(
                "{what}每条最多 {MAX_PATTERN_CHARS} 个字符，不能含控制字符"
            )));
        }
        cleaned.push(pattern.to_owned());
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests;
