//! 主机属性对话框（编辑 / 新建）的纯逻辑：表单校验、怎么处理密码和凭据、拼出保存草稿、凭据和认证方式
//! 是否配套。规则取自 0.2.14 的 `HostDetailsPanel.tsx` 和 `authIdentity.ts`，并保留 V2-F3 的
//! 修复（保存已有主机时未展示的字段 `tags` / `sort` 取库里的原值，不丢）。
//!
//! V2-U11 起**密码归主机自己**（同 Xshell）：表单里没有「选哪条密码凭据」，主机的密码就是库里一条
//! 名为「<主机名> 的密码」的密码身份，由 [`plan_save`] 在保存时新建 / 覆盖 / 改名 / 释放；
//! 密钥是共享资源，认证方式选私钥 / ssh-agent 时仍从凭据库里选。
//!
//! 不依赖 GPUI：单测直接盯这些规则。

use zssh_core::model::{
    AuthMethod, Host, HostDraft, Identity, IdentityDraft, IdentityKind, LocalEcho, Protocol,
    ProtocolSettings, ProxyMode, SerialDataBits, SerialFlowControl, SerialNewline, SerialOptions,
    SerialParity, SerialStopBits, TelnetNewline, TelnetOptions, TextEncoding,
};
use zssh_core::store::Database;

use super::host_store::normalize_address;
use super::protocol_form::{FieldError, FormField, parse_baud, parse_patterns};

/// 这种凭据能不能配这种认证方式（同 `identityFitsAuth`，和后端 `check_identity_matches` 一致）。
///
/// 密码认证只能用密码凭据：私钥类凭据的秘密是私钥口令或整段私钥，配给密码认证就会被当成密码
/// 发给服务器。agent 认证不读凭据里的秘密，只列 agent 凭据，免得看起来像会用到。
pub fn identity_fits_auth(kind: IdentityKind, method: AuthMethod) -> bool {
    match method {
        AuthMethod::Password => kind == IdentityKind::Password,
        AuthMethod::Key => matches!(kind, IdentityKind::KeyFile | IdentityKind::KeyInline),
        AuthMethod::Agent => kind == IdentityKind::Agent,
    }
}

pub fn auth_label(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::Password => "密码",
        AuthMethod::Key => "私钥",
        AuthMethod::Agent => "ssh-agent",
    }
}

/// 表单里的值（输入框已经读成字符串）。
#[derive(Clone, Debug)]
pub struct FormInput<'a> {
    pub name: &'a str,
    pub address: &'a str,
    pub port: &'a str,
    pub username: &'a str,
    /// 密码框；留空表示不改已存的密码。
    pub password: &'a str,
    pub group: Option<i64>,
    pub auth: AuthMethod,
    /// 私钥 / ssh-agent 认证时选的凭据（密钥或 agent）。密码认证不看它：主机的密码身份是
    /// 已有主机自己关联的那条（见 [`password_identity`]），表单里没有选它的地方。
    pub identity: Option<i64>,
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
    pub legacy: bool,
    pub protocol: Protocol,
    /// X11 转发（只对 SSH 有意义，别的协议保存时忽略）。
    pub x11: bool,
    pub telnet: TelnetInput<'a>,
    pub serial: SerialInput<'a>,
}

/// Telnet 高级设置里的值（文字框已经读成字符串）。
#[derive(Clone, Debug)]
pub struct TelnetInput<'a> {
    pub newline: TelnetNewline,
    pub local_echo: LocalEcho,
    pub naws: bool,
    pub terminal_type: &'a str,
    pub encoding: TextEncoding,
    pub auto_login: bool,
    /// 用户名 / 密码提示词，逗号分隔。
    pub username_prompts: &'a str,
    pub password_prompts: &'a str,
    pub raw_tcp: bool,
}

/// 串口设置里的值。
#[derive(Clone, Debug)]
pub struct SerialInput<'a> {
    pub device: &'a str,
    pub baud: &'a str,
    pub data_bits: SerialDataBits,
    pub parity: SerialParity,
    pub stop_bits: SerialStopBits,
    pub flow_control: SerialFlowControl,
    pub newline: SerialNewline,
    pub local_echo: bool,
    pub encoding: TextEncoding,
}

impl TelnetInput<'static> {
    /// 没有任何自定义的默认值（测试和新建用）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn defaults() -> Self {
        let options = TelnetOptions::default();
        Self {
            newline: options.newline,
            local_echo: options.local_echo,
            naws: options.naws,
            terminal_type: "xterm-256color",
            encoding: options.encoding,
            auto_login: false,
            username_prompts: "login:, username:, user name:, user:",
            password_prompts: "password:, passwd:, passcode:",
            raw_tcp: false,
        }
    }
}

impl SerialInput<'static> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn defaults() -> Self {
        let options = SerialOptions::default();
        Self {
            device: "",
            baud: "115200",
            data_bits: options.data_bits,
            parity: options.parity,
            stop_bits: options.stop_bits,
            flow_control: options.flow_control,
            newline: options.newline,
            local_echo: options.local_echo,
            encoding: options.encoding,
        }
    }
}

impl FormInput<'_> {
    /// 按表单里的值拼出协议设置（不校验，校验交给核心库）。
    pub fn protocol_settings(&self) -> Result<ProtocolSettings, FieldError> {
        let mut settings = ProtocolSettings::new(self.protocol);
        settings.x11_forwarding = self.protocol.supports_x11() && self.x11;
        match self.protocol {
            Protocol::Ssh => {}
            Protocol::Telnet => {
                let input = &self.telnet;
                let mut options = TelnetOptions {
                    newline: input.newline,
                    local_echo: input.local_echo,
                    naws: input.naws,
                    terminal_type: input.terminal_type.trim().to_owned(),
                    encoding: input.encoding,
                    raw_tcp: input.raw_tcp,
                    ..TelnetOptions::default()
                };
                options.auto_login.enabled = input.auto_login;
                options.auto_login.username_patterns = parse_patterns(input.username_prompts);
                options.auto_login.password_patterns = parse_patterns(input.password_prompts);
                settings.telnet = Some(options);
            }
            Protocol::Serial => {
                let input = &self.serial;
                let baud_rate = parse_baud(input.baud)
                    .map_err(|message| FieldError::new(FormField::Baud, message))?;
                settings.serial = Some(SerialOptions {
                    device: input.device.trim().to_owned(),
                    baud_rate,
                    data_bits: input.data_bits,
                    parity: input.parity,
                    stop_bits: input.stop_bits,
                    flow_control: input.flow_control,
                    newline: input.newline,
                    local_echo: input.local_echo,
                    encoding: input.encoding,
                });
            }
        }
        Ok(settings)
    }
}

/// 保存前要对凭据库做的事（只涉及这台主机的密码身份）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityAction {
    /// 不动凭据库。
    None,
    /// 这台主机独占的密码身份：用填的密码覆盖它（名称可能顺带跟着主机改名）。
    Overwrite { id: i64, draft: IdentityDraftLite },
    /// 新建一条「<名称> 的密码」并关联到这台主机。
    Create { draft: IdentityDraftLite },
    /// 密码没改，只是主机改了名：独占的、名字还是「<旧名> 的密码」的那条跟着改名（密文不动）。
    Rename { id: i64, name: String },
}

/// 密码凭据草稿里需要的几项（完整草稿在调用方拼，这里只描述要存什么）。
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityDraftLite {
    pub name: String,
    pub secret: String,
}

/// 手写：`secret` 是明文密码，不能进日志 / 断言输出。
impl std::fmt::Debug for IdentityDraftLite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityDraftLite")
            .field("name", &self.name)
            .field("secret", &"<已隐藏>")
            .finish()
    }
}

impl IdentityDraftLite {
    pub fn into_draft(self, id: Option<i64>) -> IdentityDraft {
        IdentityDraft {
            id,
            name: self.name,
            kind: IdentityKind::Password,
            key_path: None,
            secret: Some(self.secret),
            passphrase: None,
        }
    }
}

/// 保存要做的全部：先（可能）动凭据库，再存主机，最后（可能）释放它不再用的独占密码。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    pub identity: IdentityAction,
    /// 主机存好之后要删的旧密码身份：认证方式从密码改成私钥 / agent 时，原来那条只有这台主机在用的。
    /// 删之前 `identity_delete` 还会再查一次引用，被别处用上了就保留。
    pub release: Option<i64>,
    pub draft: HostDraft,
}

