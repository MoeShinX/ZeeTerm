//! ZeeTerm 内置助手在 `agent_setup` 里的那一份（V2-A4）：接口清单的进程级状态、"配好了没有"的判定、
//! 模型下拉要的目录（含思考强度变体）、用量信号翻译、模型资料库（models.dev）的加载与后台更新。
//!
//! 面板（`agent_chat_demo`）和设置页读同一份进程级状态；设置页保存 / 删除接口后调
//! [`reload`]，面板下一个节拍据 [`version`] 变了就重读。业务判定（能力识别、映射、对话循环）
//! 全是 core 的，这里只拼装。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use zssh_core::ai::agent::builtin::catalog::{
    CapsSource, Catalog, CatalogOrigin, ModelCaps, PartialCaps, ReasoningSupport, ReqwestFetch,
    refresh_if_stale, resolve, unix_now, unix_to_date,
};
use zssh_core::ai::agent::builtin::config::{AiEndpoint, EndpointKind};
use zssh_core::ai::agent::builtin::mapping::thinking_options;
use zssh_core::ai::agent::builtin::provider::{ReqwestTransport, Transport};
use zssh_core::ai::agent::builtin::{NoticeKind, TurnUsage};
use zssh_core::ai::agent::usage::ContextUsage;
use zssh_core::ai::agent::{AgentModel, AgentModelCatalog};
use zssh_core::store::Database;

use super::status::{AgentStatus, InstallState};

/// 模型资料库缓存放在数据目录下的这个子目录。
pub const CATALOG_DIR: &str = "ai-models";

// ---------------------------------------------------------------------------
// 接口清单（进程级）
// ---------------------------------------------------------------------------

/// 界面要用的接口摘要（不含 Key）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    pub id: i64,
    pub uid: String,
    pub name: String,
    pub kind: EndpointKind,
    /// 配好了 Key（接口表只记有没有，Key 在保险库里）。
    pub has_key: bool,
    pub default_model: Option<String>,
    /// 改了就变：模型目录、远端模型清单的缓存键用它。
    pub updated_at: i64,
}

/// "设为默认接口 + 默认模型"。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultChoice {
    pub endpoint_id: i64,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuiltinInfo {
    pub endpoints: Vec<EndpointInfo>,
    pub default: Option<DefaultChoice>,
}

impl BuiltinInfo {
    /// 内置助手算不算可用：至少有一个配好 Key 的接口。
    pub fn usable(&self) -> bool {
        self.endpoints.iter().any(|endpoint| endpoint.has_key)
    }

    pub fn get(&self, id: i64) -> Option<&EndpointInfo> {
        self.endpoints.iter().find(|endpoint| endpoint.id == id)
    }

    /// 面板里能选的接口（配好 Key 的）。
    pub fn selectable(&self) -> Vec<&EndpointInfo> {
        self.endpoints.iter().filter(|e| e.has_key).collect()
    }

    /// 新对话默认用哪个接口：设置里的默认接口（它得可用）> 第一个可用的。
    pub fn default_endpoint(&self) -> Option<&EndpointInfo> {
        self.default
            .as_ref()
            .and_then(|choice| self.get(choice.endpoint_id))
            .filter(|endpoint| endpoint.has_key)
            .or_else(|| self.endpoints.iter().find(|endpoint| endpoint.has_key))
    }

    /// 这个接口开新对话时默认选哪个模型：设为默认的那个接口用设置里的默认模型，
    /// 其余用接口自己的默认模型。
    pub fn initial_model(&self, endpoint_id: i64) -> Option<String> {
        let chosen = self
            .default
            .as_ref()
            .filter(|choice| choice.endpoint_id == endpoint_id)
            .and_then(|choice| choice.model.clone());
        chosen.or_else(|| self.get(endpoint_id).and_then(|e| e.default_model.clone()))
    }

    /// 面板上显示的名字：只有一个可选接口时就叫"ZeeTerm 内置助手"，多个时带上接口名。
    pub fn label_for(&self, id: i64) -> String {
        let name = self.get(id).map(|endpoint| endpoint.name.as_str());
        match (name, self.selectable().len()) {
            (Some(name), count) if count > 1 => format!("内置 · {name}"),
            _ => super::AgentKind::Builtin.label().to_owned(),
        }
    }
}

