//! Tokenway 一键连接在 `agent_setup` 里的那一份（V2-A5）：现在生效的预设清单、连接 / 断开的流程拼装、
//! 余额显示的状态与刷新节流、"重新连接"请求、调试版的本机假 Tokenway 钩子。
//!
//! 业务（验签、白名单、PKCE、回环监听、换令牌、余额解析、错误分类）全是 core 的
//! （`zssh_core::ai::agent::builtin::tokenway`），这里只拼装；界面在 `main_window/ai_presets.rs`
//! 和 AI 面板（`agent_chat_demo`）。
//!
//! 不加任何使用统计 / 遥测：只请求预设清单、授权相关接口、余额状态、模型与对话接口。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use zssh_core::ai::agent::builtin::config::AiEndpoint;
use zssh_core::ai::agent::builtin::probe::{MODELS_TIMEOUT, fetch_models};
use zssh_core::ai::agent::builtin::provider::{ProviderConfig, ProviderKind, Transport};
use zssh_core::ai::agent::builtin::tokenway::api::fetch_status;
use zssh_core::ai::agent::builtin::tokenway::connect::{AuthorizeParams, aff_from_register_url};
use zssh_core::ai::agent::builtin::tokenway::endpoint::{
    DisconnectReport, disconnect, is_authorized_token, resolve_base_url, save_authorized,
};
use zssh_core::ai::agent::builtin::tokenway::fake::{FakeBrowser, FakeTokenway};
use zssh_core::ai::agent::builtin::tokenway::presets::now_unix;
use zssh_core::ai::agent::builtin::tokenway::{
    ActivePresets, ApiError, ApiErrorKind, Authorized, BALANCE_MIN_INTERVAL, Balance, ConnectError,
    PRESETS_URL, Preset, PresetStore, RefreshGate, RefreshOutcome, Stage, UrlOpener, UrlPolicy,
    authorize, production_key,
};
use zssh_core::store::Database;

use super::builtin;

/// 预设清单的缓存放在数据目录下的这个子目录。
pub const PRESETS_DIR: &str = "ai-presets";

// ---------------------------------------------------------------------------
// 调试版的本机假 Tokenway
// ---------------------------------------------------------------------------

/// 调试版里设了 `ZSSH_TOKENWAY_FAKE` 才有；release 里恒为 `None`（`dev_hooks` 只在 debug 构建里读环境变量）。
pub struct Fake {
    pub server: FakeTokenway,
    pub browser: FakeBrowser,
    /// "打开浏览器"但没真的打开的地址（注册页、充值页）。
    pub opened: Mutex<Vec<String>>,
}

pub fn fake() -> Option<&'static Fake> {
    crate::dev_hooks::var("ZSSH_TOKENWAY_FAKE")?;
    static FAKE: OnceLock<Option<Fake>> = OnceLock::new();
    FAKE.get_or_init(|| {
        let runtime = crate::ssh_backend::runtime().ok()?;
        let server = runtime.block_on(FakeTokenway::start()).ok()?;
        Some(Fake {
            server,
            browser: FakeBrowser::new(),
            opened: Mutex::new(Vec::new()),
        })
    })
    .as_ref()
}

/// 假 Tokenway 在跑：传输层直连本机（不走代理），地址白名单放行本机回环的 http。
pub fn fake_active() -> bool {
    fake().is_some()
}

// ---------------------------------------------------------------------------
// 预设清单
// ---------------------------------------------------------------------------

static PRESETS: Mutex<Option<ActivePresets>> = Mutex::new(None);

fn presets_lock() -> MutexGuard<'static, Option<ActivePresets>> {
    PRESETS.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn presets_dir() -> PathBuf {
    crate::settings::data_dir().join(PRESETS_DIR)
}

/// 地址规则：产品是"只有 https + 白名单域名"；调试版的假 Tokenway 另外放行本机回环。
pub fn policy() -> UrlPolicy {
    if fake_active() {
        UrlPolicy::with_http_loopback_for_tests()
    } else {
        UrlPolicy::production()
    }
}