/// 主机密码身份的默认名。
pub fn default_password_name(host_name: &str) -> String {
    format!("{host_name} 的密码")
}

/// 已有主机现在关联的**密码**身份（关联的是密钥 / agent 或没有关联就是 `None`）。
/// 密码认证时主机的密码就是它；表单里没有选它的地方。
pub fn password_identity<'a>(
    existing: Option<&Host>,
    identities: &'a [Identity],
) -> Option<&'a Identity> {
    let id = existing?.identity_id?;
    identities
        .iter()
        .find(|item| item.id == id && item.kind == IdentityKind::Password)
}

/// 除了 `host_id` 这台，还有没有别的主机在用这条身份。
fn used_by_others(hosts: &[Host], host_id: Option<i64>, identity_id: i64) -> bool {
    hosts
        .iter()
        .any(|host| Some(host.id) != host_id && host.identity_id == Some(identity_id))
}

/// 私钥 / ssh-agent 认证时下拉里列哪些凭据：私钥认证只列密钥类，agent 只列 agent，密码认证不列
/// （密码归主机，不从凭据库里选）。
pub fn pickable_identities(identities: &[Identity], method: AuthMethod) -> Vec<&Identity> {
    if method == AuthMethod::Password {
        return Vec::new();
    }
    identities
        .iter()
        .filter(|item| identity_fits_auth(item.kind, method))
        .collect()
}

/// 认证类别里按顺序显示哪些字段（对话框照它画；没有「保存的凭据」下拉）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthField {
    Username,
    Method,
    /// 密码（带小眼睛）。
    Password,
    /// 私钥认证：从凭据库选密钥。
    KeyPicker,
    /// ssh-agent 认证：可选的 agent 凭据。
    AgentPicker,
    /// Telnet 的自动登录开关和提示词。
    AutoLogin,
}

impl AuthField {
    /// 调试脚本里断言用的短名字。
    #[cfg(debug_assertions)]
    pub fn id(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Method => "method",
            Self::Password => "password",
            Self::KeyPicker => "key",
            Self::AgentPicker => "agent",
            Self::AutoLogin => "autologin",
        }
    }
}

pub fn auth_fields(protocol: Protocol, method: AuthMethod) -> Vec<AuthField> {
    let mut fields = vec![AuthField::Username];
    match protocol {
        Protocol::Ssh => {
            fields.push(AuthField::Method);
            fields.push(match method {
                AuthMethod::Password => AuthField::Password,
                AuthMethod::Key => AuthField::KeyPicker,
                AuthMethod::Agent => AuthField::AgentPicker,
            });
        }
        Protocol::Telnet => {
            fields.push(AuthField::Password);
            fields.push(AuthField::AutoLogin);
        }
        Protocol::Serial => fields.clear(),
    }
    fields
}

/// 检查表单、决定怎么处理密码，拼出保存草稿。
///
/// - 名称不能为空；SSH / Telnet 还要地址、端口 1–65535，SSH 还要用户名（Telnet 可以不填）；
///   串口要设备名和波特率，不看地址 / 端口 / 用户名；
/// - **密码归主机**（认证方式是密码，Telnet 恒为密码）：主机的密码身份是它现在关联的那条密码身份
///   （[`password_identity`]），`hosts` 用来判断有没有别的主机也在用它：
///   - 填了「真的改过」的密码（点小眼睛取出来看了一眼、没改的不算）：新建主机 / 原来没有密码身份 /
///     原来关联的是密钥 → 新建一条「<名称> 的密码」并关联；关联的密码身份只有自己在用 → 覆盖它；
///     还有别的主机在用（旧数据里共用的）→ 不动它，新建一条并改关联；
///   - 密码没改：关联不变（共用的旧密码照常能用）；原来关联的是密钥 / agent 就解除关联（密钥不删）；
/// - 主机改名时，独占的、名字还是「<旧名> 的密码」的那条密码身份跟着改名；用户自己改过名的不动；
/// - 认证方式是私钥 / agent：关联到选的凭据（类型对不上就不关联）；原来那条独占的密码身份放进
///   `release`，存完主机后删掉，共用的保留；密码框里的字一律不写进保险库；
/// - 地址整理成裸地址（IPv6 去方括号）；已有主机的 `tags` / `sort` 取库里的原值（F3 X1）；
/// - 不适用于当前协议的字段（已填的值还在表单里）保存时按协议忽略：Telnet 只用密码认证，
///   串口不带凭据 / 代理 / 跳板机，X11 只给 SSH；
/// - 协议设置和库里一样就不带（`None`），免得把这一版不认识的选项键洗掉。
///
/// 错误带着该显示在哪个字段旁边。
pub fn plan_save(
    input: &FormInput<'_>,
    existing: Option<&Host>,
    identities: &[Identity],
    hosts: &[Host],
    revealed: Option<(i64, &str)>,
) -> Result<SavePlan, FieldError> {
    let protocol = input.protocol;
    let name = input.name.trim();
    let username = input.username.trim();
    let serial = protocol == Protocol::Serial;
    let required = match protocol {
        Protocol::Ssh => "名称、地址、用户名都不能为空",
        Protocol::Telnet => "名称、地址都不能为空",
        Protocol::Serial => "名称不能为空",
    };
    if name.is_empty() {
        return Err(FieldError::new(FormField::Name, required));
    }
    if !serial && input.address.trim().is_empty() {
        return Err(FieldError::new(FormField::Address, required));
    }
    if protocol == Protocol::Ssh && username.is_empty() {
        return Err(FieldError::new(FormField::Username, required));
    }
    let port = if serial {
        0
    } else {
        input
            .port
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|port| (1..=65535).contains(port))
            .ok_or_else(|| FieldError::new(FormField::Port, "端口要在 1-65535 之间"))?
            as u16
    };
    if serial && input.serial.device.trim().is_empty() {
        return Err(FieldError::new(FormField::Device, "请选择或填写串口设备"));
    }
    let settings = input.protocol_settings()?;

    // Telnet 只有密码认证；串口没有认证。表单里残留的私钥 / agent 选择在这里按协议忽略。
    let auth = if protocol == Protocol::Ssh {
        input.auth
    } else {
        AuthMethod::Password
    };
    let host_id = existing.map(|host| host.id);
    let current = password_identity(existing, identities);
    let exclusive = current.filter(|item| !used_by_others(hosts, host_id, item.id));
    // 主机改名了，并且独占的那条密码身份还叫「<旧名> 的密码」：跟着改名。
    let follow_name = |item: &Identity| -> Option<String> {
        let old = existing?.name.as_str();
        (old != name && item.name == default_password_name(old))
            .then(|| default_password_name(name))
    };

    let mut action = IdentityAction::None;
    let mut release = None;
    let mut identity_id = None;
    if serial {
        release = exclusive.map(|item| item.id);
    } else if auth == AuthMethod::Password {
        let unchanged_reveal = matches!(
            (revealed, current),
            (Some((id, value)), Some(item)) if id == item.id && value == input.password
        );
        let password_changed = !input.password.is_empty() && !unchanged_reveal;
        match (current, exclusive, password_changed) {
            (Some(item), Some(_), true) => {
                identity_id = Some(item.id);
                action = IdentityAction::Overwrite {
                    id: item.id,
                    draft: IdentityDraftLite {
                        name: follow_name(item).unwrap_or_else(|| item.name.clone()),
                        secret: input.password.to_owned(),
                    },
                };
            }
            // 没有密码身份，或者它还被别的主机用着（不能动）：给本主机新建一条自己的。
            (_, _, true) => {
                action = IdentityAction::Create {
                    draft: IdentityDraftLite {
                        name: default_password_name(name),
                        secret: input.password.to_owned(),
                    },
                };
            }
            (Some(item), exclusive, false) => {
                identity_id = Some(item.id);
                if exclusive.is_some()
                    && let Some(new_name) = follow_name(item)
                {
                    action = IdentityAction::Rename {
                        id: item.id,
                        name: new_name,
                    };
                }
            }
            (None, _, false) => {}
        }
    } else {
        identity_id = input.identity.filter(|id| {
            identities
                .iter()
                .any(|item| item.id == *id && identity_fits_auth(item.kind, auth))
        });
        release = exclusive.map(|item| item.id);
    }

    let protocol_change = match existing {
        Some(host) if host.protocol == settings => None,
        None if settings.is_plain_ssh() => None,
        _ => Some(settings),
    };
    Ok(SavePlan {
        identity: action,
        release,
        draft: HostDraft {
            id: existing.map(|host| host.id),
            group_id: input.group,
            name: name.to_owned(),
            host: if serial {
                String::new()
            } else {
                normalize_address(input.address)
            },
            port,
            username: if serial {
                String::new()
            } else {
                username.to_owned()
            },
            auth_method: auth,
            identity_id,
            proxy_id: if serial { None } else { input.proxy },
            jump_host_id: if serial { None } else { input.jump },
            proxy_mode: if serial {
                ProxyMode::None
            } else {
                input.proxy_mode
            },
            // 标签、排序这张表单不编辑：已有主机保持库里的原值，新建用默认值。
            tags: existing.map(|host| host.tags.clone()).unwrap_or_default(),
            sort: existing.map_or(0, |host| host.sort),
            legacy_algorithms: protocol == Protocol::Ssh && input.legacy,
            protocol: protocol_change,
        },
    })
}