fn read_info(db: &Database) -> BuiltinInfo {
    let endpoints = match db.ai_endpoint_list() {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!(error = %err, "读取内置助手接口失败");
            Vec::new()
        }
    };
    let default = db
        .ai_builtin_default()
        .ok()
        .flatten()
        .map(|choice| DefaultChoice {
            endpoint_id: choice.endpoint_id,
            model: choice.model,
        });
    BuiltinInfo {
        endpoints: endpoints
            .into_iter()
            .map(|endpoint| EndpointInfo {
                id: endpoint.id,
                uid: endpoint.uid,
                name: endpoint.name,
                kind: endpoint.kind,
                has_key: endpoint.has_key,
                default_model: endpoint.default_model,
                updated_at: endpoint.updated_at,
            })
            .collect(),
        default,
    }
}

/// 库里现在有没有可用的内置接口（设置库已经打开时用）。
pub fn usable_in(db: &Database) -> bool {
    read_info(db).usable()
}

struct State {
    version: u64,
    info: Option<BuiltinInfo>,
}

static STATE: Mutex<State> = Mutex::new(State {
    version: 0,
    info: None,
});

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn load_from_disk() -> BuiltinInfo {
    match crate::hosts_demo::open_database() {
        Ok(db) => read_info(&db),
        Err(err) => {
            tracing::warn!(error = %err, "打开设置库失败，内置助手按没配置处理");
            BuiltinInfo::default()
        }
    }
}

/// 现在的接口清单（第一次调用时从设置库读）。
pub fn info() -> BuiltinInfo {
    let mut state = state();
    if state.info.is_none() {
        state.info = Some(load_from_disk());
        state.version += 1;
    }
    state.info.clone().unwrap_or_default()
}

/// 状态的版本号：接口清单、资料库任何一项变了就加一。
pub fn version() -> u64 {
    let _ = info();
    state().version
}

/// 设置页保存 / 删除接口、改默认之后调：重读设置库，内容变了才加版本号。
pub fn reload() {
    apply(load_from_disk());
}

/// 同 [`reload`]，用已经打开的库（主窗口节拍里定时调：同步、别的窗口改了接口也能跟上，不用每次重新开库）。
pub fn reload_with(db: &Database) {
    apply(read_info(db));
}

fn apply(fresh: BuiltinInfo) {
    let mut state = state();
    if state.info.as_ref() != Some(&fresh) {
        state.info = Some(fresh);
        state.version += 1;
    }
}

/// 面板和设置页读的"内置助手状态"：用 [`AgentStatus`] 的外壳套进已有的可用性判断里。
/// 有可用接口 = 就绪；没有 = 未配置（面板显示"还没有配置内置助手"，带"去设置"）。
pub fn status() -> AgentStatus {
    let install = if info().usable() {
        InstallState::Found(zssh_core::ai::agent::discovery::AgentInstall {
            path: String::new(),
            version: String::new(),
            source: zssh_core::ai::agent::discovery::DiscoverySource::Path,
            needs_shell: false,
            auto: true,
            too_old: false,
            skipped_older: None,
        })
    } else {
        InstallState::Missing
    };
    AgentStatus {
        install,
        login: None,
        guard: None,
    }
}

/// 面板里内置助手没配置时的整段说明。
pub const MISSING_TEXT: &str = "还没有配置内置助手：到 设置 → AI → 内置助手 添加一个接口并填好 API Key，就能直接对话，不用安装别的工具。";

// ---------------------------------------------------------------------------
// 模型资料库（models.dev）
// ---------------------------------------------------------------------------

static CATALOG: Mutex<Option<Arc<Catalog>>> = Mutex::new(None);

pub fn catalog_dir() -> PathBuf {
    crate::settings::data_dir().join(CATALOG_DIR)
}

/// 当前的资料库：缓存 > 内置快照 > 空表；第一次用到时加载。
pub fn catalog() -> Arc<Catalog> {
    let mut slot = CATALOG.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(catalog) = slot.as_ref() {
        return Arc::clone(catalog);
    }
    let loaded = Arc::new(Catalog::load(&catalog_dir()));
    *slot = Some(Arc::clone(&loaded));
    loaded
}

