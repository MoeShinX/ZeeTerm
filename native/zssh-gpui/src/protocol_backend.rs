//! Telnet / 串口主机的会话入口：按主机库里的设置把一台主机变成一个能放进终端标签的会话。
//!
//! 协议引擎在 `zssh-protocols`；这里只管"怎么拨号、用谁的设置"：
//! - Telnet：直连、经代理（SOCKS5 / HTTP CONNECT，沿用 SSH 的拨号器）、经 SSH 跳板机
//!   （在跳板机的已认证连接上开 `direct-tcpip` 通道，跳板机连接和 SSH 标签共用连接池）；
//! - 串口：在阻塞线程里打开本机串口（打开可能卡在有毛病的驱动上，不能占着运行时线程）。
//!
//! 入口是 [`crate::ssh_backend::begin_connect`]：它先看主机的协议再分流到这里，所以新标签、
//! 重新连接、复制标签对三种协议都是同一条路。

use std::sync::Arc;

use zssh_core::model::{Protocol, SerialOptions, TelnetOptions};
use zssh_core::ssh::latency::{LatencyMonitor, TcpProbe, Transport, probe_kind_for};
use zssh_core::ssh::{BoxedStream, ProxyConfig, connect_direct, dialer::connect_via_proxy};
use zssh_core::store::Database;
use zssh_protocols::serial::open_serial;
use zssh_protocols::telnet::{TelnetParams, spawn_telnet};
use zssh_terminal::Size;

use crate::settings::data_dir;
use crate::ssh_backend::{OpenResult, Opened, SshSessionFactory, factory_for_host, resolve_proxy};

/// 一台主机该怎么打开。
pub(crate) enum HostPlan {
    Ssh,
    Telnet(Box<TelnetPlan>),
    Serial(SerialOptions),
}

/// 怎么到达 Telnet 目标。
pub(crate) enum Route {
    Direct,
    Proxy(ProxyConfig),
    /// 经这台 SSH 主机（库里的 id）的 `direct-tcpip`。设了跳板机时目标自己的代理设置不再用：
    /// 跳板机那条链有它自己的代理。
    Jump(i64),
}

pub(crate) struct TelnetPlan {
    pub host: String,
    pub port: u16,
    pub options: TelnetOptions,
    pub username: String,
    /// 主机绑定的密码凭据里的密码（自动登录用）。
    pub password: Option<String>,
    pub route: Route,
}

/// `Debug` 手写：密码不能进日志。
impl std::fmt::Debug for TelnetPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelnetPlan")
            .field("host", &self.host)
            .field("port", &self.port)
            .field(
                "password",
                &self
                    .password
                    .as_ref()
                    .map(|_| crate::i18n::t(ProtocolMsg::HiddenSecret)),
            )
            .finish_non_exhaustive()
    }
}

/// 协议会话（串口 / Telnet）自己的文案：错误提示都是用户能看到的。
#[derive(Clone, Copy, Debug)]
enum ProtocolMsg {
    DbOpenFailedPrefix,
    NoSerialSettings,
    NoSerialDevice,
    DirectConnectPrefix,
    ConnectFailSuffix,
    ProxyConnectPrefix,
    JumpConnectPrefix,
    SerialTaskEnded,
    HiddenSecret,
    MissingAddress,
    BadAddressPort,
    BadPortPrefix,
    BadBaudPrefix,
}

