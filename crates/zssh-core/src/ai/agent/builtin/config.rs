//! 内置助手的"接口"配置（V2-A3）：一个接口 = 地址 + 类型 + Key（在凭据保险库里）+ 模型清单 +
//! 手动能力覆盖 + OpenAI 兼容各家差异的开关。
//!
//! 这里只放数据类型和校验；落库在 [`crate::store::ai_endpoints`]，同步在 `sync/` 里。
//! API Key 不出现在这些结构体里（只有 `key_identity_id` 指向保险库里的一条密码类凭据）；
//! 草稿里的 `api_key` 是明文输入，`Debug` 不打印。

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::catalog::CapsOverride;
use super::provider::{ApiKey, OpenAiOptions, ProviderConfig, ProviderKind, Switch};
use crate::error::ZsshError;

/// 接口类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// OpenAI 兼容 Chat Completions。
    OpenAiChat,
    /// Anthropic Messages。
    Anthropic,
}

impl EndpointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai_chat",
            Self::Anthropic => "anthropic",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ZsshError> {
        match raw {
            "openai_chat" => Ok(Self::OpenAiChat),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(ZsshError::InvalidInput(format!("未知的接口类型：{other}"))),
        }
    }

    pub fn provider_kind(self) -> ProviderKind {
        match self {
            Self::OpenAiChat => ProviderKind::OpenAiChat,
            Self::Anthropic => ProviderKind::Anthropic,
        }
    }
}

/// 自动 / 强制开 / 强制关（存储和界面用；转成 provider 层的 [`Switch`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    #[default]
    Auto,
    On,
    Off,
}

impl From<Tri> for Switch {
    fn from(value: Tri) -> Self {
        match value {
            Tri::Auto => Switch::Auto,
            Tri::On => Switch::On,
            Tri::Off => Switch::Off,
        }
    }
}

/// OpenAI 兼容接口的差异开关（对应 [`OpenAiOptions`]）。每个字段缺省 = 自动，所以以后加字段
/// 不影响已经存下的配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiOptionsConfig {
    /// 输出上限用 `max_completion_tokens`（On）还是 `max_tokens`（Off）。
    pub max_completion_tokens: Tri,
    /// assistant 历史里带不带 `reasoning_content`。
    pub echo_reasoning_content: Tri,
    /// 发不发 DeepSeek 风格的 `thinking: {type}` 开关。
    pub thinking_toggle: Tri,
    /// 发 `stream_options.include_usage`。
    pub include_usage: bool,
    /// 这个接口不需要 Key（Ollama、vLLM、本机网关等）：请求不带 `Authorization` 头。
    /// 放在这组选项里是因为只有 OpenAI 兼容接口允许没有 Key（Anthropic 仍必填），
    /// 这样接口表和同步格式都不用加字段。
    pub no_key: bool,
}

impl Default for OpenAiOptionsConfig {
    fn default() -> Self {
        Self {
            max_completion_tokens: Tri::Auto,
            echo_reasoning_content: Tri::Auto,
            thinking_toggle: Tri::Auto,
            include_usage: true,
            no_key: false,
        }
    }
}

impl From<&OpenAiOptionsConfig> for OpenAiOptions {
    fn from(value: &OpenAiOptionsConfig) -> Self {
        OpenAiOptions {
            max_completion_tokens: value.max_completion_tokens.into(),
            echo_reasoning_content: value.echo_reasoning_content.into(),
            thinking_toggle: value.thinking_toggle.into(),
            include_usage: value.include_usage,
        }
    }
}

/// 一个已保存的接口。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiEndpoint {
    pub id: i64,
    /// 同步用的全局标识。
    pub uid: String,
    pub name: String,
    pub kind: EndpointKind,
    /// 用户填的地址（已带版本段，如 `https://host/v1`）。
    pub base_url: String,
    /// 保险库里存 Key 的凭据；没填 Key（Ollama 之类）就是 `None`。
    pub key_identity_id: Option<i64>,
    /// 保险库里那条凭据有没有密文（界面显示"已设置 Key"用）。
    pub has_key: bool,
    pub default_model: Option<String>,
    /// 用户手填的额外模型（`GET /models` 列不出来的也能选）。
    pub models: Vec<String>,
    /// 按模型名的手动能力覆盖。
    pub overrides: BTreeMap<String, CapsOverride>,
    pub openai_options: OpenAiOptionsConfig,
    /// 来自哪个预设（如 `tokenway`），自己填的是 `None`。
    pub preset_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AiEndpoint {
    /// 明确设为"不需要 Key"（只有 OpenAI 兼容接口可以）。
    pub fn key_not_needed(&self) -> bool {
        self.kind == EndpointKind::OpenAiChat && self.openai_options.no_key
    }

    /// 配好了、能用来对话：有 Base URL，并且（有 Key，或 Key 被明确设为不需要）。
    pub fn is_ready(&self) -> bool {
        !self.base_url.trim().is_empty() && (self.has_key || self.key_not_needed())
    }

    /// 拼 provider 层的连接配置。`api_key` 由调用方从保险库取（[`crate::store::Database::ai_endpoint_api_key`]）。
    pub fn provider_config(&self, api_key: ApiKey) -> ProviderConfig {
        let mut config = ProviderConfig::new(self.kind.provider_kind(), &self.base_url, api_key);
        config.openai = OpenAiOptions::from(&self.openai_options);
        config
    }
}