/// 现在生效的清单：通过验证的缓存 > 内置。第一次用到时加载。
pub fn active() -> ActivePresets {
    let mut slot = presets_lock();
    if let Some(active) = slot.as_ref() {
        return active.clone();
    }
    let loaded = PresetStore::new(presets_dir()).load(
        production_key().as_ref(),
        &UrlPolicy::production(),
        now_unix(),
    );
    *slot = Some(loaded.clone());
    loaded
}

/// 缓存文件变了（拉到新清单）之后重新加载。
pub fn reload_presets() {
    *presets_lock() = None;
    builtin::bump_version();
}

/// Tokenway 预设：假 Tokenway 在跑时用它的，否则取生效清单里的。
pub fn tokenway_preset() -> Option<Preset> {
    if let Some(fake) = fake() {
        return Some(fake.server.preset());
    }
    active().manifest.tokenway().cloned()
}

/// 拉一次清单（到点才拉；`force` 不看间隔）。走"软件网络"代理。公钥还是占位时什么请求都不发。
pub async fn refresh_presets(force: bool) -> RefreshOutcome {
    if fake_active() || builtin_fake_active() {
        return RefreshOutcome::NoPublicKey;
    }
    let transport = match builtin::make_transport() {
        Ok(transport) => transport,
        Err(reason) => return RefreshOutcome::Failed(reason),
    };
    PresetStore::new(presets_dir())
        .refresh(
            transport.as_ref(),
            PRESETS_URL,
            production_key().as_ref(),
            &UrlPolicy::production(),
            now_unix(),
            force,
        )
        .await
}

fn builtin_fake_active() -> bool {
    super::builtin_fake::transport().is_some()
}