impl crate::i18n::Msg for ProtocolMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::DbOpenFailedPrefix => "打开主机库失败：",
            Self::NoSerialSettings => "这台串口主机没有串口设置",
            Self::NoSerialDevice => "这台串口主机还没有选择串口设备",
            Self::DirectConnectPrefix => "连接 ",
            Self::ConnectFailSuffix => " 失败：",
            Self::ProxyConnectPrefix => "经代理连接 ",
            Self::JumpConnectPrefix => "经跳板机连接 ",
            Self::SerialTaskEnded => "打开串口的任务意外结束",
            Self::HiddenSecret => "<已隐藏>",
            Self::MissingAddress => "缺少地址 / 设备名",
            Self::BadAddressPort => "要写成 <地址>:<端口>",
            Self::BadPortPrefix => "端口不对：",
            Self::BadBaudPrefix => "波特率不对：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::DbOpenFailedPrefix => "Failed to open the host library: ",
            Self::NoSerialSettings => "This serial host has no serial settings",
            Self::NoSerialDevice => "No serial device has been selected for this host",
            Self::DirectConnectPrefix => "Failed to connect to ",
            Self::ConnectFailSuffix => ": ",
            Self::ProxyConnectPrefix => "Failed to connect through the proxy to ",
            Self::JumpConnectPrefix => "Failed to connect through the jump host to ",
            Self::SerialTaskEnded => "The serial port task ended unexpectedly",
            Self::HiddenSecret => "<hidden>",
            Self::MissingAddress => "missing address / device name",
            Self::BadAddressPort => "expected <address>:<port>",
            Self::BadPortPrefix => "invalid port: ",
            Self::BadBaudPrefix => "invalid baud rate: ",
        }
    }
}

/// 读主机库，决定一台主机的打开方式。数据库句柄不是 `Send`，查询在这个同步函数里做完。
pub(crate) fn plan_for_host(host_id: i64) -> Result<HostPlan, String> {
    let db = Database::open(&data_dir().join("zssh.db"))
        .map_err(|err| format!("{}{err}", crate::i18n::t(ProtocolMsg::DbOpenFailedPrefix)))?;
    plan_from_db(&db, host_id)
}

pub(crate) fn plan_from_db(db: &Database, host_id: i64) -> Result<HostPlan, String> {
    let settings = db
        .host_protocol_get(host_id)
        .map_err(|err| crate::error_text::error_text(&err))?;
    match settings.protocol {
        Protocol::Ssh => Ok(HostPlan::Ssh),
        Protocol::Serial => {
            let options = settings
                .serial
                .ok_or_else(|| crate::i18n::t(ProtocolMsg::NoSerialSettings).to_owned())?;
            if options.device.trim().is_empty() {
                return Err(crate::i18n::t(ProtocolMsg::NoSerialDevice).to_owned());
            }
            Ok(HostPlan::Serial(options))
        }
        Protocol::Telnet => {
            let options = settings.telnet.unwrap_or_default();
            let host = db
                .host_get(host_id)
                .map_err(|err| crate::error_text::error_text(&err))?;
            // 跳板机链（含成环、非 SSH 跳板机）在这里一并校验，连接时才不会报莫名其妙的错。
            db.host_chain(host_id)
                .map_err(|err| crate::error_text::error_text(&err))?;
            let login = db
                .host_login(host_id)
                .map_err(|err| crate::error_text::error_text(&err))?;
            let route = match host.jump_host_id {
                Some(jump_id) => Route::Jump(jump_id),
                None => {
                    let default_proxy = db
                        .default_proxy_id()
                        .map_err(|err| crate::error_text::error_text(&err))?;
                    match resolve_proxy(db, &host, default_proxy)? {
                        Some(proxy) => Route::Proxy(proxy),
                        None => Route::Direct,
                    }
                }
            };
            Ok(HostPlan::Telnet(Box::new(TelnetPlan {
                host: login.host,
                port: login.port,
                options,
                username: login.username,
                password: login.auth.password,
                route,
            })))
        }
    }
}

/// Telnet 标签要不要测延迟。默认关：TCP connect 计时每 10 秒对设备多开一条连接，交换机 / 路由器
/// 往往把每次连接记进日志或占用会话名额（2026-11-03 定）。延迟徽标只给 SSH。
const TELNET_LATENCY_ENABLED: bool = false;

/// Telnet 的延迟监视器：Telnet 没有协议级往返，只能对同一地址端口单独做 TCP connect 计时；
/// 经代理时只测得到代理（徽标提示里写明）；经 SSH 跳板机测不到"到目标"那一段，诚实起见不测、不显示。
/// 要在 tokio 运行时里调用（后台任务由它起）。
pub(crate) fn telnet_latency(route: &Route, host: &str, port: u16) -> Option<Arc<LatencyMonitor>> {
    let (transport, probe) = match route {
        Route::Direct => (Transport::TelnetDirect, TcpProbe::new(host, port)),
        Route::Proxy(proxy) => (
            Transport::TelnetProxy,
            TcpProbe::new(proxy.host.clone(), proxy.port),
        ),
        Route::Jump(_) => return None,
    };
    let monitor = LatencyMonitor::new(probe_kind_for(transport)?);
    monitor.spawn(probe);
    Some(monitor)
}