/// 缓存文件变了（更新成功）之后重新加载。
pub fn reload_catalog() {
    let loaded = Arc::new(Catalog::load(&catalog_dir()));
    *CATALOG.lock().unwrap_or_else(|p| p.into_inner()) = Some(loaded);
    state().version += 1;
}

/// 设置页上"模型资料：……"那一行。
pub fn describe_catalog(catalog: &Catalog) -> String {
    let date = catalog.timestamp().map(unix_to_date);
    match (catalog.origin(), date) {
        (CatalogOrigin::Cache, Some(date)) => format!("模型资料：已更新于 {date}"),
        (CatalogOrigin::Snapshot, Some(date)) => format!("模型资料：内置快照 {date}"),
        (CatalogOrigin::Snapshot, None) => "模型资料：内置快照".to_owned(),
        (CatalogOrigin::Cache, None) => "模型资料：已更新".to_owned(),
        (CatalogOrigin::Empty, _) => "模型资料：没有加载到（按名字估计）".to_owned(),
    }
}

/// 软件网络代理（V2-U8）→ 传输层。调试版设了 `ZSSH_BUILTIN_FAKE` 就换成进程内的假接口。
pub fn make_transport() -> Result<Arc<dyn Transport>, String> {
    if let Some(fake) = super::builtin_fake::transport() {
        return Ok(fake);
    }
    let db = crate::hosts_demo::open_database().map_err(|err| format!("打开设置库失败：{err}"))?;
    let proxy = zssh_core::sync::app_proxy::app_proxy(&db)
        .map_err(|err| format!("软件网络的代理设置有问题：{err}"))?;
    let transport = ReqwestTransport::build(proxy.as_ref()).map_err(|err| err.to_string())?;
    Ok(Arc::new(transport))
}

/// 后台更新模型资料库：启动后延迟执行，设置里关了就不做，失败只记日志。
pub fn spawn_background_refresh(delay: std::time::Duration) {
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        return;
    };
    runtime.spawn(async move {
        tokio::time::sleep(delay).await;
        let enabled = crate::hosts_demo::open_database()
            .ok()
            .and_then(|db| db.ai_models_auto_update().ok())
            .unwrap_or(true);
        if !enabled || super::builtin_fake::transport().is_some() {
            return;
        }
        match refresh_catalog(false).await {
            Ok(true) => reload_catalog(),
            Ok(false) => {}
            Err(reason) => tracing::info!(%reason, "模型资料库后台更新没成功，继续用现有的"),
        }
    });
}

/// 拉一次资料库。`force` = 不管缓存新不新鲜（"立即更新"）。成功更新了返回 `true`。
pub async fn refresh_catalog(force: bool) -> Result<bool, String> {
    let db = crate::hosts_demo::open_database().map_err(|err| format!("打开设置库失败：{err}"))?;
    let proxy = zssh_core::sync::app_proxy::app_proxy(&db)
        .map_err(|err| format!("软件网络的代理设置有问题：{err}"))?;
    let http = ReqwestFetch::build(proxy.as_ref())?;
    let dir = catalog_dir();
    let now = unix_now();
    let outcome = if force {
        zssh_core::ai::agent::builtin::catalog::refresh_now(&dir, &http, now).await
    } else {
        refresh_if_stale(&dir, &http, now, true).await
    };
    use zssh_core::ai::agent::builtin::catalog::RefreshOutcome::*;
    match outcome {
        Updated { .. } => Ok(true),
        Fresh | Disabled => Ok(false),
        Failed(reason) => Err(reason),
    }
}

// ---------------------------------------------------------------------------
// 接口自报的模型清单（进程级缓存）
// ---------------------------------------------------------------------------

/// `GET /models` 的结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteModels {
    pub order: Vec<String>,
    pub caps: HashMap<String, PartialCaps>,
}

impl RemoteModels {
    pub fn from_list(list: Vec<(String, PartialCaps)>) -> Self {
        let order = list.iter().map(|(id, _)| id.clone()).collect();
        Self {
            order,
            caps: list.into_iter().collect(),
        }
    }
}

enum Fetch {
    Loading,
    Ready(Arc<RemoteModels>),
    /// 没取到（不提供 /models、连不上）：不自动重试，改了接口（`updated_at` 变）才重来。
    Failed,
}

static REMOTE: Mutex<Option<HashMap<String, Fetch>>> = Mutex::new(None);