/// 后台每 24 小时拉一次清单：启动后延迟执行，之后每 6 小时看一次是不是到点了（真正的间隔由
/// core 的 `PresetStore` 把关）。失败只记日志，继续用现有的。
pub fn spawn_background_refresh(delay: Duration) {
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        return;
    };
    runtime.spawn(async move {
        tokio::time::sleep(delay).await;
        loop {
            match refresh_presets(false).await {
                RefreshOutcome::Updated { issued_at } => {
                    tracing::info!(%issued_at, "Tokenway 预设清单已更新");
                    reload_presets();
                }
                RefreshOutcome::Rejected(reason) => {
                    tracing::warn!(%reason, "拉到的 Tokenway 预设清单没通过验证，已丢弃");
                }
                RefreshOutcome::Failed(reason) => {
                    tracing::info!(%reason, "Tokenway 预设清单这次没拉到，继续用现有的");
                }
                RefreshOutcome::NoPublicKey
                | RefreshOutcome::NotDue
                | RefreshOutcome::Unchanged => {}
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

// ---------------------------------------------------------------------------
// 传输层 / 打开网址
// ---------------------------------------------------------------------------

/// 联网用的传输层：假 Tokenway 在跑时直连本机，否则是按"软件网络"代理建的。
pub fn transport() -> Result<Arc<dyn Transport>, String> {
    builtin::make_transport()
}

/// 产品里用系统默认浏览器打开授权页（地址已经由 core 先过了白名单）。
pub struct SystemOpener;

impl UrlOpener for SystemOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        zssh_core::external_url::open_external_url(url)
    }
}

/// 授权用的"浏览器"：假 Tokenway 在跑时是自动点"授权"的假浏览器，否则是系统浏览器。
pub fn opener() -> Arc<dyn UrlOpener> {
    match fake() {
        Some(fake) => Arc::new(fake.browser.clone()),
        None => Arc::new(SystemOpener),
    }
}

/// 打开注册页 / 充值页：地址必须通过白名单；假 Tokenway 在跑时只记下来、不真的打开。
pub fn open_page(url: &str) -> Result<(), String> {
    policy()
        .check(url)
        .map_err(|why| format!("这个地址不在允许打开的名单里（{why}）"))?;
    if let Some(fake) = fake() {
        fake.opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(url.to_owned());
        return Ok(());
    }
    zssh_core::external_url::open_external_url(url)
}

pub fn open_topup() -> Result<(), String> {
    let url = tokenway_preset()
        .and_then(|preset| preset.topup_url)
        .ok_or_else(|| "清单里没有充值地址".to_owned())?;
    open_page(&url)
}

pub fn open_register() -> Result<(), String> {
    let url = tokenway_preset()
        .and_then(|preset| preset.register_url)
        .ok_or_else(|| "清单里没有注册地址".to_owned())?;
    open_page(&url)
}

// ---------------------------------------------------------------------------
// 连接
// ---------------------------------------------------------------------------

/// 默认的设备名：计算机名的友好化版本，不含用户名。
pub fn default_device_name() -> String {
    zssh_core::ai::agent::builtin::tokenway::connect::friendly_device_name(
        std::env::var("COMPUTERNAME").ok().as_deref(),
    )
}

/// 授权成功后的东西（还没存库）。
#[derive(Debug)]
pub struct Connected {
    pub authorized: Authorized,
    /// 用新令牌取到的模型清单（取不到是 `None`）。
    pub models: Option<Vec<String>>,
}

/// 连接流程要用的东西。
pub struct ConnectDeps<'a> {
    pub preset: &'a Preset,
    pub policy: &'a UrlPolicy,
    pub transport: &'a dyn Transport,
    pub opener: &'a dyn UrlOpener,
    /// 授权页上显示的设备名。
    pub device: &'a str,
    pub timeout: Duration,
}

/// 授权 + 用新令牌取一次模型清单。取消 / 超时 / 拒绝 / 网络错误都是明确的 [`ConnectError`]。
pub async fn run_connect(
    deps: &ConnectDeps<'_>,
    cancel: &Notify,
    on_stage: &(dyn Fn(Stage) + Send + Sync),
) -> Result<Connected, ConnectError> {
    let ConnectDeps {
        preset,
        policy,
        transport,
        opener,
        device,
        timeout,
    } = *deps;
    let urls = preset.authorize_urls().ok_or_else(|| {
        ConnectError::Api(ApiError {
            kind: ApiErrorKind::Refused,
            message: "这份预设不支持一键授权".to_owned(),
            status: None,
            code: None,
        })
    })?;
    let aff = preset
        .register_url
        .as_deref()
        .and_then(aff_from_register_url);
    let authorized = authorize(
        AuthorizeParams {
            urls,
            device,
            aff: aff.as_deref(),
            policy,
            timeout,
        },
        opener,
        transport,
        cancel,
        on_stage,
    )
    .await?;
    // 拿新令牌发请求之前先确认接入地址在白名单里。
    let base = resolve_base_url(preset, &authorized, policy)
        .map_err(|err| ConnectError::Internal(err.to_string()))?;
    let config = ProviderConfig::new(ProviderKind::OpenAiChat, base, authorized.api_key.clone());
    let models = fetch_models(transport, &config, MODELS_TIMEOUT)
        .await
        .map(|list| list.into_iter().map(|(id, _)| id).collect());
    Ok(Connected { authorized, models })
}

/// 把授权结果存成接口（Key 进保险库）。调用方负责让面板 / 设置页重读（`builtin::reload`）。
pub fn finish_connect(
    db: &Database,
    preset: &Preset,
    connected: &Connected,
    policy: &UrlPolicy,
) -> Result<AiEndpoint, String> {
    let endpoint = save_authorized(
        db,
        preset,
        &connected.authorized,
        connected.models.as_deref(),
        policy,
    )
    .map_err(|err| err.to_string())?;
    // 换了令牌：余额要重新取（立刻允许，不受上次时间限制）。
    book().forget(endpoint.id);
    BALANCE_VERSION.fetch_add(1, Ordering::SeqCst);
    Ok(endpoint)
}

/// 断开：通知 Tokenway 停用（失败也继续）→ 删保险库里的 Key → 删接口配置。
pub async fn disconnect_endpoint(endpoint_id: i64) -> Result<DisconnectReport, String> {
    let db = crate::hosts_demo::open_database().map_err(|err| format!("打开设置库失败：{err}"))?;
    let transport = transport()?;
    let revoke_url = tokenway_preset().and_then(|preset| preset.revoke_url);
    let uid = db.ai_endpoint_get(endpoint_id).ok().map(|e| e.uid);
    let report = disconnect(
        &db,
        transport.as_ref(),
        revoke_url.as_deref(),
        &policy(),
        endpoint_id,
    )
    .await
    .map_err(|err| err.to_string())?;
    book().forget(endpoint_id);
    if let Some(uid) = uid {
        builtin::remote_forget(&uid);
    }
    builtin::reload();
    Ok(report)
}

// ---------------------------------------------------------------------------
// 余额
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BalanceProblem {
    /// 令牌被服务端停用 / 过期（401）。
    AuthorizationInvalid,
    /// 这次没取到（网络、限速、服务器问题）；有上次的数就继续显示。
    Unavailable(String),
}

/// 面板上显示的余额。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceView {
    pub label: String,
    pub detail: String,
    pub needs_topup: bool,
    pub problem: Option<BalanceProblem>,
}