/// 连接 Telnet 目标并开始会话。
pub(crate) async fn open_telnet(plan: TelnetPlan, size: Size, generation: u64) -> OpenResult {
    let TelnetPlan {
        host,
        port,
        options,
        username,
        password,
        route,
    } = plan;
    let latency = if TELNET_LATENCY_ENABLED {
        telnet_latency(&route, &host, port)
    } else {
        None
    };
    let (stream, guard): (BoxedStream, Box<dyn Send>) = match route {
        Route::Direct => {
            let socket = connect_direct(&host, port).await.map_err(|err| {
                format!(
                    "{}{host}:{port}{}{err}",
                    crate::i18n::t(ProtocolMsg::DirectConnectPrefix),
                    crate::i18n::t(ProtocolMsg::ConnectFailSuffix)
                )
            })?;
            (Box::new(socket), Box::new(()))
        }
        Route::Proxy(proxy) => {
            let stream = connect_via_proxy(&proxy, &host, port)
                .await
                .map_err(|err| {
                    format!(
                        "{}{host}:{port}{}{err}",
                        crate::i18n::t(ProtocolMsg::ProxyConnectPrefix),
                        crate::i18n::t(ProtocolMsg::ConnectFailSuffix)
                    )
                })?;
            (stream, Box::new(()))
        }
        Route::Jump(jump_id) => {
            let factory = factory_for_host(jump_id).await?;
            open_via_jump(&factory, &host, port).await?
        }
    };
    let mut opened = start_telnet(stream, guard, options, username, password, size, generation);
    opened.latency = latency;
    Ok(opened)
}

/// 在跳板机的已认证连接上开 `direct-tcpip` 通道到目标。返回的守卫要和会话一起活着：
/// 它持有跳板机连接，会话在用时连接不会被释放。
pub(crate) async fn open_via_jump(
    factory: &Arc<SshSessionFactory>,
    host: &str,
    port: u16,
) -> Result<(BoxedStream, Box<dyn Send>), String> {
    let originator = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    let channel = factory
        .connection()
        .open_forward_channel(host, port, originator)
        .await
        .map_err(|err| {
            format!(
                "{}{host}:{port}{}{err}",
                crate::i18n::t(ProtocolMsg::JumpConnectPrefix),
                crate::i18n::t(ProtocolMsg::ConnectFailSuffix)
            )
        })?;
    Ok((Box::new(channel), Box::new(Arc::clone(factory))))
}

pub(crate) fn start_telnet(
    stream: BoxedStream,
    guard: Box<dyn Send>,
    options: TelnetOptions,
    username: String,
    password: Option<String>,
    size: Size,
    generation: u64,
) -> Opened {
    let cols = u16::try_from(size.cols).unwrap_or(u16::MAX);
    let rows = u16::try_from(size.rows).unwrap_or(u16::MAX);
    let params = TelnetParams::new(options, username, password, cols, rows);
    let (backend, events, gate) = spawn_telnet(stream, params, generation, guard);
    Opened {
        factory: None,
        backend: Box::new(backend),
        events,
        gate,
        latency: None,
    }
}

/// 打开串口主机的会话。串口没有窗口大小，`size` 不用。
pub(crate) async fn open_serial_host(
    options: SerialOptions,
    _size: Size,
    generation: u64,
) -> OpenResult {
    let runtime = tokio::runtime::Handle::current();
    let opened = tokio::task::spawn_blocking(move || open_serial(&options, generation, runtime))
        .await
        .map_err(|_| crate::i18n::t(ProtocolMsg::SerialTaskEnded).to_owned())?
        .map_err(|err| err.to_string())?;
    let (backend, events, gate) = opened;
    Ok(Opened {
        factory: None,
        backend: Box::new(backend),
        events,
        gate,
        latency: None,
    })
}