/// 新建 / 修改接口的请求。
#[derive(Clone, PartialEq)]
pub struct AiEndpointDraft {
    pub id: Option<i64>,
    pub name: String,
    pub kind: EndpointKind,
    pub base_url: String,
    /// 明文 Key：`None` = 不改已存的；`Some("")` = 清掉；有值 = 存进保险库。
    pub api_key: Option<String>,
    pub default_model: Option<String>,
    pub models: Vec<String>,
    pub overrides: BTreeMap<String, CapsOverride>,
    pub openai_options: OpenAiOptionsConfig,
    pub preset_id: Option<String>,
}

impl AiEndpointDraft {
    /// 拼 provider 层的连接配置（"测试"按钮用表单里还没保存的值）。`api_key` 由调用方决定
    /// 用表单里新填的还是库里已存的。
    pub fn provider_config(&self, api_key: ApiKey) -> ProviderConfig {
        let mut config = ProviderConfig::new(
            self.kind.provider_kind(),
            self.base_url.trim().trim_end_matches('/'),
            api_key,
        );
        config.openai = OpenAiOptions::from(&self.openai_options);
        config
    }
}

/// `Debug` 手写：`api_key` 是明文，不能进日志。
impl fmt::Debug for AiEndpointDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiEndpointDraft")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("default_model", &self.default_model)
            .field("models", &self.models)
            .field("preset_id", &self.preset_id)
            .finish_non_exhaustive()
    }
}

/// "默认接口 + 默认模型"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinDefault {
    pub endpoint_id: i64,
    pub endpoint_uid: String,
    /// 空 = 用接口自己的默认模型。
    pub model: Option<String>,
}

pub const NAME_MAX_CHARS: usize = 100;
pub const URL_MAX_CHARS: usize = 2048;
pub const MODEL_MAX_CHARS: usize = 200;
pub const MODELS_MAX: usize = 200;
pub const OVERRIDES_MAX: usize = 200;
pub const PRESET_MAX_CHARS: usize = 64;

/// 接口名对应的 Key 凭据名。
pub fn key_identity_name(endpoint_name: &str) -> String {
    format!("{} 的 API Key", endpoint_name.trim())
}