struct Entry {
    balance: Option<Balance>,
    problem: Option<BalanceProblem>,
    dirty: bool,
    in_flight: bool,
    gate: RefreshGate,
}

impl Entry {
    fn new() -> Self {
        Self {
            balance: None,
            problem: None,
            dirty: true,
            in_flight: false,
            gate: RefreshGate::new(BALANCE_MIN_INTERVAL),
        }
    }
}

/// 每个接口一份余额状态（纯数据，节流用传进来的时间，所以能写状态测试）。
#[derive(Default)]
pub struct BalanceBook {
    entries: HashMap<i64, Entry>,
}

impl BalanceBook {
    /// 该不该现在去取一次？第一次看到这个接口就要取（打开面板时）；之后只有被标脏（一轮对话结束、
    /// 重新打开面板）才取；取的间隔至少 60 秒，没轮到就留着，下个节拍再看。
    pub fn poll(&mut self, endpoint_id: i64, now: Instant) -> bool {
        let entry = self.entries.entry(endpoint_id).or_insert_with(Entry::new);
        if !entry.dirty || entry.in_flight || !entry.gate.try_start(now) {
            return false;
        }
        entry.in_flight = true;
        entry.dirty = false;
        true
    }

    pub fn finish(&mut self, endpoint_id: i64, result: Result<Balance, ApiError>) {
        let entry = self.entries.entry(endpoint_id).or_insert_with(Entry::new);
        entry.in_flight = false;
        match result {
            Ok(balance) => {
                entry.balance = Some(balance);
                entry.problem = None;
            }
            Err(err) if err.kind == ApiErrorKind::InvalidToken => {
                entry.balance = None;
                entry.problem = Some(BalanceProblem::AuthorizationInvalid);
            }
            Err(err) => entry.problem = Some(BalanceProblem::Unavailable(err.message)),
        }
    }

    pub fn mark_all_dirty(&mut self) {
        for entry in self.entries.values_mut() {
            entry.dirty = true;
        }
    }

    /// 忘掉这个接口的余额（断开、重新连接换了令牌）。
    pub fn forget(&mut self, endpoint_id: i64) {
        self.entries.remove(&endpoint_id);
    }