// ---------------------------------------------------------------- 调试钩子

/// 手动验证用（只在调试版有效，见 `dev_hooks`）：按环境变量往主机库里建一台 Telnet / 串口主机，
/// 返回要在主窗口里打开的主机 id。
///
/// - `ZSSH_OPEN_TELNET=<地址>:<端口>[,raw]`：Telnet 主机，`raw` 表示纯 TCP 模式；
/// - `ZSSH_OPEN_SERIAL=<设备名>[,<波特率>]`：串口主机，例如 `COM3,115200`。
///
/// 同名主机已存在就更新它，不会越建越多。
pub(crate) fn dev_hook_hosts() -> Vec<i64> {
    let mut ids = Vec::new();
    if let Some(spec) = crate::dev_hooks::var("ZSSH_OPEN_TELNET") {
        match seed_dev_host(&spec, true) {
            Ok(id) => ids.push(id),
            Err(err) => {
                tracing::warn!("ZSSH_OPEN_TELNET 无效：{err}");
            }
        }
    }
    if let Some(spec) = crate::dev_hooks::var("ZSSH_OPEN_SERIAL") {
        match seed_dev_host(&spec, false) {
            Ok(id) => ids.push(id),
            Err(err) => {
                tracing::warn!("ZSSH_OPEN_SERIAL 无效：{err}");
            }
        }
    }
    ids
}

fn seed_dev_host(spec: &str, telnet: bool) -> Result<i64, String> {
    let db = Database::open(&data_dir().join("zssh.db"))
        .map_err(|err| format!("{}{err}", crate::i18n::t(ProtocolMsg::DbOpenFailedPrefix)))?;
    let (draft, settings) = dev_host_from_spec(spec, telnet)?;
    let mut draft = draft;
    draft.id = db
        .host_list()
        .map_err(|err| crate::error_text::error_text(&err))?
        .into_iter()
        .find(|host| host.name == draft.name)
        .map(|host| host.id);
    let (host, _) = db
        .host_save_with_protocol(&draft, &settings)
        .map_err(|err| crate::error_text::error_text(&err))?;
    Ok(host.id)
}

/// 钩子字符串 → 主机草稿和协议设置（纯函数，单测直接用）。
fn dev_host_from_spec(
    spec: &str,
    telnet: bool,
) -> Result<
    (
        zssh_core::model::HostDraft,
        zssh_core::model::ProtocolSettings,
    ),
    String,