/// 照 [`SavePlan`] 落库：先动密码身份，再存主机，最后释放不再用的独占密码。
///
/// 主机没存成功时，刚新建的那条密码身份删掉（免得留下没人用的孤儿）；覆盖 / 改名没法回滚，
/// 但它们都只动这台主机自己独占的身份。释放失败不算保存失败（主机已经存好了，只是留着一条没人用的
/// 密码，用户能在凭据页看到并删除），只记日志；日志里没有密码。
pub fn apply_save(db: &Database, plan: SavePlan) -> Result<Host, String> {
    let SavePlan {
        identity,
        release,
        mut draft,
    } = plan;
    let mut created = None;
    match identity {
        IdentityAction::None => {}
        IdentityAction::Overwrite { id, draft: lite } => {
            db.identity_save(&lite.into_draft(Some(id)))
                .map_err(|err| err.to_string())?;
        }
        IdentityAction::Create { draft: lite } => {
            let made = db
                .identity_save(&lite.into_draft(None))
                .map_err(|err| err.to_string())?;
            draft.identity_id = Some(made.id);
            created = Some(made.id);
        }
        IdentityAction::Rename { id, name } => {
            // secret 为 None：只改名，密文原样保留。
            db.identity_save(&IdentityDraft {
                id: Some(id),
                name,
                kind: IdentityKind::Password,
                key_path: None,
                secret: None,
                passphrase: None,
            })
            .map_err(|err| err.to_string())?;
        }
    }
    let saved = match db.host_save(&draft) {
        Ok(saved) => saved,
        Err(err) => {
            if let Some(id) = created {
                let _ = db.identity_delete(id);
            }
            return Err(err.to_string());
        }
    };
    if let Some(id) = release {
        match db.identity_delete(id) {
            // 两步之间被别处引用了：保留，不算错误。
            Ok(()) | Err(zssh_core::error::ZsshError::IdentityInUse { .. }) => {}
            Err(err) => tracing::warn!(%err, identity = id, "释放主机不再用的密码失败"),
        }
    }
    Ok(saved)
}

/// 表单打开时的代理方式：新建主机默认「跟随默认代理」，编辑已有主机显示它保存的值。
/// 串口主机不用代理由保存时的规则保证（`plan_save` 对串口一律写「不用代理」），这里不特殊处理。
pub fn initial_proxy_mode(existing: Option<&Host>) -> ProxyMode {
    existing.map_or(ProxyMode::Default, |host| host.proxy_mode)
}

/// 表单里「连接方式」三段（V2-U13）：直接连接 / SSH 跳板 / 代理访问。
///
/// 数据模型没有变：一台主机仍然是 `proxy_mode`（跟随默认 / 不用 / 指定）+ `proxy` + `jump_host_id` 三个字段。
/// 三段只是它们的一种看法，映射规则（打开表单时 [`initial_conn_mode`]，保存时 [`effective_routing`]）：
///
/// | 打开时的数据 | 显示的段 |
/// |---|---|
/// | 设了跳板机（不管代理怎么设） | SSH 跳板；有代理设置（不是「不用代理」）时下面同时显示它，保存不丢 |
/// | 没跳板机，代理方式「不用代理」 | 直接连接 |
/// | 没跳板机，代理方式「跟随默认代理」或「指定代理」 | 代理访问（新建主机默认就是这个 + 跟随默认代理） |
///
/// 运行时的真实语义：设了跳板机时，只有第一跳（跳板机自己）的代理生效，目标主机自己的代理设置不参与连接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnMode {
    Direct,
    Jump,
    Proxy,
}

impl ConnMode {
    /// 分段按钮从左到右的顺序。
    pub const ALL: [ConnMode; 3] = [Self::Direct, Self::Jump, Self::Proxy];

    pub fn label(self) -> &'static str {
        match self {
            Self::Direct => "直接连接",
            Self::Jump => "SSH 跳板",
            Self::Proxy => "代理访问",
        }
    }

    /// 调试脚本里用的短名字。
    #[cfg(debug_assertions)]
    pub fn id(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Jump => "jump",
            Self::Proxy => "proxy",
        }
    }
}

/// 打开表单时显示哪一段：新建主机是「代理访问 + 跟随默认代理」（V2-U8 的默认）。
pub fn initial_conn_mode(existing: Option<&Host>) -> ConnMode {
    match existing {
        None => ConnMode::Proxy,
        Some(host) if host.jump_host_id.is_some() => ConnMode::Jump,
        Some(host) => match host.proxy_mode {
            ProxyMode::None => ConnMode::Direct,
            ProxyMode::Default | ProxyMode::Custom => ConnMode::Proxy,
        },
    }
}

/// 一台既有跳板机又有代理设置的旧主机：「SSH 跳板」段下面要把代理设置也显示出来，保存时带着。
pub fn jump_keeps_proxy(existing: Option<&Host>) -> bool {
    existing.is_some_and(|host| host.jump_host_id.is_some() && host.proxy_mode != ProxyMode::None)
}

/// 保存时真正写进库里的三个字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Routing {
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
}

/// 按选中的段和表单里的值算出要保存的代理 / 跳板机：
/// - 直接连接：不用代理、不用跳板机（也不留指向某个代理的引用，免得它挡住那个代理的删除）；
/// - SSH 跳板：写选的跳板机；代理设置只有旧数据里本来就两个都有（`keeps_proxy`）时才带着，
///   否则清掉（跳板链里只有跳板机自己的代理生效，目标的代理没有用）；
/// - 代理访问：不用跳板机；代理方式取表单里的（跟随默认 / 指定），原来是「不用代理」就落成跟随默认。
pub fn effective_routing(
    mode: ConnMode,
    proxy_mode: ProxyMode,
    proxy: Option<i64>,
    jump: Option<i64>,
    keeps_proxy: bool,
) -> Routing {
    match mode {
        ConnMode::Direct => Routing {
            proxy_mode: ProxyMode::None,
            proxy: None,
            jump: None,
        },
        ConnMode::Jump if keeps_proxy => Routing {
            proxy_mode,
            proxy,
            jump,
        },
        ConnMode::Jump => Routing {
            proxy_mode: ProxyMode::None,
            proxy: None,
            jump,
        },
        ConnMode::Proxy => Routing {
            proxy_mode: if proxy_mode == ProxyMode::None {
                ProxyMode::Default
            } else {
                proxy_mode
            },
            proxy,
            jump: None,
        },
    }
}

/// 原来存的组合就不配套（旧数据或同步来的）：返回要取消选择的原因。
pub fn mismatch_note(identity: &Identity, method: AuthMethod) -> Option<String> {
    (!identity_fits_auth(identity.kind, method))
        .then(|| format!("凭据「{}」和认证方式不配套，已取消选择", identity.name))
}

/// 密码框旁边的提示。
pub fn password_hint(editing: bool, identity_has_secret: bool) -> &'static str {
    if editing && identity_has_secret {
        "留空表示不改保险库里的密码"
    } else {
        "留空表示不保存密码，连接时会提示"
    }
}