fn remote_slot() -> MutexGuard<'static, Option<HashMap<String, Fetch>>> {
    REMOTE.lock().unwrap_or_else(|p| p.into_inner())
}

/// 缓存键：接口 uid + 更新时间。
pub fn remote_key(endpoint: &EndpointInfo) -> String {
    format!("{}|{}", endpoint.uid, endpoint.updated_at)
}

/// 已经取到的远端清单。
pub fn remote_get(key: &str) -> Option<Arc<RemoteModels>> {
    match remote_slot().as_ref()?.get(key)? {
        Fetch::Ready(models) => Some(Arc::clone(models)),
        _ => None,
    }
}

/// 这个键要不要发起一次拉取：没有记录才要，并立刻标成"取数中"。
pub fn remote_claim(key: &str) -> bool {
    let mut guard = remote_slot();
    let map = guard.get_or_insert_with(HashMap::new);
    if map.contains_key(key) {
        return false;
    }
    map.insert(key.to_owned(), Fetch::Loading);
    true
}

pub fn remote_finish(key: String, result: Option<RemoteModels>) {
    let entry = match result {
        Some(models) => Fetch::Ready(Arc::new(models)),
        None => Fetch::Failed,
    };
    remote_slot()
        .get_or_insert_with(HashMap::new)
        .insert(key, entry);
}

/// 忘掉某个接口的缓存（"测试"拿到了新清单、改了接口之后）。
pub fn remote_forget(uid: &str) {
    let prefix = format!("{uid}|");
    if let Some(map) = remote_slot().as_mut() {
        map.retain(|key, _| !key.starts_with(&prefix));
    }
}

// ---------------------------------------------------------------------------
// 模型能力 / 模型下拉目录（纯函数）
// ---------------------------------------------------------------------------

/// 一个模型的能力：手动覆盖 > 接口自报 > models.dev > 兜底（A1）。
pub fn caps_for(
    endpoint: &AiEndpoint,
    remote: Option<&RemoteModels>,
    catalog: &Catalog,
    model: &str,
) -> ModelCaps {
    let manual = endpoint.overrides.get(model).cloned();
    let reported = remote.and_then(|remote| remote.caps.get(model)).cloned();
    resolve(model, manual, reported, catalog)
}

/// 面板模型下拉要的目录：每个模型一项；支持调节思考的模型再加 `模型/强度` 变体（名字
/// `名称 · 强度`），这样下拉分组、强度下拉、发送时拼"模型/强度"串（A3 的格式）都走现成的
/// `model_picker`。`default` 是新对话默认选的模型。
pub fn picker_catalog(
    endpoint: &AiEndpoint,
    remote: Option<&RemoteModels>,
    catalog: &Catalog,
    default: Option<&str>,
) -> AgentModelCatalog {
    let mut ids: Vec<String> = Vec::new();
    let mut push = |id: &str| {
        let id = id.trim();
        if !id.is_empty() && !ids.iter().any(|known| known == id) {
            ids.push(id.to_owned());
        }
    };
    if let Some(default) = default {
        push(default);
    }
    if let Some(default) = &endpoint.default_model {
        push(default);
    }
    for model in &endpoint.models {
        push(model);
    }
    if let Some(remote) = remote {
        for id in &remote.order {
            push(id);
        }
    }
    let mut models = Vec::new();
    for id in &ids {
        let caps = caps_for(endpoint, remote, catalog, id);
        let name = if caps.display_name.trim().is_empty() {
            id.clone()
        } else {
            caps.display_name.clone()
        };
        models.push(AgentModel {
            id: id.clone(),
            name: name.clone(),
            unavailable: false,
        });
        for option in thinking_options(&caps) {
            models.push(AgentModel {
                id: format!("{id}/{option}"),
                name: format!("{name} · {option}"),
                unavailable: false,
            });
        }
    }
    let default = default
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    AgentModelCatalog {
        models,
        default,
        default_label: None,
    }
}

/// 发送时选中的"模型/强度"串里的模型名（给能力查询用）。
pub fn model_of_selection(selection: &str, known: &[String]) -> String {
    zssh_core::ai::agent::builtin::mapping::parse_model_selection(selection, known).0
}

