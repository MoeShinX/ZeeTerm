//! 主机协议（SSH / Telnet / RDP）和各协议的选项，以及"协议 + 其他字段"组合的校验。
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
/// RDP 默认端口。
pub const DEFAULT_RDP_PORT: u16 = 3389;

/// 终端类型（TTYPE）的默认值。
pub const DEFAULT_TELNET_TERMINAL_TYPE: &str = "xterm-256color";

/// 自动登录的模式条数、单条长度上限：这些规则会在每一行输出上匹配，不能无限多。
const MAX_PATTERNS: usize = 8;
const MAX_PATTERN_CHARS: usize = 64;
/// 域 / 终端类型的长度上限。
const MAX_DOMAIN_CHARS: usize = 255;
const MAX_TERMINAL_TYPE_CHARS: usize = 64;
/// 固定分辨率的范围（mstsc 接受的范围）。
const MIN_RDP_DIMENSION: u32 = 200;
const MAX_RDP_DIMENSION: u32 = 8192;

/// 主机的连接协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Ssh,
    Telnet,
    Rdp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Telnet => "telnet",
            Self::Rdp => "rdp",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ZsshError> {
        match raw {
            "ssh" => Ok(Self::Ssh),
            "telnet" => Ok(Self::Telnet),
            "rdp" => Ok(Self::Rdp),
            other => Err(ZsshError::InvalidInput(format!("未知的连接协议：{other}"))),
        }
    }

    /// 给用户看的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Ssh => "SSH",
            Self::Telnet => "Telnet",
            Self::Rdp => "RDP",
        }
    }

    /// 新建主机时按协议填的默认端口（已有主机不动）。
    pub fn default_port(self) -> u16 {
        match self {
            Self::Ssh => DEFAULT_SSH_PORT,
            Self::Telnet => DEFAULT_TELNET_PORT,
            Self::Rdp => DEFAULT_RDP_PORT,
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

/// Telnet 字节流的字符编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelnetEncoding {
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
    pub encoding: TelnetEncoding,
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
            encoding: TelnetEncoding::default(),
            auto_login: TelnetAutoLogin::default(),
            raw_tcp: false,
        }
    }
}

/// RDP 的分辨率模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RdpResolution {
    /// 跟随窗口大小
    #[default]
    FitWindow,
    Fullscreen,
    Fixed {
        width: u32,
        height: u32,
    },
}

/// RDP 音频。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RdpAudio {
    /// 在本机播放
    #[default]
    Local,
    /// 不播放
    None,
}

/// RDP 证书策略。现在只有一种：交给 mstsc 自己弹窗处理；留成枚举是为以后自带 RDP 引擎时
/// 能加"记住指纹"之类的策略而不用改存储格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RdpCertificatePolicy {
    #[default]
    Mstsc,
}

/// RDP 选项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RdpOptions {
    pub domain: Option<String>,
    pub resolution: RdpResolution,
    pub multi_monitor: bool,
    pub clipboard: bool,
    /// 驱动器重定向（把本机磁盘挂给远端），默认关
    pub drives: bool,
    pub audio: RdpAudio,
    /// 连管理员会话（mstsc 的 /admin）
    pub admin_session: bool,
    /// 网络级别身份验证（NLA）
    pub nla: bool,
    pub certificate_policy: RdpCertificatePolicy,
}

impl Default for RdpOptions {
    fn default() -> Self {
        Self {
            domain: None,
            resolution: RdpResolution::default(),
            multi_monitor: false,
            clipboard: true,
            drives: false,
            audio: RdpAudio::default(),
            admin_session: false,
            nla: true,
            certificate_policy: RdpCertificatePolicy::default(),
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
    /// 只有 `protocol == Rdp` 时才有
    pub rdp: Option<RdpOptions>,
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
            rdp: (protocol == Protocol::Rdp).then(RdpOptions::default),
        }
    }

    /// 是不是"普通 SSH 主机"（没有任何新字段的值）。同步靠它决定要不要用新格式。
    pub fn is_plain_ssh(&self) -> bool {
        self.protocol == Protocol::Ssh
            && !self.x11_forwarding
            && self.telnet.is_none()
            && self.rdp.is_none()
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
        if protocol != Protocol::Rdp && self.rdp.is_some() {
            return Err(invalid(format!("{} 主机不能带 RDP 选项", protocol.label())));
        }

        match protocol {
            Protocol::Ssh => {}
            Protocol::Telnet | Protocol::Rdp => check_password_only(protocol, facts)?,
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
            Protocol::Rdp => {
                // mstsc 不会经我们的代理出去；"跟随默认代理"也会被悄悄忽略，所以只接受"不用"。
                if facts.proxy_mode != ProxyMode::None || facts.proxy_id.is_some() {
                    return Err(invalid(
                        "RDP 暂不支持经代理连接，请把代理设为「不使用」（需要穿透时用跳板机）"
                            .to_owned(),
                    ));
                }
                let mut options = self.rdp.take().unwrap_or_default();
                validate_rdp(&mut options)?;
                self.rdp = Some(options);
            }
        }
        Ok(self)
    }

    /// 存进 `hosts.protocol_options` 的 JSON；没有任何协议选项是 `None`。
    pub fn options_json(&self) -> Result<Option<String>, ZsshError> {
        if self.telnet.is_none() && self.rdp.is_none() {
            return Ok(None);
        }
        let stored = StoredOptions {
            telnet: self.telnet.clone(),
            rdp: self.rdp.clone(),
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
            rdp: (protocol == Protocol::Rdp).then(|| stored.rdp.unwrap_or_default()),
        })
    }
}

/// `protocol_options` 列里的 JSON 形状。
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    telnet: Option<TelnetOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rdp: Option<RdpOptions>,
}

fn invalid(message: String) -> ZsshError {
    ZsshError::InvalidInput(message)
}

/// Telnet / RDP 的认证：不能用私钥 / agent，凭据只能是密码类。
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

fn validate_rdp(options: &mut RdpOptions) -> Result<(), ZsshError> {
    if let Some(domain) = options.domain.take() {
        let domain = domain.trim().to_owned();
        if domain.chars().count() > MAX_DOMAIN_CHARS || domain.chars().any(char::is_control) {
            return Err(invalid(format!(
                "域最多 {MAX_DOMAIN_CHARS} 个字符，不能含控制字符或换行"
            )));
        }
        // 空串当"没填"：写进 .rdp 文件的 domain 行不能是空值。
        options.domain = (!domain.is_empty()).then_some(domain);
    }
    if let RdpResolution::Fixed { width, height } = options.resolution {
        let range = MIN_RDP_DIMENSION..=MAX_RDP_DIMENSION;
        if !range.contains(&width) || !range.contains(&height) {
            return Err(invalid(format!(
                "固定分辨率的宽和高都要在 {MIN_RDP_DIMENSION}-{MAX_RDP_DIMENSION} 之间"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