    pub fn view(&self, endpoint_id: i64) -> Option<BalanceView> {
        let entry = self.entries.get(&endpoint_id)?;
        match (&entry.balance, &entry.problem) {
            (_, Some(BalanceProblem::AuthorizationInvalid)) => Some(BalanceView {
                label: "授权已失效".to_owned(),
                detail: "Tokenway 授权已失效（可能在网站上改过密码或停用了授权），请重新连接"
                    .to_owned(),
                needs_topup: false,
                problem: entry.problem.clone(),
            }),
            (Some(balance), problem) => Some(BalanceView {
                label: balance.label(),
                detail: balance.detail(),
                needs_topup: balance.needs_topup,
                problem: problem.clone(),
            }),
            (None, _) => None,
        }
    }
}

static BOOK: Mutex<Option<BalanceBook>> = Mutex::new(None);
static BALANCE_VERSION: AtomicU64 = AtomicU64::new(0);

struct BookGuard(MutexGuard<'static, Option<BalanceBook>>);

impl std::ops::Deref for BookGuard {
    type Target = BalanceBook;
    fn deref(&self) -> &BalanceBook {
        self.0.as_ref().expect("book 已初始化")
    }
}

impl std::ops::DerefMut for BookGuard {
    fn deref_mut(&mut self) -> &mut BalanceBook {
        self.0.as_mut().expect("book 已初始化")
    }
}

fn book() -> BookGuard {
    let mut guard = BOOK.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        *guard = Some(BalanceBook::default());
    }
    BookGuard(guard)
}

/// 余额显示变了就加一：面板每个节拍比一下，变了才重画。
pub fn balance_version() -> u64 {
    BALANCE_VERSION.load(Ordering::SeqCst)
}

/// 这个接口现在要显示的余额（还没取到是 `None`）。
pub fn balance_view(endpoint_id: i64) -> Option<BalanceView> {
    book().view(endpoint_id)
}

/// 一轮对话结束 / 重新打开面板：下次允许时重新取余额。
pub fn mark_balance_dirty() {
    book().mark_all_dirty();
}

/// 面板每个节拍对"选中的是浏览器授权的 Tokenway 接口"调一次：该取的时候在后台取，最多每 60 秒一次。
pub fn balance_poll(endpoint_id: i64) {
    if !book().poll(endpoint_id, Instant::now()) {
        return;
    }
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        book().finish(endpoint_id, Err(ApiError::network("后台运行时没起来")));
        return;
    };
    runtime.spawn(async move {
        let result = fetch_balance(endpoint_id).await;
        book().finish(endpoint_id, result);
        BALANCE_VERSION.fetch_add(1, Ordering::SeqCst);
    });
}

async fn fetch_balance(endpoint_id: i64) -> Result<Balance, ApiError> {
    let failed = |why: String| ApiError {
        kind: ApiErrorKind::Refused,
        message: why,
        status: None,
        code: None,
    };
    let (endpoint, key) = {
        let db = crate::hosts_demo::open_database()
            .map_err(|err| failed(format!("打开设置库失败：{err}")))?;
        crate::agent_host::load_endpoint_from(&db, endpoint_id).map_err(failed)?
    };
    if !is_authorized_token(endpoint.preset_id.as_deref()) {
        return Err(failed("这个接口不是浏览器授权的 Tokenway 令牌".to_owned()));
    }
    let status_url = tokenway_preset()
        .and_then(|preset| preset.status_url)
        .ok_or_else(|| failed("清单里没有余额地址".to_owned()))?;
    let transport = transport().map_err(failed)?;
    fetch_status(transport.as_ref(), &policy(), &status_url, &key).await
}

// ---------------------------------------------------------------------------
// "重新连接"请求（AI 面板里的按钮 → 主窗口下一个节拍打开连接对话框）
// ---------------------------------------------------------------------------

static RECONNECT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_reconnect() {
    RECONNECT_REQUESTED.store(true, Ordering::SeqCst);
}

/// 取走"想重新连接"的请求（取一次就清掉）。
pub fn take_reconnect_request() -> bool {
    RECONNECT_REQUESTED.swap(false, Ordering::SeqCst)
}

#[cfg(test)]
mod tests;