/// 密码框右边的小眼睛什么时候出现：选了存过密码的密码凭据（能从保险库取出来看），或者框里有字。
pub fn can_reveal(identity: Option<&Identity>, method: AuthMethod) -> bool {
    method == AuthMethod::Password
        && identity.is_some_and(|item| item.kind == IdentityKind::Password && item.has_secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: i64) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: Some(3),
            name: "web".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 2222,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: Some(7),
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: vec!["blue".to_owned(), "edge".to_owned()],
            sort: 4,
            legacy_algorithms: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn new_hosts_follow_the_default_proxy_and_existing_ones_keep_theirs() {
        assert_eq!(initial_proxy_mode(None), ProxyMode::Default);
        // 已有主机原样（host() 夹具存的是「不用代理」），不被新默认带偏。
        let none = host(1);
        assert_eq!(initial_proxy_mode(Some(&none)), ProxyMode::None);
        let mut custom = host(2);
        custom.proxy_mode = ProxyMode::Custom;
        assert_eq!(initial_proxy_mode(Some(&custom)), ProxyMode::Custom);
        let mut follow = host(3);
        follow.proxy_mode = ProxyMode::Default;
        assert_eq!(initial_proxy_mode(Some(&follow)), ProxyMode::Default);
    }

    #[test]
    fn a_new_serial_host_still_saves_without_a_proxy() {
        // 新建表单的初值是「跟随默认代理」，串口主机保存时仍然落成「不用代理」。
        let mut form = serial_input();
        form.proxy_mode = initial_proxy_mode(None);
        let plan = plan_save(&form, None, &[], None).expect("合法");
        assert_eq!(plan.draft.proxy_mode, ProxyMode::None);
    }

    fn identity(id: i64, name: &str, kind: IdentityKind, has_secret: bool) -> Identity {
        Identity {
            id,
            name: name.to_owned(),
            kind,
            key_path: None,
            has_secret,
        }
    }

    fn input<'a>() -> FormInput<'a> {
        FormInput {
            name: "web",
            address: "192.0.2.10",
            port: "22",
            username: "ops",
            password: "",
            group: None,
            auth: AuthMethod::Password,
            identity: None,
            proxy_mode: ProxyMode::None,
            proxy: None,
            jump: None,
            legacy: false,
            protocol: Protocol::Ssh,
            x11: false,
            telnet: TelnetInput::defaults(),
            serial: SerialInput::defaults(),
        }
    }

    #[test]
    fn identities_must_fit_the_auth_method() {
        use IdentityKind::*;
        assert!(identity_fits_auth(Password, AuthMethod::Password));
        assert!(
            !identity_fits_auth(KeyFile, AuthMethod::Password),
            "私钥凭据不能当密码发出去"
        );
        assert!(!identity_fits_auth(KeyInline, AuthMethod::Password));
        assert!(identity_fits_auth(KeyFile, AuthMethod::Key));
        assert!(identity_fits_auth(KeyInline, AuthMethod::Key));
        assert!(!identity_fits_auth(Password, AuthMethod::Key));
        assert!(identity_fits_auth(Agent, AuthMethod::Agent));
        assert!(
            !identity_fits_auth(Password, AuthMethod::Agent),
            "agent 只列 agent 凭据"
        );
    }

    #[test]
    fn name_address_and_user_are_required_and_the_port_must_be_valid() {
        for (name, address, user) in [("", "a", "u"), ("n", "  ", "u"), ("n", "a", " ")] {
            let mut form = input();
            form.name = name;
            form.address = address;
            form.username = user;
            assert_eq!(
                plan_save(&form, None, &[], None).unwrap_err(),
                "名称、地址、用户名都不能为空"
            );
        }
        for port in ["", "0", "65536", "abc", "-1", "22.5"] {
            let mut form = input();
            form.port = port;
            assert_eq!(
                plan_save(&form, None, &[], None).unwrap_err(),
                "端口要在 1-65535 之间",
                "{port}"
            );
        }
        let mut form = input();
        form.port = " 65535 ";
        assert_eq!(
            plan_save(&form, None, &[], None).expect("合法").draft.port,
            65535
        );
    }

    #[test]
    fn saving_an_existing_host_keeps_the_tags_and_sort_it_does_not_show() {
        let existing = host(9);
        let mut form = input();
        form.name = "web renamed";
        form.legacy = true;
        let plan = plan_save(&form, Some(&existing), &[], None).expect("合法");
        assert_eq!(plan.draft.id, Some(9));
        assert_eq!(
            plan.draft.tags,
            vec!["blue".to_owned(), "edge".to_owned()],
            "标签不能丢"
        );
        assert_eq!(plan.draft.sort, 4);
        assert_eq!(plan.draft.name, "web renamed");
        let fresh = plan_save(&input(), None, &[], None).expect("合法");
        assert_eq!(fresh.draft.id, None);
        assert!(fresh.draft.tags.is_empty());
        assert_eq!(fresh.draft.sort, 0);
    }

    #[test]
    fn the_address_is_stored_bare_and_trimmed() {
        let mut form = input();
        form.address = "  [2001:db8::1] ";
        assert_eq!(
            plan_save(&form, None, &[], None).expect("合法").draft.host,
            "2001:db8::1"
        );
        form.address = " web.example ";
        assert_eq!(
            plan_save(&form, None, &[], None).expect("合法").draft.host,
            "web.example"
        );
    }

    // ---- V2-U11：密码归主机 ----

    /// 测试里的旧四参调用（不关心别的主机）；要看「别的主机也在用」的用 [`plan_with`]。
    fn plan_save(
        input: &FormInput<'_>,
        existing: Option<&Host>,
        identities: &[Identity],
        revealed: Option<(i64, &str)>,
    ) -> Result<SavePlan, FieldError> {
        super::plan_save(input, existing, identities, &[], revealed)
    }

    fn plan_with(
        input: &FormInput<'_>,
        existing: Option<&Host>,
        identities: &[Identity],
        hosts: &[Host],
        revealed: Option<(i64, &str)>,
    ) -> SavePlan {
        super::plan_save(input, existing, identities, hosts, revealed).expect("合法")
    }

    /// 一台已有的 SSH 密码主机：名「web」，关联密码身份 `identity_id`。
    fn pw_host(id: i64, name: &str, identity_id: Option<i64>) -> Host {
        let mut item = host(id);
        item.name = name.to_owned();
        item.identity_id = identity_id;
        item.auth_method = AuthMethod::Password;
        item
    }

    fn pw_identity(id: i64, name: &str) -> Identity {
        identity(id, name, IdentityKind::Password, true)
    }

    fn lite(name: &str, secret: &str) -> IdentityDraftLite {
        IdentityDraftLite {
            name: name.to_owned(),
            secret: secret.to_owned(),
        }
    }

    #[test]
    fn a_new_host_with_a_password_gets_its_own_identity_and_without_one_gets_none() {
        let mut form = input();
        form.password = "s3cret";
        let plan = plan_with(&form, None, &[], &[], None);
        assert_eq!(
            plan.identity,
            IdentityAction::Create {
                draft: lite("web 的密码", "s3cret")
            }
        );
        assert_eq!(plan.release, None);

        // 没填密码：不建（连接时再问），也没有关联。
        let plan = plan_with(&input(), None, &[], &[], None);
        assert_eq!(plan.identity, IdentityAction::None);
        assert_eq!(plan.draft.identity_id, None);
        assert_eq!(plan.release, None);
    }

    #[test]
    fn a_new_hosts_name_is_trimmed_in_the_password_identity_name() {
        let mut form = input();
        form.name = "  web  ";
        form.password = "pw";
        assert!(matches!(
            plan_with(&form, None, &[], &[], None).identity,
            IdentityAction::Create { draft } if draft.name == "web 的密码"
        ));
    }

    #[test]
    fn changing_the_password_of_an_exclusive_identity_overwrites_it() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [pw_identity(7, "web 的密码")];
        let hosts = [existing.clone(), pw_host(2, "other", Some(8))];
        let mut form = input();
        form.password = "changed";
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(
            plan.identity,
            IdentityAction::Overwrite {
                id: 7,
                draft: lite("web 的密码", "changed")
            }
        );
        assert_eq!(plan.draft.identity_id, Some(7), "关联不变");
        assert_eq!(plan.release, None);
    }

    #[test]
    fn changing_the_password_of_a_shared_identity_leaves_it_alone_and_makes_a_new_one() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [pw_identity(7, "共用密码")];
        // 旧数据里 web 和 db 共用同一条。
        let hosts = [existing.clone(), pw_host(2, "db", Some(7))];
        let mut form = input();
        form.password = "changed";
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(
            plan.identity,
            IdentityAction::Create {
                draft: lite("web 的密码", "changed")
            },
            "共用的那条不能改，别的主机不受影响"
        );
        // 关联由 apply_save 改成新建的那条；计划里先不带旧的。
        assert_eq!(plan.draft.identity_id, None);
        assert_eq!(plan.release, None, "共用的旧密码保留");
    }

    #[test]
    fn switching_a_key_host_to_password_creates_a_password_identity() {
        let mut existing = pw_host(1, "web", Some(5));
        existing.auth_method = AuthMethod::Key;
        let identities = [identity(5, "deploy key", IdentityKind::KeyFile, false)];
        let hosts = [existing.clone()];
        let mut form = input();
        form.auth = AuthMethod::Password;
        form.password = "pw";
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(
            plan.identity,
            IdentityAction::Create {
                draft: lite("web 的密码", "pw")
            }
        );
        assert_eq!(plan.release, None, "密钥不动");

        // 没填密码：只是解除对密钥的关联（密码认证不带密钥），密钥本身不动。
        form.password = "";
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(plan.identity, IdentityAction::None);
        assert_eq!(plan.draft.identity_id, None);
        assert_eq!(plan.release, None);
    }

    #[test]
    fn an_agent_host_switching_to_password_is_handled_like_a_key_host() {
        let mut existing = pw_host(1, "web", Some(5));
        existing.auth_method = AuthMethod::Agent;
        let identities = [identity(5, "agent", IdentityKind::Agent, false)];
        let mut form = input();
        form.password = "pw";
        let plan = plan_with(
            &form,
            Some(&existing),
            &identities,
            std::slice::from_ref(&existing),
            None,
        );
        assert!(matches!(plan.identity, IdentityAction::Create { .. }));
        assert_eq!(plan.release, None);
    }

    #[test]
    fn an_unchanged_password_leaves_the_vault_and_the_link_alone() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [pw_identity(7, "共用密码")];
        // 共用的旧密码、没改密码：关联不变，照常能用。
        let hosts = [existing.clone(), pw_host(2, "db", Some(7))];
        let plan = plan_with(&input(), Some(&existing), &identities, &hosts, None);
        assert_eq!(plan.identity, IdentityAction::None);
        assert_eq!(plan.draft.identity_id, Some(7));
        assert_eq!(plan.release, None);
    }

    #[test]
    fn peeking_at_the_stored_password_is_not_a_change() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [pw_identity(7, "web 的密码")];
        let hosts = [existing.clone()];
        let mut form = input();
        form.password = "stored";
        let plan = plan_with(
            &form,
            Some(&existing),
            &identities,
            &hosts,
            Some((7, "stored")),
        );
        assert_eq!(plan.identity, IdentityAction::None, "看了一眼没改，不重写");
        assert_eq!(plan.draft.identity_id, Some(7));

        // 看了之后又改了：算改。
        form.password = "stored2";
        let plan = plan_with(
            &form,
            Some(&existing),
            &identities,
            &hosts,
            Some((7, "stored")),
        );
        assert!(matches!(
            plan.identity,
            IdentityAction::Overwrite { id: 7, .. }
        ));

        // 取回来的是别条身份的密码：不能当成没改。
        let plan = plan_with(
            &form,
            Some(&existing),
            &identities,
            &hosts,
            Some((8, "stored2")),
        );
        assert!(matches!(
            plan.identity,
            IdentityAction::Overwrite { id: 7, .. }
        ));
    }

    #[test]
    fn switching_from_password_to_key_releases_an_exclusive_password() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [
            pw_identity(7, "web 的密码"),
            identity(5, "deploy key", IdentityKind::KeyFile, false),
        ];
        let hosts = [existing.clone(), pw_host(2, "db", Some(8))];
        let mut form = input();
        form.auth = AuthMethod::Key;
        form.identity = Some(5);
        form.password = "typed but must not be saved";
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(plan.draft.auth_method, AuthMethod::Key);
        assert_eq!(plan.draft.identity_id, Some(5));
        assert_eq!(plan.release, Some(7), "独占的旧密码存完主机后删掉");
        assert_eq!(
            plan.identity,
            IdentityAction::None,
            "私钥登录时密码框里的字不能写进保险库"
        );
        // agent 同理。
        let mut agent = form.clone();
        agent.auth = AuthMethod::Agent;
        agent.identity = None;
        let plan = plan_with(&agent, Some(&existing), &identities, &hosts, None);
        assert_eq!(plan.release, Some(7));
        assert_eq!(plan.draft.identity_id, None);
    }

    #[test]
    fn switching_from_password_to_key_keeps_a_shared_password() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [
            pw_identity(7, "共用密码"),
            identity(5, "deploy key", IdentityKind::KeyFile, false),
        ];
        let hosts = [existing.clone(), pw_host(2, "db", Some(7))];
        let mut form = input();
        form.auth = AuthMethod::Key;
        form.identity = Some(5);
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(plan.release, None, "db 还在用，保留");
        assert_eq!(plan.draft.identity_id, Some(5));
    }

    #[test]
    fn a_key_host_only_links_a_fitting_credential() {
        let identities = [
            pw_identity(7, "p"),
            identity(5, "k", IdentityKind::KeyInline, true),
            identity(6, "a", IdentityKind::Agent, false),
        ];
        let mut form = input();
        form.auth = AuthMethod::Key;
        for (picked, expect) in [(5, Some(5)), (7, None), (6, None), (99, None)] {
            form.identity = Some(picked);
            assert_eq!(
                plan_with(&form, None, &identities, &[], None)
                    .draft
                    .identity_id,
                expect,
                "私钥认证选 {picked}"
            );
        }
        form.auth = AuthMethod::Agent;
        form.identity = Some(6);
        assert_eq!(
            plan_with(&form, None, &identities, &[], None)
                .draft
                .identity_id,
            Some(6)
        );
        form.identity = Some(5);
        assert_eq!(
            plan_with(&form, None, &identities, &[], None)
                .draft
                .identity_id,
            None
        );
    }

    #[test]
    fn renaming_a_host_renames_its_exclusive_default_named_password() {
        let existing = pw_host(1, "web", Some(7));
        let identities = [pw_identity(7, "web 的密码")];
        let hosts = [existing.clone()];
        let mut form = input();
        form.name = "web-new";
        // 只改名：密文不动，只改名字。
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(
            plan.identity,
            IdentityAction::Rename {
                id: 7,
                name: "web-new 的密码".to_owned()
            }
        );
        assert_eq!(plan.draft.identity_id, Some(7));
        // 改名同时改密码：一次覆盖，名字一起改。
        form.password = "pw2";
        let plan = plan_with(&form, Some(&existing), &identities, &hosts, None);
        assert_eq!(
            plan.identity,
            IdentityAction::Overwrite {
                id: 7,
                draft: lite("web-new 的密码", "pw2")
            }
        );
    }

    #[test]
    fn renaming_does_not_touch_a_custom_named_or_shared_password() {
        let existing = pw_host(1, "web", Some(7));
        let mut form = input();
        form.name = "web-new";
        // 用户自己改过名的：不动。
        let custom = [pw_identity(7, "运维账号")];
        let plan = plan_with(
            &form,
            Some(&existing),
            &custom,
            std::slice::from_ref(&existing),
            None,
        );
        assert_eq!(plan.identity, IdentityAction::None);
        // 名字是默认格式但还有别的主机在用：不是它独占的，不动。
        let default_named = [pw_identity(7, "web 的密码")];
        let hosts = [existing.clone(), pw_host(2, "db", Some(7))];
        let plan = plan_with(&form, Some(&existing), &default_named, &hosts, None);
        assert_eq!(plan.identity, IdentityAction::None);
        // 名字没变（只是前后有空格）：不算改名。
        form.name = " web ";
        let plan = plan_with(
            &form,
            Some(&existing),
            &default_named,
            std::slice::from_ref(&existing),
            None,
        );
        assert_eq!(plan.identity, IdentityAction::None);
    }

    #[test]
    fn a_serial_host_never_touches_credentials() {
        let mut form = serial_input();
        form.identity = Some(7);
        form.password = "typed";
        let plan = plan_with(&form, None, &[pw_identity(7, "p")], &[], None);
        assert_eq!(plan.identity, IdentityAction::None);
        assert_eq!((plan.draft.identity_id, plan.release), (None, None));
    }

    #[test]
    fn the_auth_category_has_no_saved_credential_picker_for_passwords() {
        use AuthField::*;
        assert_eq!(
            auth_fields(Protocol::Ssh, AuthMethod::Password),
            [Username, Method, Password],
            "密码认证：只有用户名和密码"
        );
        assert_eq!(
            auth_fields(Protocol::Ssh, AuthMethod::Key),
            [Username, Method, KeyPicker]
        );
        assert_eq!(
            auth_fields(Protocol::Ssh, AuthMethod::Agent),
            [Username, Method, AgentPicker]
        );
        // Telnet 只有密码：表单里残留的认证方式不影响。
        for method in [AuthMethod::Password, AuthMethod::Key, AuthMethod::Agent] {
            assert_eq!(
                auth_fields(Protocol::Telnet, method),
                [Username, Password, AutoLogin]
            );
        }
        assert!(auth_fields(Protocol::Serial, AuthMethod::Password).is_empty());
    }

    #[test]
    fn the_key_picker_lists_only_keys() {
        let identities = [
            pw_identity(1, "p"),
            identity(2, "file", IdentityKind::KeyFile, false),
            identity(3, "inline", IdentityKind::KeyInline, true),
            identity(4, "agent", IdentityKind::Agent, false),
        ];
        let ids = |method| {
            pickable_identities(&identities, method)
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(AuthMethod::Key),
            [2, 3],
            "私钥文件和导入的私钥，没有密码、agent"
        );
        assert_eq!(ids(AuthMethod::Agent), [4]);
        assert!(ids(AuthMethod::Password).is_empty(), "密码不从凭据库里选");
    }

    #[test]
    fn the_password_identity_is_the_hosts_linked_password_only() {
        let identities = [
            pw_identity(7, "p"),
            identity(5, "k", IdentityKind::KeyFile, false),
        ];
        assert_eq!(
            password_identity(Some(&pw_host(1, "a", Some(7))), &identities).map(|item| item.id),
            Some(7)
        );
        assert!(password_identity(Some(&pw_host(1, "a", Some(5))), &identities).is_none());
        assert!(password_identity(Some(&pw_host(1, "a", None)), &identities).is_none());
        assert!(password_identity(Some(&pw_host(1, "a", Some(99))), &identities).is_none());
        assert!(password_identity(None, &identities).is_none());
    }

    #[test]
    fn a_mismatched_saved_identity_is_dropped_with_a_note() {
        let key = identity(5, "deploy key", IdentityKind::KeyFile, true);
        assert_eq!(
            mismatch_note(&key, AuthMethod::Password).as_deref(),
            Some("凭据「deploy key」和认证方式不配套，已取消选择")
        );
        assert_eq!(mismatch_note(&key, AuthMethod::Key), None);
    }

    #[test]
    fn password_hint_and_the_eye_follow_what_is_stored() {
        assert_eq!(password_hint(true, true), "留空表示不改保险库里的密码");
        assert_eq!(
            password_hint(false, true),
            "留空表示不保存密码，连接时会提示"
        );
        assert_eq!(
            password_hint(true, false),
            "留空表示不保存密码，连接时会提示"
        );
        let stored = identity(1, "p", IdentityKind::Password, true);
        let empty = identity(2, "q", IdentityKind::Password, false);
        let key = identity(3, "k", IdentityKind::KeyInline, true);
        assert!(can_reveal(Some(&stored), AuthMethod::Password));
        assert!(!can_reveal(Some(&empty), AuthMethod::Password));
        assert!(!can_reveal(Some(&key), AuthMethod::Password));
        assert!(!can_reveal(Some(&stored), AuthMethod::Key));
        assert!(!can_reveal(None, AuthMethod::Password));
    }

    #[test]
    fn debug_output_never_shows_the_password() {
        let lite = IdentityDraftLite {
            name: "n".to_owned(),
            secret: "hunter2".to_owned(),
        };
        assert!(!format!("{lite:?}").contains("hunter2"));
        // 整个计划的 Debug（断言失败时会被打印）也不带密码。
        let mut form = input();
        form.password = "hunter2";
        let plan = plan_with(&form, None, &[], &[], None);
        assert!(!format!("{plan:?}").contains("hunter2"));
    }

    // ---- 经过真实的库：apply_save ----

    fn db_identity(
        db: &Database,
        name: &str,
        kind: IdentityKind,
        secret: Option<&str>,
    ) -> Identity {
        db.identity_save(&IdentityDraft {
            id: None,
            name: name.to_owned(),
            kind,
            key_path: (kind == IdentityKind::KeyFile).then(|| "keys/id_test".to_owned()),
            secret: secret.map(str::to_owned),
            passphrase: None,
        })
        .expect("凭据")
    }

    /// 按当前库的状态跑一遍：读主机 / 凭据 / 主机列表 → plan_save → apply_save。
    fn save(
        db: &Database,
        form: &FormInput<'_>,
        host_id: Option<i64>,
        revealed: Option<(i64, &str)>,
    ) -> Host {
        let existing = host_id.map(|id| db.host_get(id).expect("主机"));
        let plan = super::plan_save(
            form,
            existing.as_ref(),
            &db.identity_list().expect("凭据"),
            &db.host_list().expect("主机列表"),
            revealed,
        )
        .expect("合法");
        apply_save(db, plan).expect("保存")
    }

    fn names(db: &Database) -> Vec<String> {
        let mut all: Vec<String> = db
            .identity_list()
            .expect("凭据")
            .into_iter()
            .map(|item| item.name)
            .collect();
        all.sort();
        all
    }

    #[test]
    fn a_whole_password_lifecycle_through_the_real_database() {
        let db = Database::open_in_memory().expect("内存库");
        let key = db_identity(&db, "deploy key", IdentityKind::KeyFile, None);

        // 新建：填密码 → 一条「web 的密码」并关联。
        let mut form = input();
        form.password = "first";
        let created = save(&db, &form, None, None);
        let pw = created.identity_id.expect("关联了密码");
        assert_eq!(names(&db), ["deploy key", "web 的密码"]);
        assert_eq!(
            db.identity_saved_password(pw).expect("取").as_str(),
            "first"
        );

        // 只改名：密码身份跟着改名，密文还在。
        let mut form = input();
        form.name = "web2";
        let renamed = save(&db, &form, Some(created.id), None);
        assert_eq!(renamed.identity_id, Some(pw));
        assert_eq!(names(&db), ["deploy key", "web2 的密码"]);
        assert_eq!(
            db.identity_saved_password(pw).expect("取").as_str(),
            "first"
        );

        // 改密码：覆盖同一条，没有多出新的。
        form.password = "second";
        save(&db, &form, Some(created.id), None);
        assert_eq!(names(&db), ["deploy key", "web2 的密码"]);
        assert_eq!(
            db.identity_saved_password(pw).expect("取").as_str(),
            "second"
        );

        // 密码 → 私钥：独占的密码删掉，密钥还在。
        let mut form = input();
        form.name = "web2";
        form.auth = AuthMethod::Key;
        form.identity = Some(key.id);
        let keyed = save(&db, &form, Some(created.id), None);
        assert_eq!(keyed.identity_id, Some(key.id));
        assert_eq!(names(&db), ["deploy key"]);
        assert!(db.identity_saved_password(pw).is_err(), "密文跟着没了");

        // 私钥 → 密码：新建一条，密钥不动。
        let mut form = input();
        form.name = "web2";
        form.password = "third";
        let back = save(&db, &form, Some(created.id), None);
        assert_ne!(back.identity_id, Some(key.id));
        assert_eq!(names(&db), ["deploy key", "web2 的密码"]);
    }

    #[test]
    fn a_shared_legacy_password_survives_when_one_host_changes_its_password() {
        let db = Database::open_in_memory().expect("内存库");
        let shared = db_identity(&db, "共用密码", IdentityKind::Password, Some("old"));
        let draft = |name: &str| HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: Some(shared.id),
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        };
        let a = db.host_save(&draft("a")).expect("a");
        let b = db.host_save(&draft("b")).expect("b");

        // 不改密码只改别的：关联和旧密码原样。
        let mut form = input();
        form.name = "a";
        let same = save(&db, &form, Some(a.id), None);
        assert_eq!(same.identity_id, Some(shared.id));

        // a 改密码：a 拿到自己的新密码，b 还用旧的。
        form.password = "new for a";
        let changed = save(&db, &form, Some(a.id), None);
        let mine = changed.identity_id.expect("有密码");
        assert_ne!(mine, shared.id);
        assert_eq!(
            db.identity_saved_password(mine).expect("取").as_str(),
            "new for a"
        );
        assert_eq!(
            db.identity_saved_password(shared.id).expect("取").as_str(),
            "old",
            "共用的旧密码没被改"
        );
        assert_eq!(db.host_get(b.id).expect("b").identity_id, Some(shared.id));
        assert_eq!(names(&db), ["a 的密码", "共用密码"]);

        // b 改成私钥：它是共用密码最后一个用户（a 已经换了），所以现在算独占，一起删。
        let key = db_identity(
            &db,
            "k",
            IdentityKind::KeyInline,
            Some("placeholder-key-body"),
        );
        let mut form = input();
        form.name = "b";
        form.auth = AuthMethod::Key;
        form.identity = Some(key.id);
        save(&db, &form, Some(b.id), None);
        assert_eq!(names(&db), ["a 的密码", "k"]);
    }

    #[test]
    fn a_failed_host_save_leaves_no_orphan_password() {
        let db = Database::open_in_memory().expect("内存库");
        let mut form = input();
        form.password = "pw";
        // 主机草稿里的分组不存在：核心库拒绝保存。
        form.group = Some(9999);
        let plan = super::plan_save(&form, None, &[], &[], None).expect("合法");
        assert!(apply_save(&db, plan).is_err());
        assert!(names(&db).is_empty(), "刚建的密码要撤掉");
    }

    #[test]
    fn deleting_the_host_still_deletes_the_password_it_owns() {
        // 新规则下建出来的「<名称> 的密码」和删主机的现有规则对得上：独占就一起删，密钥不动。
        let db = Database::open_in_memory().expect("内存库");
        let key = db_identity(&db, "deploy key", IdentityKind::KeyFile, None);
        let mut form = input();
        form.password = "pw";
        let web = save(&db, &form, None, None);
        let mut keyed = input();
        keyed.name = "k-host";
        keyed.auth = AuthMethod::Key;
        keyed.identity = Some(key.id);
        let k = save(&db, &keyed, None, None);
        let deleted =
            super::super::host_delete::delete_host_with_passwords(&db, web.id).expect("删除");
        assert_eq!(deleted.passwords, ["web 的密码"]);
        super::super::host_delete::delete_host_with_passwords(&db, k.id).expect("删除");
        assert_eq!(names(&db), ["deploy key"]);
    }

    // ---- V2-N4：多协议 ----

    fn telnet_input<'a>() -> FormInput<'a> {
        let mut form = input();
        form.protocol = Protocol::Telnet;
        form.port = "23";
        form.username = "";
        form
    }

    fn serial_input<'a>() -> FormInput<'a> {
        let mut form = input();
        form.protocol = Protocol::Serial;
        form.address = "";
        form.port = "";
        form.username = "";
        form.serial.device = "COM3";
        form
    }

    #[test]
    fn a_new_plain_ssh_host_carries_no_protocol_change() {
        let plan = plan_save(&input(), None, &[], None).expect("合法");
        assert_eq!(plan.draft.protocol, None);
        assert!(!plan.draft.legacy_algorithms);
    }

    #[test]
    fn ssh_x11_switch_is_stored_only_for_ssh() {
        let mut form = input();
        form.x11 = true;
        let plan = plan_save(&form, None, &[], None).expect("合法");
        assert!(plan.draft.protocol.expect("要写").x11_forwarding);

        // 切到 Telnet 之后 X11 开关还留在表单里：保存时忽略。
        let mut telnet = telnet_input();
        telnet.x11 = true;
        let plan = plan_save(&telnet, None, &[], None).expect("合法");
        assert!(!plan.draft.protocol.expect("Telnet 要写").x11_forwarding);
    }

    #[test]
    fn an_unchanged_existing_host_does_not_rewrite_its_protocol_columns() {
        let existing = host(9);
        let mut form = input();
        form.legacy = true;
        let plan = plan_save(&form, Some(&existing), &[], None).expect("合法");
        assert_eq!(
            plan.draft.protocol, None,
            "没动协议设置就不带，免得洗掉不认识的选项键"
        );
        form.x11 = true;
        let plan = plan_save(&form, Some(&existing), &[], None).expect("合法");
        assert!(plan.draft.protocol.expect("开了 X11 要写").x11_forwarding);
    }

    #[test]
    fn telnet_needs_a_name_and_an_address_but_not_a_user() {
        let plan = plan_save(&telnet_input(), None, &[], None).expect("没填用户名也合法");
        assert_eq!(plan.draft.username, "");
        let settings = plan.draft.protocol.expect("Telnet 要写");
        assert_eq!(settings.protocol, Protocol::Telnet);
        assert!(settings.telnet.is_some());

        let mut form = telnet_input();
        form.address = " ";
        let err = plan_save(&form, None, &[], None).unwrap_err();
        assert_eq!(
            (err.field, err.message.as_str()),
            (FormField::Address, "名称、地址都不能为空")
        );
        let mut form = telnet_input();
        form.port = "99999";
        assert_eq!(
            plan_save(&form, None, &[], None).unwrap_err().field,
            FormField::Port
        );
    }

    #[test]
    fn telnet_ignores_a_leftover_key_login_and_a_key_identity() {
        let key = identity(5, "私钥", IdentityKind::KeyFile, false);
        let mut form = telnet_input();
        form.auth = AuthMethod::Key;
        form.identity = Some(key.id);
        let plan = plan_save(&form, None, std::slice::from_ref(&key), None).expect("合法");
        assert_eq!(plan.draft.auth_method, AuthMethod::Password);
        assert_eq!(
            plan.draft.identity_id, None,
            "私钥凭据对 Telnet 没有意义，保存时丢掉"
        );

        // Telnet 的密码同样归主机：填了就新建一条「<名称> 的密码」。
        form.password = "pw";
        let plan = plan_save(&form, None, &[key], None).expect("合法");
        assert!(matches!(
            plan.identity,
            IdentityAction::Create { draft } if draft.name == "web 的密码"
        ));
    }

    #[test]
    fn telnet_options_come_from_the_advanced_inputs() {
        let mut form = telnet_input();
        form.telnet.newline = TelnetNewline::Lf;
        form.telnet.local_echo = LocalEcho::On;
        form.telnet.naws = false;
        form.telnet.terminal_type = " vt100 ";
        form.telnet.encoding = TextEncoding::Gbk;
        form.telnet.auto_login = true;
        form.telnet.username_prompts = "login:，用户名:";
        form.telnet.raw_tcp = true;
        let options = plan_save(&form, None, &[], None)
            .expect("合法")
            .draft
            .protocol
            .and_then(|settings| settings.telnet)
            .expect("Telnet 选项");
        assert_eq!(options.newline, TelnetNewline::Lf);
        assert_eq!(options.local_echo, LocalEcho::On);
        assert!(!options.naws && options.raw_tcp);
        assert_eq!(options.terminal_type, "vt100");
        assert_eq!(options.encoding, TextEncoding::Gbk);
        assert!(options.auto_login.enabled);
        assert_eq!(
            options.auto_login.username_patterns,
            vec!["login:", "用户名:"]
        );
    }

    #[test]
    fn serial_needs_only_a_name_and_a_device_and_drops_the_network_fields() {
        let mut form = serial_input();
        form.address = "ignored";
        form.port = "2222";
        form.username = "ops";
        form.proxy_mode = ProxyMode::Custom;
        form.proxy = Some(3);
        form.jump = Some(4);
        form.identity = Some(7);
        form.password = "typed";
        let plan = plan_save(&form, None, &[], None).expect("合法");
        let draft = plan.draft;
        assert_eq!(
            (draft.host.as_str(), draft.port, draft.username.as_str()),
            ("", 0, "")
        );
        assert_eq!(draft.proxy_mode, ProxyMode::None);
        assert_eq!(
            (draft.proxy_id, draft.jump_host_id, draft.identity_id),
            (None, None, None)
        );
        assert_eq!(
            plan.identity,
            IdentityAction::None,
            "串口没有密码，不动保险库"
        );
        let serial = draft.protocol.and_then(|p| p.serial).expect("串口选项");
        assert_eq!(
            (serial.device.as_str(), serial.baud_rate),
            ("COM3", 115_200)
        );
    }

    #[test]
    fn serial_errors_land_on_the_device_and_the_baud_rate() {
        let mut form = serial_input();
        form.serial.device = "  ";
        assert_eq!(
            plan_save(&form, None, &[], None).unwrap_err().field,
            FormField::Device
        );
        let mut form = serial_input();
        form.serial.baud = "fast";
        let err = plan_save(&form, None, &[], None).unwrap_err();
        assert_eq!(err.field, FormField::Baud);
        form.name = "";
        assert_eq!(
            plan_save(&form, None, &[], None).unwrap_err().field,
            FormField::Name
        );
    }

    /// 经过真实的库保存：三种协议都能落库再读回来，核心库的校验错误能落到字段上。
    #[test]
    fn saving_each_protocol_through_the_real_database_round_trips() {
        use super::super::protocol_form::locate_error;
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");

        let mut telnet = telnet_input();
        telnet.name = "sw";
        telnet.telnet.auto_login = true;
        telnet.username = "admin";
        let saved = db
            .host_save(&plan_save(&telnet, None, &[], None).unwrap().draft)
            .expect("Telnet");
        assert_eq!(saved.protocol.protocol, Protocol::Telnet);
        assert!(saved.protocol.telnet.expect("选项").auto_login.enabled);

        let mut serial = serial_input();
        serial.name = "console";
        serial.serial.baud = "9600";
        let saved_serial = db
            .host_save(&plan_save(&serial, None, &[], None).unwrap().draft)
            .expect("串口");
        assert_eq!(saved_serial.protocol.serial.expect("选项").baud_rate, 9600);
        assert_eq!(saved_serial.host, "");

        // 核心库的校验：自动登录开了但没填用户名 → 落在自动登录上。
        let mut bad = telnet_input();
        bad.name = "sw2";
        bad.telnet.auto_login = true;
        let draft = plan_save(&bad, None, &[], None).unwrap().draft;
        let err = db.host_save(&draft).unwrap_err().to_string();
        assert_eq!(locate_error(&err), FormField::AutoLogin, "{err}");

        // 编辑已有的 Telnet 主机改成 SSH：允许；改成串口：核心库拒绝。
        let existing = db.host_get(saved.id).unwrap();
        let mut back = input();
        back.name = "sw";
        let plan = plan_save(&back, Some(&existing), &[], None).unwrap();
        let ssh = db.host_save(&plan.draft).expect("改回 SSH");
        assert!(ssh.protocol.is_plain_ssh());
        let mut to_serial = serial_input();
        to_serial.name = "sw";
        let plan = plan_save(&to_serial, Some(&ssh), &[], None).unwrap();
        assert!(db.host_save(&plan.draft).is_err());
    }

    // ---- V2-U13：连接方式三段 ----

    fn routed(jump: Option<i64>, mode: ProxyMode, proxy: Option<i64>) -> Host {
        let mut h = host(9);
        h.jump_host_id = jump;
        h.proxy_mode = mode;
        h.proxy_id = proxy;
        h
    }

    #[test]
    fn a_new_host_starts_on_the_proxy_segment_following_the_default_proxy() {
        assert_eq!(initial_conn_mode(None), ConnMode::Proxy);
        assert_eq!(initial_proxy_mode(None), ProxyMode::Default);
    }

    #[test]
    fn stored_data_maps_onto_the_three_segments() {
        use ProxyMode::{Custom, Default, None as NoProxy};
        let mode = |h: &Host| initial_conn_mode(Some(h));
        assert_eq!(mode(&routed(None, NoProxy, None)), ConnMode::Direct);
        assert_eq!(mode(&routed(None, Default, None)), ConnMode::Proxy);
        assert_eq!(mode(&routed(None, Custom, Some(3))), ConnMode::Proxy);
        // 设了跳板机：不管代理怎么设都是「SSH 跳板」。
        assert_eq!(mode(&routed(Some(5), NoProxy, None)), ConnMode::Jump);
        assert_eq!(mode(&routed(Some(5), Custom, Some(3))), ConnMode::Jump);
        assert_eq!(mode(&routed(Some(5), Default, None)), ConnMode::Jump);
    }

    #[test]
    fn only_old_hosts_with_both_a_jump_and_a_proxy_keep_the_proxy_under_the_jump_segment() {
        use ProxyMode::{Custom, Default, None as NoProxy};
        assert!(!jump_keeps_proxy(None));
        assert!(!jump_keeps_proxy(Some(&routed(Some(5), NoProxy, None))));
        assert!(!jump_keeps_proxy(Some(&routed(None, Custom, Some(3)))));
        assert!(jump_keeps_proxy(Some(&routed(Some(5), Custom, Some(3)))));
        assert!(jump_keeps_proxy(Some(&routed(Some(5), Default, None))));
    }

    #[test]
    fn saving_writes_the_fields_each_segment_means_and_never_loses_a_legacy_proxy() {
        use ProxyMode::{Custom, Default, None as NoProxy};
        // 直接连接：全清（也不留指向代理的引用）。
        assert_eq!(
            effective_routing(ConnMode::Direct, Custom, Some(3), Some(5), true),
            Routing {
                proxy_mode: NoProxy,
                proxy: None,
                jump: None
            }
        );
        // 代理访问：不用跳板机；跟随默认 / 指定代理原样；原来是「不用代理」落成跟随默认。
        assert_eq!(
            effective_routing(ConnMode::Proxy, Default, None, Some(5), false),
            Routing {
                proxy_mode: Default,
                proxy: None,
                jump: None
            }
        );
        assert_eq!(
            effective_routing(ConnMode::Proxy, Custom, Some(3), Some(5), false),
            Routing {
                proxy_mode: Custom,
                proxy: Some(3),
                jump: None
            }
        );
        assert_eq!(
            effective_routing(ConnMode::Proxy, NoProxy, None, None, false).proxy_mode,
            Default
        );
        // SSH 跳板：新选的跳板不带代理（跳板链里只有跳板机自己的代理生效）……
        assert_eq!(
            effective_routing(ConnMode::Jump, Default, None, Some(5), false),
            Routing {
                proxy_mode: NoProxy,
                proxy: None,
                jump: Some(5)
            }
        );
        // ……旧数据里两个都有的，打开再原样保存，一个字段都不丢。
        let both = routed(Some(5), Custom, Some(3));
        assert_eq!(
            effective_routing(
                initial_conn_mode(Some(&both)),
                both.proxy_mode,
                both.proxy_id,
                both.jump_host_id,
                jump_keeps_proxy(Some(&both)),
            ),
            Routing {
                proxy_mode: Custom,
                proxy: Some(3),
                jump: Some(5)
            }
        );
    }

    #[test]
    fn an_untouched_form_saves_every_kind_of_stored_routing_unchanged() {
        use ProxyMode::{Custom, Default, None as NoProxy};
        for h in [
            routed(None, NoProxy, None),
            routed(None, Default, None),
            routed(None, Custom, Some(3)),
            routed(Some(5), NoProxy, None),
            routed(Some(5), Custom, Some(3)),
        ] {
            let saved = effective_routing(
                initial_conn_mode(Some(&h)),
                h.proxy_mode,
                h.proxy_id,
                h.jump_host_id,
                jump_keeps_proxy(Some(&h)),
            );
            assert_eq!(
                saved,
                Routing {
                    proxy_mode: h.proxy_mode,
                    proxy: h.proxy_id,
                    jump: h.jump_host_id
                },
                "{:?}",
                (h.proxy_mode, h.proxy_id, h.jump_host_id)
            );
        }
    }

    #[test]
    fn segment_labels_and_order_follow_the_reference_form() {
        let labels: Vec<&str> = ConnMode::ALL.iter().map(|m| m.label()).collect();
        assert_eq!(labels, ["直接连接", "SSH 跳板", "代理访问"]);
    }
}