> {
    use zssh_core::model::{AuthMethod, HostDraft, ProtocolSettings, ProxyMode};
    let mut parts = spec.split(',').map(str::trim);
    let first = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| crate::i18n::t(ProtocolMsg::MissingAddress))?;
    let mut draft = HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: String::new(),
        host: String::new(),
        port: 23,
        username: String::new(),
        auth_method: AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::None,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    };
    if telnet {
        let (host, port) = first
            .rsplit_once(':')
            .ok_or_else(|| crate::i18n::t(ProtocolMsg::BadAddressPort))?;
        draft.host = host.to_owned();
        draft.port = port
            .parse()
            .map_err(|_| format!("{}{port}", crate::i18n::t(ProtocolMsg::BadPortPrefix)))?;
        draft.name = format!("dev-telnet {host}:{port}");
        let mut settings = ProtocolSettings::new(Protocol::Telnet);
        if let Some(options) = settings.telnet.as_mut() {
            options.raw_tcp = parts.any(|part| part.eq_ignore_ascii_case("raw"));
        }
        Ok((draft, settings))
    } else {
        draft.name = format!("dev-serial {first}");
        let mut settings = ProtocolSettings::new(Protocol::Serial);
        if let Some(options) = settings.serial.as_mut() {
            options.device = first.to_owned();
            if let Some(baud) = parts.next() {
                options.baud_rate = baud
                    .parse()
                    .map_err(|_| format!("{}{baud}", crate::i18n::t(ProtocolMsg::BadBaudPrefix)))?;
            }
        }
        Ok((draft, settings))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use zssh_core::model::{
        AuthMethod, HostDraft, IdentityDraft, IdentityKind, ProtocolSettings, ProxyMode,
        SerialDataBits,
    };
    use zssh_core::ssh::{
        AuthConfig, BoxFuture, ChainHop, Connection, HostKeyDecision, HostKeyInfo, Interactor,
        KnownHosts, PromptAnswers, PromptRequest,
    };
    use zssh_session::{SessionCommand, SessionEvent};

    use super::*;

    fn memory_db() -> Database {
        Database::open_in_memory().expect("内存库")
    }

    fn draft(name: &str) -> HostDraft {
        HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "device.internal".to_owned(),
            port: 23,
            username: "admin".to_owned(),
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

    fn plan_text(plan: &HostPlan) -> &'static str {
        match plan {
            HostPlan::Ssh => "ssh",
            HostPlan::Telnet(_) => "telnet",
            HostPlan::Serial(_) => "serial",
        }
    }

    #[test]
    fn hosts_are_dispatched_by_their_protocol() {
        let db = memory_db();
        let ssh = db.host_save(&draft("web-1")).unwrap();
        assert_eq!(plan_text(&plan_from_db(&db, ssh.id).unwrap()), "ssh");

        let identity = password_identity(&db);
        let mut request = draft("switch-1");
        request.identity_id = Some(identity);
        let mut settings = ProtocolSettings::new(Protocol::Telnet);
        settings.telnet.as_mut().unwrap().auto_login.enabled = true;
        let (telnet, _) = db.host_save_with_protocol(&request, &settings).unwrap();
        let HostPlan::Telnet(plan) = plan_from_db(&db, telnet.id).unwrap() else {
            panic!("应当是 Telnet");
        };
        assert_eq!((plan.host.as_str(), plan.port), ("device.internal", 23));
        assert_eq!(plan.username, "admin");
        assert_eq!(plan.password.as_deref(), Some("s3cret"));
        assert!(plan.options.auto_login.enabled);
        assert!(matches!(plan.route, Route::Direct));
        // 密码不能出现在 Debug 输出里。
        assert!(!format!("{plan:?}").contains("s3cret"));

        let mut serial = ProtocolSettings::new(Protocol::Serial);
        serial.serial.as_mut().unwrap().device = "COM7".to_owned();
        serial.serial.as_mut().unwrap().data_bits = SerialDataBits::Seven;
        let (port, _) = db
            .host_save_with_protocol(&draft("console"), &serial)
            .unwrap();
        let HostPlan::Serial(options) = plan_from_db(&db, port.id).unwrap() else {
            panic!("应当是串口");
        };
        assert_eq!(options.device, "COM7");
        assert_eq!(options.data_bits, SerialDataBits::Seven);
    }

    #[test]
    fn dev_hook_specs_become_hosts_that_pass_validation() {
        let db = memory_db();
        let (draft, settings) = dev_host_from_spec("127.0.0.1:2323,raw", true).unwrap();
        assert!(settings.telnet.as_ref().unwrap().raw_tcp);
        let (host, _) = db.host_save_with_protocol(&draft, &settings).unwrap();
        assert_eq!(host.port, 2323);

        let (draft, settings) = dev_host_from_spec("COM3,9600", false).unwrap();
        assert_eq!(settings.serial.as_ref().unwrap().baud_rate, 9600);
        let (serial, _) = db.host_save_with_protocol(&draft, &settings).unwrap();
        let HostPlan::Serial(options) = plan_from_db(&db, serial.id).unwrap() else {
            panic!("应当是串口");
        };
        assert_eq!(options.device, "COM3");

        assert!(dev_host_from_spec("no-port", true).is_err());
        assert!(dev_host_from_spec("COM3,fast", false).is_err());
        assert!(dev_host_from_spec("", false).is_err());
    }

    #[test]
    fn a_telnet_host_behind_a_jump_host_routes_through_it() {
        let db = memory_db();
        let mut jump = draft("bastion");
        jump.port = 22;
        let jump = db.host_save(&jump).unwrap();
        let mut request = draft("switch-2");
        request.jump_host_id = Some(jump.id);
        let (telnet, _) = db
            .host_save_with_protocol(&request, &ProtocolSettings::new(Protocol::Telnet))
            .unwrap();
        let HostPlan::Telnet(plan) = plan_from_db(&db, telnet.id).unwrap() else {
            panic!("应当是 Telnet");
        };
        assert!(matches!(plan.route, Route::Jump(id) if id == jump.id));
    }

    #[test]
    fn ssh_only_entry_points_refuse_non_ssh_hosts() {
        // SFTP 窗口直接要 SSH 连接：对 Telnet / 串口主机要给清楚的错，不是去连 23 端口当 SSH。
        let db = memory_db();
        let (telnet, _) = db
            .host_save_with_protocol(&draft("switch-3"), &ProtocolSettings::new(Protocol::Telnet))
            .unwrap();
        let error = crate::ssh_backend::prepare_from_db(&db, telnet.id)
            .err()
            .unwrap_or_else(|| panic!("Telnet 主机没有 SSH 连接"));
        assert!(
            error.contains("Telnet") && error.contains("没有 SSH 连接"),
            "{error}"
        );
        let ssh = db.host_save(&draft("web-2")).unwrap();
        assert!(crate::ssh_backend::prepare_from_db(&db, ssh.id).is_ok());
    }

    struct TrustOnce;

    impl Interactor for TrustOnce {
        fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::TrustOnce })
        }

        fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
            Box::pin(async { None })
        }
    }

    /// 一台只会说"JUMP_OK"、回显收到的行的假 Telnet 服务器（本机回环）。
    async fn spawn_telnet_server() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let _ = socket.write_all(b"JUMP_OK\r\n").await;
            let mut buffer = [0u8; 256];
            while let Ok(read) = socket.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let _ = tx.send(buffer[..read].to_vec());
            }
        });
        (addr, rx)
    }

    /// 经进程内的 SSH 跳板机（`direct-tcpip`）连到本机的假 Telnet 服务器：
    /// 横幅到达、键入送达、关闭后目标收到 EOF。
    #[test]
    fn telnet_over_an_ssh_jump_host_connects_exchanges_data_and_closes() {
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let (factory, telnet_addr, mut received) = runtime.block_on(async {
            let (telnet_addr, received) = spawn_telnet_server().await;
            let jump_addr = crate::test_ssh_server::spawn_forwarding().await;
            let known_hosts = std::env::temp_dir().join(format!(
                "zssh-n1-jump-kh-{}-{}",
                std::process::id(),
                jump_addr.port()
            ));
            let hop = ChainHop {
                label: "本机跳板机".to_owned(),
                host: jump_addr.ip().to_string(),
                port: jump_addr.port(),
                username: "tester".to_owned(),
                proxy: None,
                auth: AuthConfig::password(Some("pw".to_owned())),
                known_hosts: KnownHosts::new(&known_hosts),
                legacy_algorithms: false,
            };
            let connection = Connection::connect_chain(vec![hop], Arc::new(TrustOnce))
                .await
                .expect("连接本机跳板机");
            let _ = std::fs::remove_file(&known_hosts);
            (
                SshSessionFactory::from_connection(connection),
                telnet_addr,
                received,
            )
        });

        let mut opened = runtime
            .block_on(async {
                let (stream, guard) =
                    open_via_jump(&factory, &telnet_addr.ip().to_string(), telnet_addr.port())
                        .await?;
                Ok::<_, String>(start_telnet(
                    stream,
                    guard,
                    TelnetOptions::default(),
                    String::new(),
                    None,
                    Size { rows: 24, cols: 80 },
                    1,
                ))
            })
            .expect("经跳板机打开 Telnet");
        assert!(opened.factory.is_none());

        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !text.contains("JUMP_OK") {
            let left = deadline.saturating_duration_since(Instant::now());
            match opened.events.recv_timeout(left).expect("等横幅超时") {
                SessionEvent::Data { bytes, .. } => {
                    opened.gate.on_ack(bytes.len());
                    text.push_str(&String::from_utf8_lossy(&bytes));
                }
                SessionEvent::Notice { text: notice, .. } => text.push_str(&notice),
                other => panic!("意外事件 {other:?}，已收到 {text:?}"),
            }
        }
        assert!(text.contains("Telnet 不加密"), "要有明文提示：{text:?}");

        opened
            .backend
            .send(SessionCommand::Write(b"ping\r".to_vec()));
        let mut at_server = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&at_server).contains("ping\r\n") {
            let left = deadline.saturating_duration_since(Instant::now());
            let chunk = runtime
                .block_on(async { tokio::time::timeout(left, received.recv()).await })
                .expect("等目标收到超时")
                .expect("目标侧通道关闭");
            at_server.extend_from_slice(&chunk);
        }

        opened.backend.send(SessionCommand::Close);
        let closed = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while received.recv().await.is_some() {}
            })
            .await
        });
        assert!(closed.is_ok(), "关闭之后目标应当看到连接断开");
    }

    /// Telnet 的延迟：直连测目标、经代理只测代理、经跳板机不测；真连本机监听时看得见就测到第一个值。
    #[test]
    fn telnet_latency_follows_the_route_and_measures_a_real_connect() {
        use zssh_core::ssh::latency::{ProbeKind, Sample};
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let probe_port = listener.local_addr().unwrap().port();
            tokio::spawn(async move { while listener.accept().await.is_ok() {} });

            assert!(telnet_latency(&Route::Jump(1), "device.internal", 23).is_none());
            let proxy = ProxyConfig {
                kind: zssh_core::model::ProxyKind::Socks5,
                host: "127.0.0.1".to_owned(),
                port: probe_port,
                username: None,
                password: None,
            };
            let via_proxy = telnet_latency(&Route::Proxy(proxy), "device.internal", 23).unwrap();
            assert_eq!(via_proxy.kind(), ProbeKind::Tcp { via_proxy: true });

            // 直连：测的是目标自己的端口（这里是一个一直 accept 的本机监听）。
            let direct = telnet_latency(&Route::Direct, "127.0.0.1", probe_port).unwrap();
            assert_eq!(direct.kind(), ProbeKind::Tcp { via_proxy: false });
            let mut viewer = direct.viewer();
            assert!(direct.snapshot().stats.is_empty(), "没人看之前不测");
            viewer.set_visible(true);
            let deadline = Instant::now() + Duration::from_secs(5);
            while direct.snapshot().stats.is_empty() {
                assert!(Instant::now() < deadline, "看得见之后应当很快测到第一个值");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(matches!(
                direct.snapshot().stats.last(),
                Some(Sample::Rtt(_))
            ));
        });
    }

    /// 目标连不上时，跳板机给的错误要带上目标地址。
    #[test]
    fn a_refused_target_behind_the_jump_host_gives_a_clear_error() {
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let outcome = runtime.block_on(async {
            let jump_addr = crate::test_ssh_server::spawn_forwarding().await;
            let known_hosts = std::env::temp_dir().join(format!(
                "zssh-n1-jump2-kh-{}-{}",
                std::process::id(),
                jump_addr.port()
            ));
            let hop = ChainHop {
                label: "本机跳板机".to_owned(),
                host: jump_addr.ip().to_string(),
                port: jump_addr.port(),
                username: "tester".to_owned(),
                proxy: None,
                auth: AuthConfig::password(Some("pw".to_owned())),
                known_hosts: KnownHosts::new(&known_hosts),
                legacy_algorithms: false,
            };
            let connection = Connection::connect_chain(vec![hop], Arc::new(TrustOnce))
                .await
                .expect("连接本机跳板机");
            let _ = std::fs::remove_file(&known_hosts);
            let factory = SshSessionFactory::from_connection(connection);
            // 没有人监听的本机端口。
            let free = {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                listener.local_addr().unwrap().port()
            };
            open_via_jump(&factory, "127.0.0.1", free).await
        });
        let error = outcome.err().expect("目标没人监听应当失败");
        assert!(error.contains("经跳板机连接 127.0.0.1:"), "{error}");
    }
}