fn plain(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

/// 地址只认 http / https，不含空白和控制字符。
pub fn valid_base_url(url: &str) -> bool {
    let url = url.trim();
    url.chars().count() <= URL_MAX_CHARS
        && (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() > "http://".len()
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// 保存前的整理和校验：名字、地址、模型清单、覆盖表都在这里过一遍。返回整理后的草稿
/// （名字、地址去首尾空白，模型清单去重去空）。
pub fn normalize_draft(draft: &AiEndpointDraft) -> Result<AiEndpointDraft, ZsshError> {
    let invalid = |why: &str| ZsshError::InvalidInput(why.to_owned());
    let mut out = draft.clone();
    out.name = draft.name.trim().to_owned();
    if !plain(&out.name, NAME_MAX_CHARS) {
        return Err(invalid("接口名不能为空，且不能超过 100 个字符"));
    }
    out.base_url = draft.base_url.trim().trim_end_matches('/').to_owned();
    if !valid_base_url(&out.base_url) {
        return Err(invalid(
            "接口地址要以 http:// 或 https:// 开头，不能含空格或控制字符",
        ));
    }
    out.default_model = draft
        .default_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    if let Some(model) = &out.default_model {
        if !plain(model, MODEL_MAX_CHARS) {
            return Err(invalid("默认模型名不合法"));
        }
    }
    let mut models: Vec<String> = Vec::new();
    for model in &draft.models {
        let model = model.trim();
        if model.is_empty() || models.iter().any(|m| m == model) {
            continue;
        }
        if !plain(model, MODEL_MAX_CHARS) {
            return Err(invalid("模型名不合法（太长或含控制字符）"));
        }
        models.push(model.to_owned());
    }
    if models.len() > MODELS_MAX {
        return Err(invalid("手填的模型太多了（最多 200 个）"));
    }
    out.models = models;
    if draft.overrides.len() > OVERRIDES_MAX {
        return Err(invalid("手动能力覆盖太多了（最多 200 个）"));
    }
    if draft.overrides.keys().any(|m| !plain(m, MODEL_MAX_CHARS)) {
        return Err(invalid("手动能力覆盖里有不合法的模型名"));
    }
    if out.openai_options.no_key {
        if out.kind != EndpointKind::OpenAiChat {
            return Err(invalid("只有 OpenAI 兼容接口可以不填 API Key"));
        }
        // 明确说了不需要 Key：已经存着的 Key 一起清掉，免得留一把没人用的密钥。
        out.api_key = Some(String::new());
    }
    out.preset_id = draft
        .preset_id
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    if let Some(preset) = &out.preset_id {
        if !plain(preset, PRESET_MAX_CHARS) {
            return Err(invalid("预设标识不合法"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> AiEndpointDraft {
        AiEndpointDraft {
            id: None,
            name: " 我的接口 ".to_owned(),
            kind: EndpointKind::OpenAiChat,
            base_url: " https://api.example.com/v1/ ".to_owned(),
            api_key: Some("sk-test-key-0001".to_owned()),
            default_model: Some(" gpt-x ".to_owned()),
            models: vec!["a".into(), " a ".into(), "".into(), "b".into()],
            overrides: BTreeMap::new(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: Some(" ".to_owned()),
        }
    }

    #[test]
    fn normalize_trims_and_dedups() {
        let out = normalize_draft(&draft()).unwrap();
        assert_eq!(out.name, "我的接口");
        assert_eq!(out.base_url, "https://api.example.com/v1");
        assert_eq!(out.default_model.as_deref(), Some("gpt-x"));
        assert_eq!(out.models, vec!["a", "b"]);
        assert_eq!(out.preset_id, None);
    }

    #[test]
    fn normalize_rejects_bad_input() {
        let mut d = draft();
        d.name = "  ".into();
        assert!(normalize_draft(&d).is_err());
        let mut d = draft();
        d.base_url = "ftp://x".into();
        assert!(normalize_draft(&d).is_err());
        let mut d = draft();
        d.base_url = "https://a b".into();
        assert!(normalize_draft(&d).is_err());
        let mut d = draft();
        d.base_url = "https://".into();
        assert!(normalize_draft(&d).is_err());
        let mut d = draft();
        d.models = (0..201).map(|i| format!("m{i}")).collect();
        assert!(normalize_draft(&d).is_err());
    }

    #[test]
    fn draft_debug_hides_the_key() {
        let shown = format!("{:?}", draft());
        assert!(!shown.contains("sk-test-key-0001"), "{shown}");
        assert!(shown.contains("***"));
    }

    #[test]
    fn options_round_trip_and_default_missing_fields() {
        let parsed: OpenAiOptionsConfig =
            serde_json::from_str(r#"{"max_completion_tokens":"on"}"#).unwrap();
        assert_eq!(parsed.max_completion_tokens, Tri::On);
        assert_eq!(parsed.echo_reasoning_content, Tri::Auto);
        assert!(parsed.include_usage);
        let options = OpenAiOptions::from(&parsed);
        assert_eq!(options.max_completion_tokens, Switch::On);
    }

    fn endpoint(kind: EndpointKind, has_key: bool, no_key: bool, url: &str) -> AiEndpoint {
        AiEndpoint {
            id: 1,
            uid: "u".into(),
            name: "x".into(),
            kind,
            base_url: url.into(),
            key_identity_id: has_key.then_some(7),
            has_key,
            default_model: None,
            models: Vec::new(),
            overrides: BTreeMap::new(),
            openai_options: OpenAiOptionsConfig {
                no_key,
                ..OpenAiOptionsConfig::default()
            },
            preset_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn ready_means_a_base_url_and_either_a_key_or_an_explicit_no_key() {
        let openai = EndpointKind::OpenAiChat;
        assert!(endpoint(openai, true, false, "http://h/v1").is_ready());
        assert!(endpoint(openai, false, true, "http://h/v1").is_ready());
        assert!(!endpoint(openai, false, false, "http://h/v1").is_ready());
        assert!(
            !endpoint(openai, true, false, "  ").is_ready(),
            "没有地址不算配好"
        );
        // Anthropic 不能靠"不需要 Key"过关。
        assert!(!endpoint(EndpointKind::Anthropic, false, true, "http://h").is_ready());
        assert!(endpoint(EndpointKind::Anthropic, true, false, "http://h").is_ready());
    }

    #[test]
    fn no_key_is_only_for_openai_compatible_and_clears_a_typed_key() {
        let mut d = draft();
        d.openai_options.no_key = true;
        let out = normalize_draft(&d).unwrap();
        assert_eq!(out.api_key.as_deref(), Some(""), "存着的 Key 一起清掉");
        d.kind = EndpointKind::Anthropic;
        assert!(normalize_draft(&d).is_err());
    }

    #[test]
    fn old_options_without_no_key_still_load() {
        let parsed: OpenAiOptionsConfig =
            serde_json::from_str(r#"{"include_usage":false}"#).unwrap();
        assert!(!parsed.no_key);
    }

    #[test]
    fn kind_strings_round_trip() {
        for kind in [EndpointKind::OpenAiChat, EndpointKind::Anthropic] {
            assert_eq!(EndpointKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(EndpointKind::parse("responses").is_err());
    }
}