// ---------------------------------------------------------------------------
// 用量 / 提示信号 → 界面
// ---------------------------------------------------------------------------

/// 内置助手一次请求的用量 → 界面统一的 [`ContextUsage`]（窗口用 `window`，估计的标"估计"）。
pub fn usage_from_turn(turn: &TurnUsage) -> ContextUsage {
    let some_if_any = |n: u64| (n > 0).then_some(n);
    ContextUsage {
        used: turn.used,
        window: (turn.window > 0).then_some(u64::from(turn.window)),
        window_estimated: turn.window_estimated,
        input: Some(turn.input),
        output: Some(turn.output),
        cache_read: some_if_any(turn.cache_read),
        cache_write: some_if_any(turn.cache_write),
        model: Some(turn.model.clone()),
    }
}

/// 发第一条之前指示器要显示的"0%"：只知道窗口，还没用任何 token。
pub fn preview_usage(caps: &ModelCaps, model: &str) -> ContextUsage {
    ContextUsage {
        used: 0,
        window: (caps.context_window > 0).then_some(u64::from(caps.context_window)),
        window_estimated: caps.source == CapsSource::Heuristic,
        model: Some(model.to_owned()),
        ..ContextUsage::default()
    }
}

/// 服务端要求充值时的错误码（Tokenway 网关给 ZeeTerm 专用令牌的）。
pub const REQUIRES_TOPUP_CODE: &str = "zeeterm_requires_topup";

/// 余额不足的提示文字（按钮留给 A5，这里只有文字）。
pub const REQUIRES_TOPUP_TEXT: &str =
    "需要充值：这个接口的账户余额不足，充值后才能继续使用（充值入口稍后提供）。";

/// 提示的类别在对话里显示成什么样：压缩 / 历史修复是中性提示；不支持工具、输出被截断带警示。
pub fn notice_is_warning(kind: NoticeKind) -> bool {
    matches!(
        kind,
        NoticeKind::NoToolSupport | NoticeKind::OutputTruncated
    )
}

/// 当前模型不支持工具调用吗（输入框上方的淡色提示用）。
pub fn lacks_tools(caps: &ModelCaps) -> bool {
    !caps.tool_call
}

/// 模型能力的一行摘要（设置页"识别为"下面用）。
pub fn caps_summary(caps: &ModelCaps) -> Vec<(&'static str, String)> {
    let tokens = |n: u32| {
        if n >= 1_000_000 {
            format!("{} M", trim_zero(n as f64 / 1_000_000.0))
        } else if n >= 1_000 {
            format!("{} K", trim_zero(n as f64 / 1_000.0))
        } else {
            n.to_string()
        }
    };
    let thinking = match &caps.reasoning {
        ReasoningSupport::None => "不支持调节".to_owned(),
        ReasoningSupport::Toggle => "开关".to_owned(),
        ReasoningSupport::Levels(levels) => format!("等级：{}", levels.join(" / ")),
        ReasoningSupport::Budget { min, max } => {
            format!("预算 {} 到 {} token", tokens(*min), tokens(*max))
        }
    };
    let yes = |value: bool| if value { "支持" } else { "不支持" }.to_owned();
    vec![
        (
            "上下文窗口",
            format!("{} token", tokens(caps.context_window)),
        ),
        ("最大输出", format!("{} token", tokens(caps.max_output))),
        ("思考", thinking),
        ("工具调用", yes(caps.tool_call)),
        ("图片输入", yes(caps.image_input)),
    ]
}

fn trim_zero(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").unwrap_or(&text).to_owned()
}

/// 能力来源的中文名（设置页"来源：……"）。
pub fn source_label(source: CapsSource) -> &'static str {
    match source {
        CapsSource::Manual => "手动",
        CapsSource::Endpoint => "接口",
        CapsSource::ModelsDev => "models.dev",
        CapsSource::Builtin => "内置快照",
        CapsSource::Heuristic => "估计",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ai::agent::builtin::catalog::CapsOverride;
    use zssh_core::ai::agent::builtin::config::OpenAiOptionsConfig;

    fn info_with(endpoints: Vec<EndpointInfo>, default: Option<DefaultChoice>) -> BuiltinInfo {
        BuiltinInfo { endpoints, default }
    }

    fn endpoint_info(id: i64, name: &str, has_key: bool) -> EndpointInfo {
        EndpointInfo {
            id,
            uid: format!("uid-{id}"),
            name: name.to_owned(),
            kind: EndpointKind::OpenAiChat,
            has_key,
            default_model: Some(format!("model-{id}")),
            updated_at: 1,
        }
    }

    fn endpoint(models: &[&str], default: Option<&str>) -> AiEndpoint {
        AiEndpoint {
            id: 1,
            uid: "uid-1".into(),
            name: "主力".into(),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.example.com/v1".into(),
            key_identity_id: Some(1),
            has_key: true,
            default_model: default.map(str::to_owned),
            models: models.iter().map(|m| (*m).to_owned()).collect(),
            overrides: Default::default(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn only_endpoints_with_a_key_make_the_builtin_usable() {
        let none = BuiltinInfo::default();
        assert!(!none.usable());
        let keyless = info_with(vec![endpoint_info(1, "本地", false)], None);
        assert!(!keyless.usable(), "没 Key 的不算配好");
        assert!(keyless.default_endpoint().is_none());
        let both = info_with(
            vec![
                endpoint_info(1, "本地", false),
                endpoint_info(2, "主力", true),
            ],
            None,
        );
        assert!(both.usable());
        assert_eq!(both.default_endpoint().map(|e| e.id), Some(2));
        assert_eq!(both.selectable().len(), 1);
    }

    #[test]
    fn the_default_endpoint_and_model_follow_the_setting_when_it_is_usable() {
        let info = info_with(
            vec![endpoint_info(1, "甲", true), endpoint_info(2, "乙", true)],
            Some(DefaultChoice {
                endpoint_id: 2,
                model: Some("chosen".into()),
            }),
        );
        assert_eq!(info.default_endpoint().map(|e| e.id), Some(2));
        assert_eq!(info.initial_model(2).as_deref(), Some("chosen"));
        // 没设为默认的接口用自己的默认模型。
        assert_eq!(info.initial_model(1).as_deref(), Some("model-1"));
        // 设的默认接口没 Key 了：退回第一个可用的。
        let broken = info_with(
            vec![endpoint_info(1, "甲", true), endpoint_info(2, "乙", false)],
            Some(DefaultChoice {
                endpoint_id: 2,
                model: None,
            }),
        );
        assert_eq!(broken.default_endpoint().map(|e| e.id), Some(1));
    }

    #[test]
    fn labels_name_the_endpoint_only_when_there_is_a_choice() {
        let one = info_with(vec![endpoint_info(1, "主力", true)], None);
        assert_eq!(one.label_for(1), "ZeeTerm 内置助手");
        let two = info_with(
            vec![
                endpoint_info(1, "主力", true),
                endpoint_info(2, "备用", true),
            ],
            None,
        );
        assert_eq!(two.label_for(2), "内置 · 备用");
    }

    #[test]
    fn the_picker_lists_each_model_once_with_thinking_variants_and_a_default() {
        let catalog = Catalog::builtin();
        let remote = RemoteModels::from_list(vec![
            ("gpt-5.5".to_owned(), PartialCaps::default()),
            ("my-chat-model".to_owned(), PartialCaps::default()),
        ]);
        let picker = picker_catalog(
            &endpoint(&["my-chat-model"], Some("gpt-5.5")),
            Some(&remote),
            &catalog,
            Some("gpt-5.5"),
        );
        let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
        // 默认模型排第一、没有重复；gpt-5.5 支持调思考，所以带强度变体。
        assert_eq!(ids.iter().filter(|id| **id == "gpt-5.5").count(), 1);
        assert_eq!(ids[0], "gpt-5.5");
        assert!(ids.contains(&"gpt-5.5/high"), "{ids:?}");
        assert!(ids.contains(&"my-chat-model"));
        assert!(
            !ids.iter().any(|id| id.starts_with("my-chat-model/")),
            "认不出的模型兜底为不思考：不给强度变体 {ids:?}"
        );
        assert_eq!(picker.default.as_deref(), Some("gpt-5.5"));
        let high = picker
            .models
            .iter()
            .find(|m| m.id == "gpt-5.5/high")
            .unwrap();
        assert!(high.name.ends_with(" · high"), "{}", high.name);
        // 现成的下拉分组规则能把变体归到模型下面。
        let groups = crate::main_window::model_picker::build(Some(&picker));
        let gpt = groups
            .groups
            .iter()
            .find(|g| g.value == "gpt-5.5")
            .expect("有 gpt-5.5 这组");
        assert!(gpt.efforts.iter().any(|e| e == "high"));
        assert!(gpt.plain, "也可以不指定强度");
    }

    #[test]
    fn a_manual_override_beats_the_database_and_shows_as_manual() {
        let catalog = Catalog::builtin();
        let mut endpoint = endpoint(&[], Some("gpt-5.5"));
        endpoint.overrides.insert(
            "gpt-5.5".into(),
            CapsOverride {
                context_window: Some(64_000),
                tool_call: Some(false),
                ..CapsOverride::default()
            },
        );
        let caps = caps_for(&endpoint, None, &catalog, "gpt-5.5");
        assert_eq!(caps.context_window, 64_000);
        assert!(lacks_tools(&caps));
        assert_eq!(caps.source, CapsSource::Manual);
        assert_eq!(source_label(caps.source), "手动");
    }

    #[test]
    fn usage_signals_become_the_panel_usage_with_the_models_window() {
        let usage = usage_from_turn(&TurnUsage {
            model: "gpt-5.5".into(),
            used: 12_345,
            output: 200,
            input: 12_000,
            cache_read: 145,
            cache_write: 0,
            window: 400_000,
            window_estimated: true,
        });
        assert_eq!(usage.used, 12_345);
        assert_eq!(usage.window, Some(400_000));
        assert!(usage.window_estimated, "估计的窗口要标出来");
        assert_eq!(usage.cache_read, Some(145));
        assert_eq!(usage.cache_write, None, "没有缓存写入就不列");
        assert_eq!(usage.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(usage.percent().map(|p| p.round()), Some(3.0));
    }

    #[test]
    fn before_the_first_message_the_meter_shows_zero_of_the_window() {
        let catalog = Catalog::builtin();
        let caps = caps_for(&endpoint(&[], None), None, &catalog, "my-company-chat-v7");
        let preview = preview_usage(&caps, "my-company-chat-v7");
        assert_eq!(preview.used, 0);
        assert_eq!(preview.window, Some(u64::from(caps.context_window)));
        assert!(preview.window_estimated, "认不出的模型窗口是估计的");
        assert_eq!(preview.percent(), Some(0.0));
    }

    #[test]
    fn the_catalog_line_says_snapshot_or_updated_with_a_date() {
        let builtin = Catalog::builtin();
        let line = describe_catalog(&builtin);
        assert!(line.starts_with("模型资料：内置快照 20"), "{line}");
        assert_eq!(
            describe_catalog(&Catalog::empty()),
            "模型资料：没有加载到（按名字估计）"
        );
    }

    #[test]
    fn caps_summary_lists_window_output_thinking_tools_and_images() {
        let catalog = Catalog::builtin();
        let caps = caps_for(&endpoint(&[], None), None, &catalog, "gpt-5.5");
        let lines = caps_summary(&caps);
        let names: Vec<&str> = lines.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            ["上下文窗口", "最大输出", "思考", "工具调用", "图片输入"]
        );
        assert!(lines[2].1.starts_with("等级："), "{:?}", lines[2]);
    }

    #[test]
    fn remote_models_are_fetched_once_per_endpoint_revision() {
        let key = "uid-remote-test|1".to_owned();
        assert!(remote_claim(&key));
        assert!(!remote_claim(&key), "取数中不重复发");
        remote_finish(key.clone(), None);
        assert!(remote_get(&key).is_none());
        assert!(!remote_claim(&key), "失败过也不自动重试");
        // 接口改了（updated_at 变）= 新的键，可以重新取。
        assert!(remote_claim("uid-remote-test|2"));
        remote_finish(
            "uid-remote-test|2".to_owned(),
            Some(RemoteModels::from_list(vec![(
                "a".into(),
                Default::default(),
            )])),
        );
        assert_eq!(
            remote_get("uid-remote-test|2").map(|r| r.order.clone()),
            Some(vec!["a".to_owned()])
        );
        remote_forget("uid-remote-test");
        assert!(remote_get("uid-remote-test|2").is_none());
        assert!(remote_claim(&key));
    }
}
