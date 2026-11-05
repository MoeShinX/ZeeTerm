//! 设置 → AI → 内置助手：添加 / 编辑接口的对话框（V2-A4）。
//!
//! 沿用主机属性对话框（V2-U7 / U13）的外壳和单栏表单：名称、类型（分段按钮）、Base URL、API Key
//! （密码框带小眼睛，编辑时留空表示不改）、默认模型（可输入，旁边的下拉列出crate::i18n::t(EndpointMsg::Test)拉到的和手填的模型）、
//! 额外模型（一行一个）；下面是"识别为……"的能力结果和折叠的crate::i18n::t(EndpointMsg::Advanced)（逐项手动覆盖、改选匹配、
//! OpenAI 兼容的几个开关）；底部有crate::i18n::t(EndpointMsg::Test)按钮。
//!
//! 能力识别、错误分类、保存都是 core 的（`catalog::resolve` / `probe::test_endpoint` /
//! `Database::ai_endpoint_save`），这里是表单校验、结果文案和界面。校验与结果映射是纯函数，
//! 状态测试不用起窗口。

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use ::gpui::{App, Entity, ScrollHandle, SharedString, Subscription, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::ai::agent::builtin::catalog::{
    CapsOverride, Catalog, ModelCaps, PartialCaps, ReasoningSupport, resolve,
};
use zssh_core::ai::agent::builtin::config::{
    AiEndpoint, AiEndpointDraft, EndpointKind, OpenAiOptionsConfig, Tri, valid_base_url,
};
use zssh_core::ai::agent::builtin::probe::EndpointProbe;
use zssh_core::ai::agent::builtin::provider::ApiKey;

use super::MainWindow;
use super::modal::Modal;
use super::widgets::{
    ButtonLook, check_card, error_note, field, note, required_label, segmented_bar, text_button,
};
use crate::agent_setup::builtin::{self, caps_summary, source_label};
use crate::app_theme::Tokens;
use crate::menu_kit;
use crate::typography::{Role, Typo as _};

/// 接口对话框自己的文案。
#[derive(Clone, Copy, Debug)]
enum EndpointMsg {
    KindOpenAi,
    KeyWhitespace,
    KeyControl,
    NameRequired,
    NameTooLong,
    BaseUrlScheme,
    KeyRequiredWithHint,
    KeyRequired,
    ModelInvalid,
    NumberExample,
    NumberZero,
    NumberTooLarge,
    OutputTooLarge,
    NoTextButOk,
    NoModelList,
    NamePlaceholder,
    ModelPlaceholder,
    ExtraModelsPlaceholder,
    Auto,
    Support,
    Unsupported,
    On,
    Off,
    Switch,
    Level,
    Budget,
    SearchModelsPlaceholder,
    EndpointGone,
    TestNeedsModel,
    RuntimeNotStarted,
    EditTitle,
    AddTitle,
    NameLabel,
    TypeLabel,
    KindOpenAiNote,
    KindAnthropicNote,
    HideKey,
    ShowKey,
    KeyKeepHint,
    KeyStoredHint,
    NoKeyLabel,
    NoKeyHint,
    NoKeyCleared,
    Choose,
    ModelPickHint,
    ModelFetchHint,
    DefaultModelLabel,
    DefaultModelHint,
    ExtraModels,
    ExtraModelsHint,
    Advanced,
    TestingLong,
    Testing,
    Test,
    Cancel,
    Save,
    CapsHint,
    NeedDefaultModel,
    ContextWindow,
    ContextHint,
    MaxOutput,
    Reasoning,
    ReasoningHint,
    ToolCalls,
    ImageInput,
    MaxCompletionTokens,
    MaxCompletionHint,
    ReasoningContent,
    ReasoningContentHint,
    ThinkingSwitch,
    ThinkingSwitchHint,
    IncludeUsage,
    IncludeUsageHint,
    ManualDefaultSuffix,
    ManualOverrideSuffix,
    OverrideModel,
    OverrideHint,
    Remap,
    ClearRemap,
    BaseUrlRequiredPrefix,
    ContextWindowPrefix,
    MaxOutputPrefix,
    SecondsSuffix,
    ReplyPrefix,
    ReplyClose,
    TestPassedPrefix,
    TestPassedMid,
    RecognizedPrefix,
    RecognizedMid,
    RecognizedClose,
    TestFailedPrefix,
    ServerSaidPrefix,
    ErrorCodePrefix,
    ModelsReturnedPrefix,
    ModelsReturnedMid,
    ModelCountSuffix,
    SettingsDbPrefix,
    SavedPrefix,
    SavedClose,
    BaseUrlFirstPrefix,
    BaseUrlFirstClose,
    TestAbortedPrefix,
    ExamplePrefix,
    ExampleDot,
    Recognized2Prefix,
    Recognized2Close,
    ManualForPrefix,
    ManualForSuffix,
    RemapPrefix,
    RemapSuffix,
    RemappedPrefix,
}

impl crate::i18n::Msg for EndpointMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::KindOpenAi => "OpenAI 兼容",
            Self::KeyWhitespace => "API Key 里有空格或换行，检查一下是不是复制多了",
            Self::KeyControl => "API Key 里有看不见的控制字符",
            Self::NameRequired => "请填一个接口名称",
            Self::NameTooLong => "名称不能超过 100 个字符",
            Self::BaseUrlScheme => "Base URL 要以 http:// 或 https:// 开头，不能含空格",
            Self::KeyRequiredWithHint => {
                "请填 API Key；接口不需要 Key 的话（比如本机的 Ollama）勾选「不需要 Key」"
            }
            Self::KeyRequired => "请填 API Key",
            Self::ModelInvalid => "默认模型名不合法",
            Self::NumberExample => "要填数字，例如 128000 或 128K",
            Self::NumberZero => "不能是 0",
            Self::NumberTooLarge => "数字太大了",
            Self::OutputTooLarge => "最大输出不能比上下文窗口还大",
            Self::NoTextButOk => "模型没有输出文字（可能把额度花在思考上了），但接口能用",
            Self::NoModelList => "接口没有提供模型列表（或取不到）：模型名手填就行",
            Self::NamePlaceholder => "例如：我的 OpenAI 网关",
            Self::ModelPlaceholder => "例如 gpt-5.5",
            Self::ExtraModelsPlaceholder => "接口的模型列表里没有的，一行填一个",
            Self::Auto => "自动",
            Self::Support => "支持",
            Self::Unsupported => "不支持",
            Self::On => "开",
            Self::Off => "关",
            Self::Switch => "开关",
            Self::Level => "等级",
            Self::Budget => "预算",
            Self::SearchModelsPlaceholder => "搜索 models.dev 的模型…",
            Self::EndpointGone => "这个接口已经不存在了",
            Self::TestNeedsModel => {
                "先填一个默认模型再测试（不知道填什么可以先留空点测试看接口返回的清单）"
            }
            Self::RuntimeNotStarted => "后台运行时没起来，不能测试",
            Self::EditTitle => "编辑接口",
            Self::AddTitle => "添加接口",
            Self::NameLabel => "名称",
            Self::TypeLabel => "类型",
            Self::KindOpenAiNote => {
                "通常带 /v1 这样的版本段；网关、DeepSeek、OpenRouter、Ollama 等都选这一种。"
            }
            Self::KindAnthropicNote => "官方或兼容 Anthropic Messages 的网关。",
            Self::HideKey => "隐藏 Key",
            Self::ShowKey => "显示 Key",
            Self::KeyKeepHint => "留空表示不改；Key 加密存在本机凭据保险库里，不会写进日志",
            Self::KeyStoredHint => "加密存在本机凭据保险库里，不会写进日志",
            Self::NoKeyLabel => "不需要 Key",
            Self::NoKeyHint => {
                "Ollama、vLLM、本机网关这类不校验 Key 的接口勾上；请求里就不带 Authorization 头"
            }
            Self::NoKeyCleared => "已设为不需要 Key。保存时，这个接口原来存的 Key 会一起清掉。",
            Self::Choose => "选择",
            Self::ModelPickHint => "从接口返回的和手填的模型里选",
            Self::ModelFetchHint => "点底部的「测试」可以拉取接口的模型列表",
            Self::DefaultModelLabel => "默认模型",
            Self::DefaultModelHint => "可以直接输入，也可以从右边的下拉里选",
            Self::ExtraModels => "额外模型",
            Self::ExtraModelsHint => "接口的模型列表里没有的；一行一个",
            Self::Advanced => "高级",
            Self::TestingLong => "正在测试…（拉模型列表，再对默认模型发一个很短的请求）",
            Self::Testing => "测试中…",
            Self::Test => "测试",
            Self::Cancel => "取消",
            Self::Save => "保存",
            Self::CapsHint => {
                "填了默认模型后，这里会显示识别出的能力（上下文窗口、思考、工具调用、图片）。"
            }
            Self::NeedDefaultModel => "先填默认模型，才能给它手动改能力。",
            Self::ContextWindow => "上下文窗口",
            Self::ContextHint => "token，如 128000 或 128K",
            Self::MaxOutput => "最大输出",
            Self::Reasoning => "思考",
            Self::ReasoningHint => "等级默认 low / medium / high，预算默认 1024 到 32000 token",
            Self::ToolCalls => "工具调用",
            Self::ImageInput => "图片输入",
            Self::MaxCompletionTokens => "输出上限用 max_completion_tokens",
            Self::MaxCompletionHint => "自动：按接口 / 模型判断；关 = 用 max_tokens",
            Self::ReasoningContent => "回传 reasoning_content",
            Self::ReasoningContentHint => "DeepSeek 等要求工具调用后把思考内容原样带回去",
            Self::ThinkingSwitch => "thinking 开关",
            Self::ThinkingSwitchHint => "发 thinking: {type} 来开关思考（DeepSeek 风格）",
            Self::IncludeUsage => "请求带 include_usage",
            Self::IncludeUsageHint => {
                "让接口在流的末尾回报 token 用量，上下文指示器靠它；个别网关不认这个参数时关掉"
            }
            Self::ManualDefaultSuffix => "（默认模型）",
            Self::ManualOverrideSuffix => " · 已有手动值",
            Self::OverrideModel => "要覆盖的模型",
            Self::OverrideHint => "默认就是上面的默认模型；也可以给接口里别的模型单独改",
            Self::Remap => "改选匹配",
            Self::ClearRemap => "清除改选",
            Self::BaseUrlRequiredPrefix => "请填 Base URL，例如 ",
            Self::ContextWindowPrefix => "上下文窗口：",
            Self::MaxOutputPrefix => "最大输出：",
            Self::SecondsSuffix => " 秒",
            Self::ReplyPrefix => "回复「",
            Self::ReplyClose => "」",
            Self::TestPassedPrefix => "测试通过：用时 ",
            Self::TestPassedMid => " · 模型 ",
            Self::RecognizedPrefix => "识别为 ",
            Self::RecognizedMid => "（来源：",
            Self::RecognizedClose => "）：",
            Self::TestFailedPrefix => "测试失败：",
            Self::ServerSaidPrefix => "服务端返回：",
            Self::ErrorCodePrefix => "错误码：",
            Self::ModelsReturnedPrefix => "接口返回了 ",
            Self::ModelsReturnedMid => "，可以在「默认模型」旁的下拉里选",
            Self::ModelCountSuffix => " 个模型",
            Self::SettingsDbPrefix => "打开设置库失败：",
            Self::SavedPrefix => "已保存接口「",
            Self::SavedClose => "」",
            Self::BaseUrlFirstPrefix => "先填好 Base URL（要以 http:// 或 https:// 开头，例如 ",
            Self::BaseUrlFirstClose => "）",
            Self::TestAbortedPrefix => "测试没能进行：",
            Self::ExamplePrefix => "示例：",
            Self::ExampleDot => "。",
            Self::Recognized2Prefix => "识别为：",
            Self::Recognized2Close => "）",
            Self::ManualForPrefix => "下面的手动值只对「",
            Self::ManualForSuffix => "」生效，优先于自动识别；留空 / 选「自动」就用识别结果。",
            Self::RemapPrefix => "「",
            Self::RemapSuffix => "」识别得不对时，从 models.dev 里选正确的那个",
            Self::RemappedPrefix => "已改选：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::KindOpenAi => "OpenAI-compatible",
            Self::KeyWhitespace => {
                "The API key contains spaces or line breaks — check for extra characters"
            }
            Self::KeyControl => "The API key contains invisible control characters",
            Self::NameRequired => "Enter a name for the endpoint",
            Self::NameTooLong => "The name cannot exceed 100 characters",
            Self::BaseUrlScheme => {
                "The Base URL must start with http:// or https:// and must not contain spaces"
            }
            Self::KeyRequiredWithHint => {
                "Enter the API key; for endpoints that do not need one (for example a local Ollama), tick \"No API key\""
            }
            Self::KeyRequired => "Enter the API key",
            Self::ModelInvalid => "The default model name is invalid",
            Self::NumberExample => "Enter a number, for example 128000 or 128K",
            Self::NumberZero => "Cannot be 0",
            Self::NumberTooLarge => "The number is too large",
            Self::OutputTooLarge => "Max output cannot exceed the context window",
            Self::NoTextButOk => {
                "The model returned no text (it may have spent the budget on reasoning), but the endpoint works"
            }
            Self::NoModelList => {
                "The endpoint did not provide a model list (or it could not be fetched) — type the model name manually"
            }
            Self::NamePlaceholder => "For example: My OpenAI gateway",
            Self::ModelPlaceholder => "For example: gpt-5.5",
            Self::ExtraModelsPlaceholder => "Models missing from the endpoint list, one per line",
            Self::Auto => "Auto",
            Self::Support => "Supported",
            Self::Unsupported => "Not supported",
            Self::On => "On",
            Self::Off => "Off",
            Self::Switch => "Switch",
            Self::Level => "Level",
            Self::Budget => "Budget",
            Self::SearchModelsPlaceholder => "Search models.dev…",
            Self::EndpointGone => "This endpoint no longer exists",
            Self::TestNeedsModel => {
                "Fill in a default model before testing (leave it empty and click Test to see the endpoint list)"
            }
            Self::RuntimeNotStarted => {
                "The background runtime is not running, so the test cannot start"
            }
            Self::EditTitle => "Edit Endpoint",
            Self::AddTitle => "Add Endpoint",
            Self::NameLabel => "Name",
            Self::TypeLabel => "Type",
            Self::KindOpenAiNote => {
                "Usually includes a version segment such as /v1; use this for gateways, DeepSeek, OpenRouter, Ollama and similar."
            }
            Self::KindAnthropicNote => {
                "The official API, or a gateway compatible with Anthropic Messages."
            }
            Self::HideKey => "Hide key",
            Self::ShowKey => "Show key",
            Self::KeyKeepHint => {
                "Leave empty to keep it; the key is stored encrypted in the local credential vault and never written to logs"
            }
            Self::KeyStoredHint => {
                "Stored encrypted in the local credential vault and never written to logs"
            }
            Self::NoKeyLabel => "No API key",
            Self::NoKeyHint => {
                "Tick for endpoints that do not check an API key (Ollama, vLLM, local gateways); requests then carry no Authorization header"
            }
            Self::NoKeyCleared => {
                "Set to require no key. Saving will also clear the key previously stored for this endpoint."
            }
            Self::Choose => "Choose",
            Self::ModelPickHint => {
                "Pick from the models the endpoint returned and the ones you typed"
            }
            Self::ModelFetchHint => "Click Test at the bottom to fetch the endpoint model list",
            Self::DefaultModelLabel => "Default model",
            Self::DefaultModelHint => {
                "Type it directly, or pick one from the dropdown on the right"
            }
            Self::ExtraModels => "Extra models",
            Self::ExtraModelsHint => "Models missing from the endpoint list; one per line",
            Self::Advanced => "Advanced",
            Self::TestingLong => {
                "Testing… (fetching the model list, then sending a very short request to the default model)"
            }
            Self::Testing => "Testing…",
            Self::Test => "Test",
            Self::Cancel => "Cancel",
            Self::Save => "Save",
            Self::CapsHint => {
                "Once a default model is set, the detected capabilities (context window, reasoning, tool calls, images) appear here."
            }
            Self::NeedDefaultModel => "Set a default model before overriding its capabilities.",
            Self::ContextWindow => "Context window",
            Self::ContextHint => "tokens, for example 128000 or 128K",
            Self::MaxOutput => "Max output",
            Self::Reasoning => "Reasoning",
            Self::ReasoningHint => {
                "Level defaults to low / medium / high; budget defaults to 1024–32000 tokens"
            }
            Self::ToolCalls => "Tool calls",
            Self::ImageInput => "Image input",
            Self::MaxCompletionTokens => "Use max_completion_tokens for the output limit",
            Self::MaxCompletionHint => "Auto: decide per endpoint and model; off = use max_tokens",
            Self::ReasoningContent => "Send reasoning_content back",
            Self::ReasoningContentHint => {
                "DeepSeek and similar require the reasoning content to be sent back verbatim after tool calls"
            }
            Self::ThinkingSwitch => "thinking switch",
            Self::ThinkingSwitchHint => {
                "Send thinking: {type} to toggle reasoning (DeepSeek style)"
            }
            Self::IncludeUsage => "Request include_usage",
            Self::IncludeUsageHint => {
                "Ask the endpoint to report token usage at the end of the stream; the context indicator relies on it. Turn this off for gateways that reject the parameter."
            }
            Self::ManualDefaultSuffix => " (default model)",
            Self::ManualOverrideSuffix => " · manual values set",
            Self::OverrideModel => "Model to override",
            Self::OverrideHint => {
                "Defaults to the default model above; you can also override another model of this endpoint"
            }
            Self::Remap => "Remap",
            Self::ClearRemap => "Clear remap",
            Self::BaseUrlRequiredPrefix => "Enter the Base URL, for example ",
            Self::ContextWindowPrefix => "Context window: ",
            Self::MaxOutputPrefix => "Max output: ",
            Self::SecondsSuffix => "s",
            Self::ReplyPrefix => "Reply \"",
            Self::ReplyClose => "\"",
            Self::TestPassedPrefix => "Passed in ",
            Self::TestPassedMid => " · model ",
            Self::RecognizedPrefix => "Detected ",
            Self::RecognizedMid => " (source: ",
            Self::RecognizedClose => "): ",
            Self::TestFailedPrefix => "Test failed: ",
            Self::ServerSaidPrefix => "Server returned: ",
            Self::ErrorCodePrefix => "Error code: ",
            Self::ModelsReturnedPrefix => "The endpoint returned ",
            Self::ModelsReturnedMid => "; pick one from the dropdown next to \"Default model\"",
            Self::ModelCountSuffix => "models",
            Self::SettingsDbPrefix => "Failed to open the settings database: ",
            Self::SavedPrefix => "Saved endpoint \"",
            Self::SavedClose => "\"",
            Self::BaseUrlFirstPrefix => {
                "Fill in the Base URL first (it must start with http:// or https://, for example "
            }
            Self::BaseUrlFirstClose => ")",
            Self::TestAbortedPrefix => "The test could not run: ",
            Self::ExamplePrefix => "Example: ",
            Self::ExampleDot => ". ",
            Self::Recognized2Prefix => "Detected: ",
            Self::Recognized2Close => ")",
            Self::ManualForPrefix => "The manual values below apply only to \"",
            Self::ManualForSuffix => {
                "\" and take precedence over auto-detection; leave them empty or choose \"Auto\" to use the detected values."
            }
            Self::RemapPrefix => "\"",
            Self::RemapSuffix => "\" was detected incorrectly — pick the right one from models.dev",
            Self::RemappedPrefix => "Remapped to: ",
        }
    }
}

/// 对话框卡片宽度：和主机属性一样的单栏表单。
pub const DIALOG_WIDTH: f32 = super::host_dialog::DIALOG_WIDTH;

/// 示例地址（表单占位和提示用，文档保留域名）。
pub const OPENAI_EXAMPLE_URL: &str = "https://api.example.com/v1";
pub const ANTHROPIC_EXAMPLE_URL: &str = "https://api.anthropic.com";

pub fn example_url(kind: EndpointKind) -> &'static str {
    match kind {
        EndpointKind::OpenAiChat => OPENAI_EXAMPLE_URL,
        EndpointKind::Anthropic => ANTHROPIC_EXAMPLE_URL,
    }
}

pub fn kind_label(kind: EndpointKind) -> &'static str {
    match kind {
        EndpointKind::OpenAiChat => crate::i18n::t(EndpointMsg::KindOpenAi),
        EndpointKind::Anthropic => "Anthropic",
    }
}

// ---------------------------------------------------------------------------
// 表单校验（纯函数）
// ---------------------------------------------------------------------------

/// 表单里出错的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    BaseUrl,
    ApiKey,
    DefaultModel,
    Overrides,
    General,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormError {
    pub field: Field,
    pub message: String,
}

impl FormError {
    fn new(field: Field, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// 表单里填的值（借用输入框里读出来的文字）。
pub struct FormValues<'a> {
    pub id: Option<i64>,
    pub preset_id: Option<String>,
    pub kind: EndpointKind,
    pub name: &'a str,
    pub base_url: &'a str,
    pub api_key: &'a str,
    /// 库里这个接口已经有 Key（编辑时留空 = 不改）。
    pub has_stored_key: bool,
    pub default_model: &'a str,
    pub extra_models: &'a str,
    pub options: OpenAiOptionsConfig,
    pub overrides: BTreeMap<String, CapsOverride>,
}

/// Key 里不能有空白和控制字符（多半是复制时带进来的换行 / 空格）。
fn key_problem(key: &str) -> Option<&'static str> {
    if key.chars().any(char::is_whitespace) {
        return Some(crate::i18n::t(EndpointMsg::KeyWhitespace));
    }
    if key.chars().any(char::is_control) {
        return Some(crate::i18n::t(EndpointMsg::KeyControl));
    }
    None
}

/// 校验并拼出要保存的草稿。Key：新建必填；编辑时库里已有就可以留空（不改）。
pub fn build_draft(values: &FormValues<'_>) -> Result<AiEndpointDraft, FormError> {
    let name = values.name.trim();
    if name.is_empty() {
        return Err(FormError::new(
            Field::Name,
            crate::i18n::t(EndpointMsg::NameRequired),
        ));
    }
    if name.chars().count() > 100 {
        return Err(FormError::new(
            Field::Name,
            crate::i18n::t(EndpointMsg::NameTooLong),
        ));
    }
    let url = values.base_url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err(FormError::new(
            Field::BaseUrl,
            format!(
                "{}{}",
                crate::i18n::t(EndpointMsg::BaseUrlRequiredPrefix),
                example_url(values.kind)
            ),
        ));
    }
    if !valid_base_url(url) {
        return Err(FormError::new(
            Field::BaseUrl,
            crate::i18n::t(EndpointMsg::BaseUrlScheme),
        ));
    }
    // 「不需要 Key」只有 OpenAI 兼容接口能选（Ollama、vLLM、本机网关）；Anthropic 一律要 Key。
    let no_key = values.kind == EndpointKind::OpenAiChat && values.options.no_key;
    let typed_key = if no_key { "" } else { values.api_key.trim() };
    let needs_key = !no_key && (values.id.is_none() || !values.has_stored_key);
    if typed_key.is_empty() && needs_key {
        return Err(FormError::new(
            Field::ApiKey,
            match values.kind {
                EndpointKind::OpenAiChat => crate::i18n::t(EndpointMsg::KeyRequiredWithHint),
                EndpointKind::Anthropic => crate::i18n::t(EndpointMsg::KeyRequired),
            },
        ));
    }
    if let Some(problem) = key_problem(typed_key) {
        return Err(FormError::new(Field::ApiKey, problem));
    }
    let default_model = values.default_model.trim();
    if default_model.chars().count() > 200 || default_model.chars().any(char::is_control) {
        return Err(FormError::new(
            Field::DefaultModel,
            crate::i18n::t(EndpointMsg::ModelInvalid),
        ));
    }
    let models: Vec<String> = values
        .extra_models
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(AiEndpointDraft {
        id: values.id,
        name: name.to_owned(),
        kind: values.kind,
        base_url: url.to_owned(),
        // 明确不需要 Key：空串 = 把库里已存的 Key 一起清掉。
        api_key: if no_key {
            Some(String::new())
        } else {
            (!typed_key.is_empty()).then(|| typed_key.to_owned())
        },
        default_model: (!default_model.is_empty()).then(|| default_model.to_owned()),
        models,
        overrides: values.overrides.clone(),
        openai_options: OpenAiOptionsConfig {
            no_key,
            ..values.options.clone()
        },
        preset_id: values.preset_id.clone(),
    })
}

// ---------------------------------------------------------------------------
// 手动覆盖 ↔ 表单项（纯函数）
// ---------------------------------------------------------------------------

/// 一项能力的手动覆盖：自动 / 强制支持 / 强制不支持。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Force {
    #[default]
    Auto,
    Yes,
    No,
}

impl Force {
    pub fn from_option(value: Option<bool>) -> Self {
        match value {
            None => Force::Auto,
            Some(true) => Force::Yes,
            Some(false) => Force::No,
        }
    }

    pub fn to_option(self) -> Option<bool> {
        match self {
            Force::Auto => None,
            Force::Yes => Some(true),
            Force::No => Some(false),
        }
    }
}

/// 覆盖表单的内容（对应当前选中的那个模型）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverrideFields {
    pub context: String,
    pub output: String,
    pub tools: Force,
    pub image: Force,
    /// `None` = 自动。
    pub thinking: Option<ReasoningSupport>,
    /// 改选匹配（models.dev 里的条目键）。
    pub pick: Option<String>,
}

impl OverrideFields {
    pub fn from_override(item: &CapsOverride) -> Self {
        Self {
            context: item
                .context_window
                .map(|n| n.to_string())
                .unwrap_or_default(),
            output: item.max_output.map(|n| n.to_string()).unwrap_or_default(),
            tools: Force::from_option(item.tool_call),
            image: Force::from_option(item.image_input),
            thinking: item.reasoning.clone(),
            pick: item.pick.clone(),
        }
    }
}

/// 把 `128000` / `128k` / `1M` 读成数字；空串是 `None`（不覆盖）。
pub fn parse_tokens(text: &str) -> Result<Option<u32>, String> {
    let text = text.trim().replace([',', '_', ' '], "");
    if text.is_empty() {
        return Ok(None);
    }
    let lower = text.to_ascii_lowercase();
    let (digits, factor) = match lower.chars().last() {
        Some('k') => (&lower[..lower.len() - 1], 1_000u64),
        Some('m') => (&lower[..lower.len() - 1], 1_000_000u64),
        _ => (lower.as_str(), 1),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| crate::i18n::t(EndpointMsg::NumberExample).to_owned())?;
    let total = value.saturating_mul(factor);
    if total == 0 {
        return Err(crate::i18n::t(EndpointMsg::NumberZero).to_owned());
    }
    u32::try_from(total)
        .map(Some)
        .map_err(|_| crate::i18n::t(EndpointMsg::NumberTooLarge).to_owned())
}

/// 表单项 → `CapsOverride`（全空 = 默认值，调用方据此决定要不要存）。
pub fn override_from_fields(fields: &OverrideFields) -> Result<CapsOverride, FormError> {
    let context_window = parse_tokens(&fields.context).map_err(|e| {
        FormError::new(
            Field::Overrides,
            format!("{}{e}", crate::i18n::t(EndpointMsg::ContextWindowPrefix)),
        )
    })?;
    let max_output = parse_tokens(&fields.output).map_err(|e| {
        FormError::new(
            Field::Overrides,
            format!("{}{e}", crate::i18n::t(EndpointMsg::MaxOutputPrefix)),
        )
    })?;
    if let (Some(window), Some(output)) = (context_window, max_output)
        && output > window
    {
        return Err(FormError::new(
            Field::Overrides,
            crate::i18n::t(EndpointMsg::OutputTooLarge),
        ));
    }
    Ok(CapsOverride {
        context_window,
        max_output,
        tool_call: fields.tools.to_option(),
        image_input: fields.image.to_option(),
        reasoning: fields.thinking.clone(),
        pick: fields.pick.clone(),
        ..CapsOverride::default()
    })
}

/// 把当前模型的覆盖写回整张表：全空就删掉这一项，别的模型的覆盖不动。
pub fn store_override(
    overrides: &mut BTreeMap<String, CapsOverride>,
    model: &str,
    item: CapsOverride,
) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    if item == CapsOverride::default() {
        overrides.remove(model);
    } else {
        overrides.insert(model.to_owned(), item);
    }
}

/// 手动覆盖现在对应哪个模型：专门选过别的就是它，否则跟着默认模型。
pub fn override_target(pick: Option<&str>, default: &str) -> String {
    pick.unwrap_or(default).to_owned()
}

/// 用户在「要覆盖的模型」里选了 `model`：选的就是默认模型（或空）时回到「跟着默认模型」。
pub fn normalize_pick(model: &str, default: &str) -> Option<String> {
    let model = model.trim();
    (!model.is_empty() && model != default.trim()).then(|| model.to_owned())
}

/// 切换覆盖目标：把正在编辑的旧模型的覆盖存回整张表（`old_item` 为 `None` = 表单里的数字读不懂，旧值不动），
/// 再返回新目标已有的覆盖（没有就是全空）。别的模型的覆盖一概原样保留。
pub fn switch_override_target(
    overrides: &mut BTreeMap<String, CapsOverride>,
    old_model: &str,
    old_item: Option<CapsOverride>,
    new_model: &str,
) -> CapsOverride {
    if let Some(item) = old_item {
        store_override(overrides, old_model, item);
    }
    overrides.get(new_model.trim()).cloned().unwrap_or_default()
}

/// 思考覆盖的几个常用档（分段按钮用）。`None` = 自动。
pub fn thinking_choices() -> [&'static str; 5] {
    [
        crate::i18n::t(EndpointMsg::Auto),
        crate::i18n::t(EndpointMsg::Unsupported),
        crate::i18n::t(EndpointMsg::Switch),
        crate::i18n::t(EndpointMsg::Level),
        crate::i18n::t(EndpointMsg::Budget),
    ]
}

pub fn thinking_choice_index(value: &Option<ReasoningSupport>) -> usize {
    match value {
        None => 0,
        Some(ReasoningSupport::None) => 1,
        Some(ReasoningSupport::Toggle) => 2,
        Some(ReasoningSupport::Levels(_)) => 3,
        Some(ReasoningSupport::Budget { .. }) => 4,
    }
}

pub fn thinking_from_choice(
    index: usize,
    current: &Option<ReasoningSupport>,
) -> Option<ReasoningSupport> {
    // 点的就是已经选着的这一档：保留它里面的细节（等级清单、预算区间）。
    if thinking_choice_index(current) == index {
        return current.clone();
    }
    match index {
        1 => Some(ReasoningSupport::None),
        2 => Some(ReasoningSupport::Toggle),
        3 => Some(ReasoningSupport::Levels(vec![
            "low".to_owned(),
            "medium".to_owned(),
            "high".to_owned(),
        ])),
        4 => Some(ReasoningSupport::Budget {
            min: 1024,
            max: 32_000,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 测试结果文案（纯函数）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestResult {
    pub ok: bool,
    /// 第一行是结论，后面是补充（识别结果、模型清单、服务端原文）。
    pub lines: Vec<String>,
}

fn seconds_text(elapsed: Duration) -> String {
    format!(
        "{:.1}{}",
        elapsed.as_secs_f64(),
        crate::i18n::t(EndpointMsg::SecondsSuffix)
    )
}

/// 一行能力摘要：上下文 · 最大输出 · 思考 · 工具 · 图片。
pub fn caps_one_line(caps: &ModelCaps) -> String {
    caps_summary(caps)
        .into_iter()
        .map(|(name, value)| format!("{name} {value}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 探测结果 → 界面上的结果行。成功：用时、模型、回复、识别出的能力、模型清单情况；
/// 失败：A2 的中文原因，加服务端原文和错误码。
pub fn describe_probe(probe: &EndpointProbe, model: &str, caps: &ModelCaps) -> TestResult {
    let mut lines = Vec::new();
    match &probe.reply {
        Ok(reply) => {
            let said = if reply.text.is_empty() {
                crate::i18n::t(EndpointMsg::NoTextButOk).to_owned()
            } else {
                format!(
                    "{}{}{}",
                    crate::i18n::t(EndpointMsg::ReplyPrefix),
                    reply.text,
                    crate::i18n::t(EndpointMsg::ReplyClose)
                )
            };
            lines.push(format!(
                "{}{}{}{model} · {said}",
                crate::i18n::t(EndpointMsg::TestPassedPrefix),
                seconds_text(reply.elapsed),
                crate::i18n::t(EndpointMsg::TestPassedMid)
            ));
            lines.push(format!(
                "{}{}{}{}{}{}",
                crate::i18n::t(EndpointMsg::RecognizedPrefix),
                display_name(caps, model),
                crate::i18n::t(EndpointMsg::RecognizedMid),
                source_label(caps.source),
                crate::i18n::t(EndpointMsg::RecognizedClose),
                caps_one_line(caps)
            ));
        }
        Err(error) => {
            lines.push(format!(
                "{}{}",
                crate::i18n::t(EndpointMsg::TestFailedPrefix),
                error.message
            ));
            if let Some(detail) = &error.detail {
                lines.push(format!(
                    "{}{detail}",
                    crate::i18n::t(EndpointMsg::ServerSaidPrefix)
                ));
            }
            if let Some(code) = &error.code {
                lines.push(format!(
                    "{}{code}",
                    crate::i18n::t(EndpointMsg::ErrorCodePrefix)
                ));
            }
        }
    }
    lines.push(match &probe.models {
        Some(models) => format!(
            "{}{}{}",
            crate::i18n::t(EndpointMsg::ModelsReturnedPrefix),
            crate::i18n::count(
                models.len() as u64,
                crate::i18n::t(EndpointMsg::ModelCountSuffix),
                "model",
                "models"
            ),
            crate::i18n::t(EndpointMsg::ModelsReturnedMid)
        ),
        None => crate::i18n::t(EndpointMsg::NoModelList).to_owned(),
    });
    TestResult {
        ok: probe.ok(),
        lines,
    }
}

fn display_name(caps: &ModelCaps, model: &str) -> String {
    if caps.display_name.trim().is_empty() {
        model.to_owned()
    } else {
        caps.display_name.clone()
    }
}

// ---------------------------------------------------------------------------
// 对话框状态
// ---------------------------------------------------------------------------

/// crate::i18n::t(EndpointMsg::Test)按钮的状态。
#[derive(Default)]
pub struct TestUi {
    /// 每点一次加一；结果回来对不上就作废（中途改了表单重测、关了对话框）。
    pub generation: u64,
    pub running: bool,
    pub result: Option<TestResult>,
}

pub struct AiEndpointDialog {
    pub id: Option<i64>,
    pub preset_id: Option<String>,
    pub kind: EndpointKind,
    pub name: Entity<InputState>,
    pub base_url: Entity<InputState>,
    pub api_key: Entity<InputState>,
    pub key_shown: bool,
    pub has_stored_key: bool,
    pub default_model: Entity<InputState>,
    pub extra_models: Entity<TextareaState>,
    pub options: OpenAiOptionsConfig,
    /// 整张覆盖表（别的模型的覆盖原样保留）。
    pub overrides: BTreeMap<String, CapsOverride>,
    /// 下面这组覆盖表单项对应哪个模型。
    pub override_model: String,
    /// 用户在「要覆盖的模型」里专门选的（`None` = 跟着默认模型）。
    pub override_pick: Option<String>,
    pub cap_context: Entity<InputState>,
    pub cap_output: Entity<InputState>,
    pub cap_fields: OverrideFields,
    pub pick_query: Entity<InputState>,
    pub advanced: bool,
    /// crate::i18n::t(EndpointMsg::Test)拉到的模型清单和接口自报的能力。
    pub remote_models: Vec<String>,
    pub remote: HashMap<String, PartialCaps>,
    pub test: TestUi,
    pub error: Option<FormError>,
    pub scroll: ScrollHandle,
    pub _subs: Vec<Subscription>,
}

impl AiEndpointDialog {
    /// 新建（`existing = None`）或编辑。输入框的变化订阅由调用方（主窗口）接上。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<MainWindow>,
        existing: Option<&AiEndpoint>,
    ) -> Self {
        let kind = existing.map_or(EndpointKind::OpenAiChat, |e| e.kind);
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(existing.map(|e| e.name.clone()).unwrap_or_default())
                .placeholder(crate::i18n::t(EndpointMsg::NamePlaceholder))
        });
        let base_url = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(existing.map(|e| e.base_url.clone()).unwrap_or_default())
                .placeholder(example_url(kind))
        });
        let api_key = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-…"));
        let default_model = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(
                    existing
                        .and_then(|e| e.default_model.clone())
                        .unwrap_or_default(),
                )
                .placeholder(crate::i18n::t(EndpointMsg::ModelPlaceholder))
        });
        let extra = existing.map(|e| e.models.join("\n")).unwrap_or_default();
        let extra_models = cx.new(|cx| {
            TextareaState::new(window, cx)
                .default_value(extra)
                .placeholder(crate::i18n::t(EndpointMsg::ExtraModelsPlaceholder))
                .auto_grow(2, 5)
        });
        let cap_context =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::i18n::t(EndpointMsg::Auto)));
        let cap_output =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::i18n::t(EndpointMsg::Auto)));
        let pick_query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::i18n::t(EndpointMsg::SearchModelsPlaceholder))
        });
        let overrides = existing.map(|e| e.overrides.clone()).unwrap_or_default();
        let mut dialog = Self {
            id: existing.map(|e| e.id),
            preset_id: existing.and_then(|e| e.preset_id.clone()),
            kind,
            name,
            base_url,
            api_key,
            key_shown: false,
            has_stored_key: existing.is_some_and(|e| e.has_key),
            default_model,
            extra_models,
            options: existing
                .map(|e| e.openai_options.clone())
                .unwrap_or_default(),
            overrides,
            override_model: String::new(),
            override_pick: None,
            cap_context,
            cap_output,
            cap_fields: OverrideFields::default(),
            pick_query,
            advanced: false,
            remote_models: Vec::new(),
            remote: HashMap::new(),
            test: TestUi::default(),
            error: None,
            scroll: ScrollHandle::new(),
            _subs: Vec::new(),
        };
        dialog.load_overrides(window, cx);
        dialog
    }

    /// 默认模型输入框里现在的名字。
    pub fn model_text(&self, cx: &App) -> String {
        self.default_model.read(cx).value().trim().to_owned()
    }

    /// 把覆盖表单项里的内容写回整张表（对应 `override_model` 那个模型）。读不懂的数字不写（保存时再报）。
    pub fn commit_override_fields(&mut self, cx: &App) {
        self.cap_fields.context = self.cap_context.read(cx).value().to_string();
        self.cap_fields.output = self.cap_output.read(cx).value().to_string();
        let model = self.override_model.clone();
        if let Ok(item) = override_from_fields(&self.cap_fields) {
            store_override(&mut self.overrides, &model, item);
        }
    }

    /// 覆盖表单现在对应哪个模型：用户在「要覆盖的模型」里专门选过就是它，否则跟着默认模型。
    pub fn override_target(&self, cx: &App) -> String {
        override_target(self.override_pick.as_deref(), &self.model_text(cx))
    }

    /// 默认模型换了（或选了别的要覆盖的模型）：先把旧模型的覆盖存好，再把新目标的覆盖读进表单项。
    pub fn load_overrides(&mut self, window: &mut Window, cx: &mut Context<MainWindow>) {
        let model = self.override_target(cx);
        // 目标没变（比如专门选了别的模型、默认模型又被改了）：表单项原样留着，别用旧值覆盖正在输入的内容。
        if !self.override_model.is_empty() && self.override_model == model {
            return;
        }
        // 旧目标的覆盖存回整张表（表单里数字读不懂就不动旧值），再读新目标的。
        let old_item = self.current_override(cx).ok();
        let old_model = self.override_model.clone();
        let item = switch_override_target(&mut self.overrides, &old_model, old_item, &model);
        self.cap_fields = OverrideFields::from_override(&item);
        let (context, output) = (
            self.cap_fields.context.clone(),
            self.cap_fields.output.clone(),
        );
        self.cap_context
            .update(cx, |input, cx| input.set_value(context, window, cx));
        self.cap_output
            .update(cx, |input, cx| input.set_value(output, window, cx));
        self.override_model = model;
    }

    /// 选「要覆盖的模型」：选的就是默认模型时回到「跟着默认模型」。
    pub fn pick_override_model(
        &mut self,
        model: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) {
        self.override_pick = normalize_pick(model, &self.model_text(cx));
        self.load_overrides(window, cx);
    }

    /// 有手动覆盖的模型（不含正在编辑的那个；界面下拉里也要能点到它们）。
    pub fn models_with_overrides(&self) -> Vec<String> {
        self.overrides.keys().cloned().collect()
    }

    /// 某个模型现在生效的手动覆盖：正在编辑的那个读表单，别的读已存的表；全空 = `None`。
    pub fn manual_for(&self, model: &str, cx: &App) -> Option<CapsOverride> {
        let item = if model == self.override_model {
            self.current_override(cx).ok()?
        } else {
            self.overrides.get(model).cloned()?
        };
        (item != CapsOverride::default()).then_some(item)
    }

    /// 现在表单里的覆盖（对应 `override_model` 那个模型）；数字读不懂时是 `Err`。
    pub fn current_override(&self, cx: &App) -> Result<CapsOverride, FormError> {
        let mut fields = self.cap_fields.clone();
        fields.context = self.cap_context.read(cx).value().to_string();
        fields.output = self.cap_output.read(cx).value().to_string();
        override_from_fields(&fields)
    }

    /// 保存用的整张覆盖表。
    pub fn overrides_for_save(
        &self,
        cx: &App,
    ) -> Result<BTreeMap<String, CapsOverride>, FormError> {
        let mut all = self.overrides.clone();
        store_override(&mut all, &self.override_model, self.current_override(cx)?);
        Ok(all)
    }

    /// 读表单 → 草稿。
    pub fn draft(&self, cx: &App) -> Result<AiEndpointDraft, FormError> {
        let overrides = self.overrides_for_save(cx)?;
        let name = self.name.read(cx).value().to_string();
        let base_url = self.base_url.read(cx).value().to_string();
        let api_key = self.api_key.read(cx).value().to_string();
        let default_model = self.default_model.read(cx).value().to_string();
        let extra_models = self.extra_models.read(cx).value().to_string();
        build_draft(&FormValues {
            id: self.id,
            preset_id: self.preset_id.clone(),
            kind: self.kind,
            name: &name,
            base_url: &base_url,
            api_key: &api_key,
            has_stored_key: self.has_stored_key,
            default_model: &default_model,
            extra_models: &extra_models,
            options: self.options.clone(),
            overrides,
        })
    }

    /// 能选的模型：测试拉到的 + 手填的（默认模型 + 额外模型），去重。
    pub fn model_choices(&self, cx: &App) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |name: &str| {
            let name = name.trim();
            if !name.is_empty() && !out.iter().any(|known| known == name) {
                out.push(name.to_owned());
            }
        };
        for name in &self.remote_models {
            push(name);
        }
        push(&self.model_text(cx));
        for line in self.extra_models.read(cx).value().lines() {
            push(line);
        }
        out
    }

    /// 当前默认模型的能力（带上手动覆盖和接口自报的）。
    pub fn caps_now(&self, catalog: &Catalog, cx: &App) -> Option<(String, ModelCaps)> {
        let model = self.model_text(cx);
        if model.is_empty() {
            return None;
        }
        let manual = self.manual_for(&model, cx);
        let reported = self.remote.get(&model).cloned();
        let caps = resolve(&model, manual, reported, catalog);
        Some((model, caps))
    }

    /// 勾了「不需要 Key」（只有 OpenAI 兼容接口算数）。
    pub fn key_not_needed(&self) -> bool {
        self.kind == EndpointKind::OpenAiChat && self.options.no_key
    }

    /// 测试用的连接配置：表单里新填的 Key 优先，没填用库里已存的。
    pub fn probe_key(&self, cx: &App) -> ApiKey {
        if self.key_not_needed() {
            return ApiKey::default();
        }
        let typed = self.api_key.read(cx).value().trim().to_owned();
        if !typed.is_empty() {
            return ApiKey::new(typed);
        }
        let stored = self.id.and_then(|id| {
            crate::host_db::open_database()
                .ok()
                .and_then(|db| db.ai_endpoint_api_key(id).ok().flatten())
        });
        ApiKey::new(stored.unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// 界面
// ---------------------------------------------------------------------------

fn fold_header(
    id: &'static str,
    title: &str,
    open: bool,
    tokens: &Tokens,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover = tokens.hover;
    h_flex()
        .id(id)
        .w_full()
        .gap_2()
        .items_center()
        .py_1()
        .cursor_pointer()
        .rounded_md()
        .hover(move |style| style.bg(hover))
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size_4()
            .text_color(tokens.text_muted),
        )
        .child(
            div()
                .typo(Role::FieldLabel, tokens)
                .child(SharedString::from(title.to_owned())),
        )
        .on_click(move |_, window, cx| on_toggle(window, cx))
        .into_any_element()
}

/// 三态（自动 / 开 / 关）的分段条。
fn tri_bar(
    id: &'static str,
    current: Tri,
    labels: [&'static str; 3],
    tokens: &Tokens,
    on_pick: impl Fn(Tri, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let all = [Tri::Auto, Tri::On, Tri::Off];
    segmented_bar(
        id,
        all.iter()
            .zip(labels)
            .map(|(value, label)| (label.to_owned(), *value == current, None))
            .collect(),
        tokens,
        move |index, window, cx| {
            if let Some(value) = all.get(index).copied() {
                on_pick(value, window, cx);
            }
        },
    )
}

fn force_bar(
    id: &'static str,
    current: Force,
    tokens: &Tokens,
    on_pick: impl Fn(Force, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let all = [Force::Auto, Force::Yes, Force::No];
    segmented_bar(
        id,
        [
            crate::i18n::t(EndpointMsg::Auto),
            crate::i18n::t(EndpointMsg::Support),
            crate::i18n::t(EndpointMsg::Unsupported),
        ]
        .iter()
        .zip(all)
        .map(|(label, value)| ((*label).to_owned(), value == current, None))
        .collect(),
        tokens,
        move |index, window, cx| {
            if let Some(value) = all.get(index).copied() {
                on_pick(value, window, cx);
            }
        },
    )
}

impl MainWindow {
    /// 打开crate::i18n::t(EndpointMsg::AddTitle)（`existing = None`）或crate::i18n::t(EndpointMsg::EditTitle)对话框。
    pub(crate) fn open_ai_endpoint_dialog(
        &mut self,
        existing: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_ai_endpoint_dialog_with(existing, None, window, cx);
    }

    /// 同上；`prefill` 是从某个预设来的表单起点（名称 / 类型 / 地址 / 是否不需要 Key / 预设标识）。
    pub(crate) fn open_ai_endpoint_dialog_with(
        &mut self,
        existing: Option<i64>,
        prefill: Option<super::ai_presets::Prefill>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let endpoint = existing.and_then(|id| {
            crate::host_db::open_database()
                .ok()
                .and_then(|db| db.ai_endpoint_get(id).ok())
        });
        if existing.is_some() && endpoint.is_none() {
            self.notice(crate::i18n::t(EndpointMsg::EndpointGone), window, cx);
            return;
        }
        let mut dialog = AiEndpointDialog::new(window, cx, endpoint.as_ref());
        if let Some(prefill) = prefill.filter(|_| endpoint.is_none()) {
            dialog.preset_id = prefill.preset_id;
            dialog.kind = prefill.kind;
            dialog.options.no_key = prefill.no_key && prefill.kind == EndpointKind::OpenAiChat;
            let kind = dialog.kind;
            dialog
                .name
                .update(cx, |input, cx| input.set_value(prefill.name, window, cx));
            dialog.base_url.update(cx, |input, cx| {
                input.set_value(prefill.base_url, window, cx);
                input.set_placeholder(example_url(kind), window, cx);
            });
        }
        // 默认模型一变，覆盖表单项要换成新模型的；其余输入框变了只需要重画（识别结果是实时算的）。
        dialog._subs.push(cx.subscribe_in(
            &dialog.default_model.clone(),
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.ai_dialog_model_changed(window, cx);
                }
            },
        ));
        // 地址、Key 一改，上一次的测试结果就不作数了；名称等一改，旧的校验错误也该消失。
        for input in [dialog.base_url.clone(), dialog.api_key.clone()] {
            dialog._subs.push(cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.with_ai_dialog(cx, |dialog| {
                            dialog.test.result = None;
                            dialog.error = None;
                        });
                    }
                },
            ));
        }
        dialog._subs.push(cx.subscribe_in(
            &dialog.name.clone(),
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.with_ai_dialog(cx, |dialog| dialog.error = None);
                }
            },
        ));
        for input in [
            dialog.cap_context.clone(),
            dialog.cap_output.clone(),
            dialog.pick_query.clone(),
        ] {
            dialog._subs.push(cx.subscribe_in(
                &input,
                window,
                |_this, _input, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
        }
        self.modal = Some(Modal::AiEndpoint(Box::new(dialog)));
        cx.notify();
    }

    pub(crate) fn ai_dialog_model_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::AiEndpoint(mut dialog)) = self.modal.take() else {
            return;
        };
        dialog.load_overrides(window, cx);
        // 测试结果说的是改之前的那个模型，留着会误导；对应的校验错误也不作数了。
        dialog.test.result = None;
        dialog.error = None;
        self.modal = Some(Modal::AiEndpoint(dialog));
        cx.notify();
    }

    /// 改对话框里的一个状态值（按钮点击的统一入口）。
    pub(crate) fn with_ai_dialog(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut AiEndpointDialog),
    ) {
        if let Some(Modal::AiEndpoint(dialog)) = &mut self.modal {
            change(dialog);
            cx.notify();
        }
    }

    /// 换接口类型：占位示例地址跟着换；地址框没填过就不动内容。
    pub(crate) fn ai_dialog_set_kind(
        &mut self,
        kind: EndpointKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::AiEndpoint(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.kind == kind {
            return;
        }
        dialog.kind = kind;
        // Anthropic 一定要 Key：换过去时「不需要 Key」自动取消。
        if kind == EndpointKind::Anthropic {
            dialog.options.no_key = false;
        }
        // 两种类型的示例地址不同：地址框还是空的（或还是另一种的示例）就换成新示例的占位。
        dialog.base_url.update(cx, |input, cx| {
            input.set_placeholder(example_url(kind), window, cx);
        });
        dialog.test.result = None;
        cx.notify();
    }

    pub(crate) fn ai_dialog_toggle_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::AiEndpoint(dialog)) = &mut self.modal else {
            return;
        };
        dialog.key_shown = !dialog.key_shown;
        let shown = dialog.key_shown;
        dialog
            .api_key
            .update(cx, |input, cx| input.set_masked(!shown, window, cx));
        cx.notify();
    }

    /// 锁屏时把显示着的 API Key 重新遮住（V2-C9）。框里的字原样保留，只切换显示方式。
    pub(crate) fn remask_ai_dialog_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::AiEndpoint(dialog)) = &mut self.modal else {
            return;
        };
        if !dialog.key_shown {
            return;
        }
        dialog.key_shown = false;
        dialog
            .api_key
            .update(cx, |input, cx| input.set_masked(true, window, cx));
        cx.notify();
    }

    /// 选了一个模型：填进crate::i18n::t(EndpointMsg::DefaultModelLabel)。
    pub(crate) fn ai_dialog_pick_model(
        &mut self,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::AiEndpoint(dialog)) = &self.modal else {
            return;
        };
        let input = dialog.default_model.clone();
        input.update(cx, |input, cx| input.set_value(model, window, cx));
        // 程序里改输入框内容不一定发"变了"事件：覆盖项、测试结果要跟着新模型换。
        self.ai_dialog_model_changed(window, cx);
    }

    /// 保存。
    pub(crate) fn ai_endpoint_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::AiEndpoint(dialog)) = &mut self.modal else {
            return;
        };
        dialog.commit_override_fields(cx);
        let draft = match dialog.draft(cx) {
            Ok(draft) => draft,
            Err(error) => {
                dialog.error = Some(error);
                cx.notify();
                return;
            }
        };
        let saved = crate::host_db::open_database()
            .map_err(|err| format!("{}{err}", crate::i18n::t(EndpointMsg::SettingsDbPrefix)))
            .and_then(|db| {
                db.ai_endpoint_save(&draft)
                    .map_err(|err| crate::error_text::error_text(&err))
            });
        match saved {
            Ok(endpoint) => {
                builtin::remote_forget(&endpoint.uid);
                builtin::reload();
                self.ai_builtin.reload_rows();
                self.modal = None;
                self.focus_content(window, cx);
                self.notice(
                    &format!(
                        "{}{}{}",
                        crate::i18n::t(EndpointMsg::SavedPrefix),
                        endpoint.name,
                        crate::i18n::t(EndpointMsg::SavedClose)
                    ),
                    window,
                    cx,
                );
            }
            Err(message) => {
                if let Some(Modal::AiEndpoint(dialog)) = &mut self.modal {
                    dialog.error = Some(FormError::new(Field::General, message));
                }
                cx.notify();
            }
        }
    }

    /// crate::i18n::t(EndpointMsg::Test)：用表单当前的值（没保存的也行）拉模型清单并对默认模型发一个极短的请求。
    pub(crate) fn ai_endpoint_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::AiEndpoint(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.test.running {
            return;
        }
        dialog.commit_override_fields(cx);
        dialog.error = None;
        let model = dialog.model_text(cx);
        // 测试只需要地址、类型、Key 和一个模型；名称之类不在这里挑剔。
        let base_url = dialog
            .base_url
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_owned();
        if !valid_base_url(&base_url) {
            dialog.test.result = Some(TestResult {
                ok: false,
                lines: vec![format!(
                    "{}{}{}",
                    crate::i18n::t(EndpointMsg::BaseUrlFirstPrefix),
                    example_url(dialog.kind),
                    crate::i18n::t(EndpointMsg::BaseUrlFirstClose)
                )],
            });
            cx.notify();
            return;
        }
        let model = if model.is_empty() {
            dialog
                .model_choices(cx)
                .into_iter()
                .next()
                .unwrap_or_default()
        } else {
            model
        };
        if model.is_empty() {
            dialog.test.result = Some(TestResult {
                ok: false,
                lines: vec![crate::i18n::t(EndpointMsg::TestNeedsModel).to_owned()],
            });
            cx.notify();
            return;
        }
        let key = dialog.probe_key(cx);
        let mut config = zssh_core::ai::agent::builtin::provider::ProviderConfig::new(
            dialog.kind.provider_kind(),
            base_url,
            key,
        );
        config.openai =
            zssh_core::ai::agent::builtin::provider::OpenAiOptions::from(&dialog.options);
        dialog.test.generation += 1;
        dialog.test.running = true;
        dialog.test.result = None;
        let generation = dialog.test.generation;
        let Ok(runtime) = crate::ssh_backend::runtime() else {
            dialog.test.running = false;
            dialog.test.result = Some(TestResult {
                ok: false,
                lines: vec![crate::i18n::t(EndpointMsg::RuntimeNotStarted).to_owned()],
            });
            cx.notify();
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task_model = model.clone();
        runtime.spawn(async move {
            let outcome = match builtin::make_transport() {
                Ok(transport) => Ok(zssh_core::ai::agent::builtin::probe::test_endpoint(
                    transport.as_ref(),
                    &config,
                    &task_model,
                )
                .await),
                Err(reason) => Err(reason),
            };
            let _ = tx.send(outcome);
        });
        self.await_result(rx, window, cx, move |this, outcome, _window, cx| {
            this.ai_endpoint_test_done(generation, model, outcome, cx);
        });
        cx.notify();
    }

    fn ai_endpoint_test_done(
        &mut self,
        generation: u64,
        model: String,
        outcome: Result<EndpointProbe, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::AiEndpoint(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.test.generation != generation {
            return;
        }
        dialog.test.running = false;
        match outcome {
            Ok(probe) => {
                if let Some(models) = &probe.models {
                    dialog.remote_models = models.iter().map(|(id, _)| id.clone()).collect();
                    dialog.remote = probe.remote_caps();
                }
                let manual = dialog.manual_for(&model, cx);
                let caps = resolve(
                    &model,
                    manual,
                    dialog.remote.get(&model).cloned(),
                    &builtin::catalog(),
                );
                dialog.test.result = Some(describe_probe(&probe, &model, &caps));
            }
            Err(reason) => {
                dialog.test.result = Some(TestResult {
                    ok: false,
                    lines: vec![format!(
                        "{}{reason}",
                        crate::i18n::t(EndpointMsg::TestAbortedPrefix)
                    )],
                });
            }
        }
        cx.notify();
    }

    /// 对话框内容：标题、可滚动的单栏表单、固定的底部按钮栏。
    pub(super) fn ai_endpoint_body(
        &self,
        dialog: &AiEndpointDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let body_h = Self::host_props_height(self.window_height);
        let title = if dialog.id.is_some() {
            crate::i18n::t(EndpointMsg::EditTitle)
        } else {
            crate::i18n::t(EndpointMsg::AddTitle)
        };
        let err = |field: Field| {
            dialog
                .error
                .as_ref()
                .filter(|error| error.field == field)
                .map(|error| error.message.as_str())
        };
        let mut rows: Vec<AnyElement> = Vec::new();

        // 名称
        rows.push(labelled(
            required_label(crate::i18n::t(EndpointMsg::NameLabel), tokens),
            Input::new(&dialog.name),
            err(Field::Name),
            tokens,
        ));
        // 类型
        {
            let kinds = [EndpointKind::OpenAiChat, EndpointKind::Anthropic];
            let pick = entity.clone();
            rows.push(
                field(
                    crate::i18n::t(EndpointMsg::TypeLabel),
                    None,
                    segmented_bar(
                        "ai-endpoint-kind",
                        kinds
                            .iter()
                            .map(|kind| (kind_label(*kind).to_owned(), *kind == dialog.kind, None))
                            .collect(),
                        tokens,
                        move |index, window, cx| {
                            if let Some(kind) = kinds.get(index).copied() {
                                pick.update(cx, |this, cx| {
                                    this.ai_dialog_set_kind(kind, window, cx)
                                });
                            }
                        },
                    ),
                    tokens,
                )
                .into_any_element(),
            );
        }
        // Base URL
        rows.push(
            v_flex()
                .gap_1p5()
                .child(labelled(
                    required_label("Base URL", tokens),
                    Input::new(&dialog.base_url),
                    err(Field::BaseUrl),
                    tokens,
                ))
                .child(note(
                    format!(
                        "{}{}{}{}",
                        crate::i18n::t(EndpointMsg::ExamplePrefix),
                        example_url(dialog.kind),
                        crate::i18n::t(EndpointMsg::ExampleDot),
                        match dialog.kind {
                            EndpointKind::OpenAiChat => crate::i18n::t(EndpointMsg::KindOpenAiNote),
                            EndpointKind::Anthropic =>
                                crate::i18n::t(EndpointMsg::KindAnthropicNote),
                        }
                    ),
                    tokens,
                ))
                .into_any_element(),
        );
        // API Key
        {
            let eye = entity.clone();
            let has_text = !dialog.api_key.read(cx).value().is_empty();
            let eye_button = Button::new("ai-endpoint-key-eye")
                .ghost()
                .compact()
                .icon(if dialog.key_shown {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .tooltip(if dialog.key_shown {
                    crate::i18n::t(EndpointMsg::HideKey)
                } else {
                    crate::i18n::t(EndpointMsg::ShowKey)
                })
                .disabled(!has_text)
                .on_click(move |_, window, cx| {
                    eye.update(cx, |this, cx| this.ai_dialog_toggle_key(window, cx));
                });
            let hint = if dialog.id.is_some() && dialog.has_stored_key {
                crate::i18n::t(EndpointMsg::KeyKeepHint)
            } else {
                crate::i18n::t(EndpointMsg::KeyStoredHint)
            };
            let no_key = dialog.key_not_needed();
            let mut key_rows = v_flex().gap_1p5();
            // 只有 OpenAI 兼容接口能不要 Key（Ollama、vLLM、本机网关）。
            if dialog.kind == EndpointKind::OpenAiChat {
                let toggle = entity.clone();
                key_rows = key_rows.child(check_card(
                    "ai-endpoint-no-key",
                    crate::i18n::t(EndpointMsg::NoKeyLabel),
                    Some(crate::i18n::t(EndpointMsg::NoKeyHint)),
                    no_key,
                    tokens,
                    move |value, _window, cx| {
                        toggle.update(cx, |this, cx| {
                            this.with_ai_dialog(cx, |dialog| {
                                dialog.options.no_key = value;
                                dialog.error = None;
                                dialog.test.result = None;
                            });
                        });
                    },
                ));
            }
            if no_key {
                key_rows = key_rows.child(note(crate::i18n::t(EndpointMsg::NoKeyCleared), tokens));
            } else {
                key_rows = key_rows
                    .child(labelled(
                        // 编辑时库里已经有 Key、留空表示不改：不标必填。
                        if dialog.id.is_some() && dialog.has_stored_key {
                            div()
                                .typo(Role::FieldLabel, tokens)
                                .child("API Key")
                                .into_any_element()
                        } else {
                            required_label("API Key", tokens).into_any_element()
                        },
                        h_flex()
                            .w_full()
                            .gap_1()
                            .items_center()
                            .child(div().flex_1().min_w_0().child(Input::new(&dialog.api_key)))
                            .child(eye_button),
                        err(Field::ApiKey),
                        tokens,
                    ))
                    .child(note(hint, tokens));
            }
            rows.push(key_rows.into_any_element());
        }
        // 默认模型 + 下拉
        {
            let choices = dialog.model_choices(cx);
            let pick = entity.clone();
            let has_choices = !choices.is_empty();
            let menu_button = Button::new("ai-endpoint-model-menu")
                .outline()
                .compact()
                .label(crate::i18n::t(EndpointMsg::Choose))
                .dropdown_caret(true)
                .tooltip(if has_choices {
                    crate::i18n::t(EndpointMsg::ModelPickHint)
                } else {
                    crate::i18n::t(EndpointMsg::ModelFetchHint)
                })
                .disabled(!has_choices)
                .dropdown_menu(move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(240.0)).max_h(px(320.0)).scrollable(true);
                    for model in &choices {
                        let pick = pick.clone();
                        let model = model.clone();
                        menu = menu.item(menu_kit::choice(model.clone(), false).on_click(
                            move |_: &::gpui::ClickEvent, window, cx| {
                                let model = model.clone();
                                pick.update(cx, |this, cx| {
                                    this.ai_dialog_pick_model(model, window, cx)
                                });
                            },
                        ));
                    }
                    menu
                });
            rows.push(labelled(
                field_label(
                    crate::i18n::t(EndpointMsg::DefaultModelLabel),
                    crate::i18n::t(EndpointMsg::DefaultModelHint),
                    tokens,
                ),
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&dialog.default_model)),
                    )
                    .child(menu_button),
                err(Field::DefaultModel),
                tokens,
            ));
        }
        // 额外模型
        rows.push(
            field(
                crate::i18n::t(EndpointMsg::ExtraModels),
                Some(crate::i18n::t(EndpointMsg::ExtraModelsHint).to_owned()),
                Textarea::new(&dialog.extra_models),
                tokens,
            )
            .into_any_element(),
        );
        // 识别结果
        rows.push(self.ai_caps_block(dialog, tokens, cx));
        // 高级
        {
            let toggle = entity.clone();
            rows.push(fold_header(
                "ai-endpoint-advanced",
                crate::i18n::t(EndpointMsg::Advanced),
                dialog.advanced,
                tokens,
                move |_window, cx| {
                    toggle.update(cx, |this, cx| {
                        this.with_ai_dialog(cx, |dialog| dialog.advanced = !dialog.advanced);
                    });
                },
            ));
            if dialog.advanced {
                rows.extend(self.ai_advanced_rows(dialog, tokens, cx));
            }
        }

        let scroll = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .border_t_1()
            .border_color(tokens.border)
            .child(
                v_flex()
                    .id("ai-endpoint-scroll")
                    .size_full()
                    .gap_3()
                    .pt_3()
                    .pr_3()
                    .pb_2()
                    .overflow_y_scroll()
                    .track_scroll(&dialog.scroll)
                    .children(rows),
            );

        // 结果栏：测试结果（绿 / 红）；找不到字段的保存错误也在这里。
        let result: Option<AnyElement> = if dialog.test.running {
            Some(
                div()
                    .text_sm()
                    .text_color(tokens.text_muted)
                    .child(crate::i18n::t(EndpointMsg::TestingLong))
                    .into_any_element(),
            )
        } else if let Some(result) = &dialog.test.result {
            let color = if result.ok {
                tokens.dot_connected
            } else {
                tokens.danger
            };
            let mut lines = result.lines.iter();
            let head = lines.next().cloned().unwrap_or_default();
            Some(
                h_flex()
                    .id("ai-endpoint-test-result")
                    .w_full()
                    .flex_none()
                    .gap_2()
                    .items_start()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(color)
                    .child(
                        Icon::new(if result.ok {
                            IconName::CircleCheck
                        } else {
                            IconName::CircleX
                        })
                        .size_4()
                        .text_color(color),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(color)
                                    .child(SharedString::from(head)),
                            )
                            .children(lines.map(|line| {
                                div()
                                    .typo(Role::Caption, tokens)
                                    .child(SharedString::from(line.clone()))
                            })),
                    )
                    .into_any_element(),
            )
        } else {
            err(Field::General)
                .or_else(|| err(Field::Overrides))
                .map(|text| error_note(text.to_owned(), tokens.danger).into_any_element())
        };

        let (cancel, save, test) = (entity.clone(), entity.clone(), entity.clone());
        let running = dialog.test.running;
        let footer = h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .items_center()
            .pr_3()
            .child(
                text_button(
                    "ai-endpoint-test",
                    if running {
                        crate::i18n::t(EndpointMsg::Testing)
                    } else {
                        crate::i18n::t(EndpointMsg::Test)
                    },
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        test.update(cx, |this, cx| this.ai_endpoint_test(window, cx));
                    },
                )
                .disabled(running),
            )
            .child(div().flex_1())
            .child(text_button(
                "ai-endpoint-cancel",
                crate::i18n::t(EndpointMsg::Cancel),
                ButtonLook::Outline,
                move |_, window, cx| {
                    cancel.update(cx, |this, cx| this.close_modal(window, cx));
                },
            ))
            .child(text_button(
                "ai-endpoint-save",
                crate::i18n::t(EndpointMsg::Save),
                ButtonLook::Primary,
                move |_, window, cx| {
                    save.update(cx, |this, cx| this.ai_endpoint_save(window, cx));
                },
            ));

        v_flex()
            .id("ai-endpoint-dialog")
            .w_full()
            .h(px(body_h))
            .gap_3()
            .child(
                div()
                    .flex_none()
                    .typo(Role::GroupTitle, tokens)
                    .child(SharedString::from(title)),
            )
            .child(scroll)
            .children(result.map(|row| div().pr_3().child(row)))
            .child(footer)
            .into_any_element()
    }

    /// "识别为：……（来源：……）"和五项能力。
    fn ai_caps_block(
        &self,
        dialog: &AiEndpointDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((model, caps)) = dialog.caps_now(&builtin::catalog(), cx) else {
            return note(crate::i18n::t(EndpointMsg::CapsHint), tokens).into_any_element();
        };
        let name = display_name(&caps, &model);
        let mut block = v_flex()
            .id("ai-endpoint-caps")
            .gap_1()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .bg(tokens.subtle)
            .child(
                div()
                    .typo(Role::FieldLabel, tokens)
                    .child(SharedString::from(format!(
                        "{}{name}{}{}{}",
                        crate::i18n::t(EndpointMsg::Recognized2Prefix),
                        crate::i18n::t(EndpointMsg::RecognizedMid),
                        source_label(caps.source),
                        crate::i18n::t(EndpointMsg::Recognized2Close)
                    ))),
            );
        for (label, value) in caps_summary(&caps) {
            block = block.child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(84.0))
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(tokens.text)
                            .child(SharedString::from(value)),
                    ),
            );
        }
        block.into_any_element()
    }

    /// crate::i18n::t(EndpointMsg::Advanced)展开后的几行：能力覆盖、改选匹配、OpenAI 兼容的开关。
    fn ai_advanced_rows(
        &self,
        dialog: &AiEndpointDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entity = cx.entity();
        let mut rows: Vec<AnyElement> = Vec::new();
        // 手动覆盖的目标：默认是默认模型，也可以在下拉里改成别的（接口里的、手填的、已经有覆盖的）。
        let model = dialog.override_model.clone();
        if model.is_empty() {
            rows.push(
                note(crate::i18n::t(EndpointMsg::NeedDefaultModel), tokens).into_any_element(),
            );
        } else {
            rows.push(self.ai_override_target_row(dialog, &model, tokens, cx));
            rows.push(
                note(
                    format!(
                        "{}{model}{}",
                        crate::i18n::t(EndpointMsg::ManualForPrefix),
                        crate::i18n::t(EndpointMsg::ManualForSuffix)
                    ),
                    tokens,
                )
                .into_any_element(),
            );
            rows.push(
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(field(
                        crate::i18n::t(EndpointMsg::ContextWindow),
                        Some(crate::i18n::t(EndpointMsg::ContextHint).to_owned()),
                        Input::new(&dialog.cap_context),
                        tokens,
                    )))
                    .child(div().flex_1().min_w_0().child(field(
                        crate::i18n::t(EndpointMsg::MaxOutput),
                        Some("token".to_owned()),
                        Input::new(&dialog.cap_output),
                        tokens,
                    )))
                    .into_any_element(),
            );
            {
                let pick = entity.clone();
                let current = dialog.cap_fields.thinking.clone();
                let selected = thinking_choice_index(&current);
                rows.push(
                    field(
                        crate::i18n::t(EndpointMsg::Reasoning),
                        Some(crate::i18n::t(EndpointMsg::ReasoningHint).to_owned()),
                        segmented_bar(
                            "ai-cap-thinking",
                            thinking_choices()
                                .iter()
                                .enumerate()
                                .map(|(index, label)| {
                                    ((*label).to_owned(), index == selected, None)
                                })
                                .collect(),
                            tokens,
                            move |index, _window, cx| {
                                pick.update(cx, |this, cx| {
                                    this.with_ai_dialog(cx, |dialog| {
                                        let next = thinking_from_choice(
                                            index,
                                            &dialog.cap_fields.thinking,
                                        );
                                        dialog.cap_fields.thinking = next;
                                    });
                                });
                            },
                        ),
                        tokens,
                    )
                    .into_any_element(),
                );
            }
            {
                let (tools, image) = (entity.clone(), entity.clone());
                rows.push(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .items_start()
                        .child(div().flex_1().min_w_0().child(field(
                            crate::i18n::t(EndpointMsg::ToolCalls),
                            None,
                            force_bar(
                                "ai-cap-tools",
                                dialog.cap_fields.tools,
                                tokens,
                                move |value, _w, cx| {
                                    tools.update(cx, |this, cx| {
                                        this.with_ai_dialog(cx, |d| d.cap_fields.tools = value);
                                    });
                                },
                            ),
                            tokens,
                        )))
                        .child(div().flex_1().min_w_0().child(field(
                            crate::i18n::t(EndpointMsg::ImageInput),
                            None,
                            force_bar(
                                "ai-cap-image",
                                dialog.cap_fields.image,
                                tokens,
                                move |value, _w, cx| {
                                    image.update(cx, |this, cx| {
                                        this.with_ai_dialog(cx, |d| d.cap_fields.image = value);
                                    });
                                },
                            ),
                            tokens,
                        )))
                        .into_any_element(),
                );
            }
            rows.push(self.ai_pick_block(dialog, &model, tokens, cx));
        }
        if dialog.kind == EndpointKind::OpenAiChat {
            let opt = |id: &'static str,
                       title: &'static str,
                       hint: &'static str,
                       current: Tri,
                       set: fn(&mut OpenAiOptionsConfig, Tri)| {
                let entity = entity.clone();
                field(
                    title,
                    Some(hint.to_owned()),
                    tri_bar(
                        id,
                        current,
                        [
                            crate::i18n::t(EndpointMsg::Auto),
                            crate::i18n::t(EndpointMsg::On),
                            crate::i18n::t(EndpointMsg::Off),
                        ],
                        tokens,
                        move |value, _w, cx| {
                            entity.update(cx, |this, cx| {
                                this.with_ai_dialog(cx, |d| set(&mut d.options, value));
                            });
                        },
                    ),
                    tokens,
                )
                .into_any_element()
            };
            rows.push(opt(
                "ai-opt-max-completion",
                crate::i18n::t(EndpointMsg::MaxCompletionTokens),
                crate::i18n::t(EndpointMsg::MaxCompletionHint),
                dialog.options.max_completion_tokens,
                |o, v| o.max_completion_tokens = v,
            ));
            rows.push(opt(
                "ai-opt-reasoning-echo",
                crate::i18n::t(EndpointMsg::ReasoningContent),
                crate::i18n::t(EndpointMsg::ReasoningContentHint),
                dialog.options.echo_reasoning_content,
                |o, v| o.echo_reasoning_content = v,
            ));
            rows.push(opt(
                "ai-opt-thinking",
                crate::i18n::t(EndpointMsg::ThinkingSwitch),
                crate::i18n::t(EndpointMsg::ThinkingSwitchHint),
                dialog.options.thinking_toggle,
                |o, v| o.thinking_toggle = v,
            ));
            let usage = entity.clone();
            rows.push(
                check_card(
                    "ai-opt-include-usage",
                    crate::i18n::t(EndpointMsg::IncludeUsage),
                    Some(crate::i18n::t(EndpointMsg::IncludeUsageHint)),
                    dialog.options.include_usage,
                    tokens,
                    move |value, _window, cx| {
                        usage.update(cx, |this, cx| {
                            this.with_ai_dialog(cx, |d| d.options.include_usage = value);
                        });
                    },
                )
                .into_any_element(),
            );
        }
        rows
    }

    /// 「要覆盖的模型」一行：当前目标 + 下拉（默认模型排第一，其后是接口返回的、手填的、已经有覆盖的）。
    fn ai_override_target_row(
        &self,
        dialog: &AiEndpointDialog,
        target: &str,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let default = dialog.model_text(cx);
        let mut choices: Vec<String> = Vec::new();
        for name in std::iter::once(default.clone())
            .chain(dialog.model_choices(cx))
            .chain(dialog.models_with_overrides())
        {
            if !name.is_empty() && !choices.contains(&name) {
                choices.push(name);
            }
        }
        let overridden = dialog.models_with_overrides();
        let pick = cx.entity();
        let label = if target == default {
            format!(
                "{target}{}",
                crate::i18n::t(EndpointMsg::ManualDefaultSuffix)
            )
        } else {
            target.to_owned()
        };
        let menu_button = Button::new("ai-override-model-menu")
            .outline()
            .compact()
            .label(SharedString::from(label))
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu.min_w(px(260.0)).max_h(px(320.0)).scrollable(true);
                for model in &choices {
                    let mut text = model.clone();
                    if *model == default {
                        text.push_str(crate::i18n::t(EndpointMsg::ManualDefaultSuffix));
                    }
                    if overridden.contains(model) {
                        text.push_str(crate::i18n::t(EndpointMsg::ManualOverrideSuffix));
                    }
                    let pick = pick.clone();
                    let model = model.clone();
                    menu = menu.item(menu_kit::choice(text, false).on_click(
                        move |_: &::gpui::ClickEvent, window, cx| {
                            let model = model.clone();
                            pick.update(cx, |this, cx| {
                                this.ai_dialog_pick_override_model(model, window, cx)
                            });
                        },
                    ));
                }
                menu
            });
        field(
            crate::i18n::t(EndpointMsg::OverrideModel),
            Some(crate::i18n::t(EndpointMsg::OverrideHint).to_owned()),
            menu_button,
            tokens,
        )
        .into_any_element()
    }

    /// 选「要覆盖的模型」：先把正在编辑的那个模型的覆盖存好，再读新目标的。
    pub(crate) fn ai_dialog_pick_override_model(
        &mut self,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::AiEndpoint(mut dialog)) = self.modal.take() else {
            return;
        };
        dialog.pick_override_model(&model, window, cx);
        // 测试结果说的是别的模型时不动它；能力块是实时算的，会跟着手动值变。
        self.modal = Some(Modal::AiEndpoint(dialog));
        cx.notify();
    }

    /// "识别为"不对时改选：在 models.dev 的数据里搜一个，点一下就按它的能力算。
    fn ai_pick_block(
        &self,
        dialog: &AiEndpointDialog,
        model: &str,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let catalog = builtin::catalog();
        let query = dialog.pick_query.read(cx).value().trim().to_lowercase();
        let hits: Vec<(String, String)> = if query.is_empty() {
            Vec::new()
        } else {
            catalog
                .entries()
                .iter()
                .filter(|entry| {
                    entry.id.to_lowercase().contains(&query)
                        || entry.name.to_lowercase().contains(&query)
                })
                .take(6)
                .map(|entry| (entry.key(), format!("{}  ·  {}", entry.name, entry.key())))
                .collect()
        };
        let mut block = v_flex().gap_1p5().child(field(
            crate::i18n::t(EndpointMsg::Remap),
            Some(format!(
                "{}{model}{}",
                crate::i18n::t(EndpointMsg::RemapPrefix),
                crate::i18n::t(EndpointMsg::RemapSuffix)
            )),
            Input::new(&dialog.pick_query),
            tokens,
        ));
        if let Some(picked) = &dialog.cap_fields.pick {
            let clear = entity.clone();
            block = block.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(tokens.text)
                            .child(SharedString::from(format!(
                                "{}{picked}",
                                crate::i18n::t(EndpointMsg::RemappedPrefix)
                            ))),
                    )
                    .child(text_button(
                        "ai-pick-clear",
                        crate::i18n::t(EndpointMsg::ClearRemap),
                        ButtonLook::Ghost,
                        move |_, _w, cx| {
                            clear.update(cx, |this, cx| {
                                this.with_ai_dialog(cx, |d| d.cap_fields.pick = None);
                            });
                        },
                    )),
            );
        }
        for (index, (key, label)) in hits.into_iter().enumerate() {
            let pick = entity.clone();
            let hover = tokens.hover;
            block = block.child(
                div()
                    .id(("ai-pick-hit", index))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(SharedString::from(label))
                    .on_click(move |_, _w, cx| {
                        let key = key.clone();
                        pick.update(cx, |this, cx| {
                            this.with_ai_dialog(cx, |d| d.cap_fields.pick = Some(key));
                        });
                    }),
            );
        }
        block.into_any_element()
    }
}

fn labelled(
    label: impl IntoElement,
    control: impl IntoElement,
    error: Option<&str>,
    tokens: &Tokens,
) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .child(label)
        .child(control)
        .children(error.map(|text| error_note(text.to_owned(), tokens.danger)))
        .into_any_element()
}

/// 标签 + 灰色补充说明（不带红星）。
fn field_label(label: &str, hint: &str, tokens: &Tokens) -> Div {
    h_flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .typo(Role::FieldLabel, tokens)
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .typo(Role::Caption, tokens)
                .child(SharedString::from(hint.to_owned())),
        )
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：`gpui_kit::*` 里有个同名的 `test` 宏，会把 `#[test]` 抢走。
    use super::{
        Field, Force, FormValues, OverrideFields, build_draft, describe_probe, example_url,
        kind_label, override_from_fields, parse_tokens, source_label, store_override,
        thinking_choice_index, thinking_from_choice,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;
    use zssh_core::ai::agent::builtin::catalog::CapsSource;
    use zssh_core::ai::agent::builtin::catalog::{
        CapsOverride, Catalog, ModelCaps, PartialCaps, ReasoningSupport, resolve,
    };
    use zssh_core::ai::agent::builtin::config::{EndpointKind, OpenAiOptionsConfig};
    use zssh_core::ai::agent::builtin::probe::EndpointProbe;
    use zssh_core::ai::agent::builtin::provider::{ErrorKind, ProviderError};

    fn values<'a>() -> FormValues<'a> {
        FormValues {
            id: None,
            preset_id: None,
            kind: EndpointKind::OpenAiChat,
            name: "我的网关",
            base_url: "https://api.example.com/v1/",
            api_key: "sk-test-0001",
            has_stored_key: false,
            default_model: " gpt-5.5 ",
            extra_models: "my-chat-model\n\n  other-model  \nmy-chat-model\n",
            options: OpenAiOptionsConfig::default(),
            overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn a_complete_form_builds_a_clean_draft() {
        let draft = build_draft(&values()).expect("合法");
        assert_eq!(draft.name, "我的网关");
        assert_eq!(
            draft.base_url, "https://api.example.com/v1",
            "去掉末尾的斜杠"
        );
        assert_eq!(draft.api_key.as_deref(), Some("sk-test-0001"));
        assert_eq!(draft.default_model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            draft.models,
            ["my-chat-model", "other-model", "my-chat-model"],
            "一行一个、去空行；去重交给 core 的 normalize_draft"
        );
        assert_eq!(draft.kind, EndpointKind::OpenAiChat);
    }

    #[test]
    fn the_form_explains_what_is_wrong_and_where() {
        let mut v = values();
        v.name = "  ";
        assert_eq!(build_draft(&v).unwrap_err().field, Field::Name);

        let mut v = values();
        v.base_url = "";
        let error = build_draft(&v).unwrap_err();
        assert_eq!(error.field, Field::BaseUrl);
        assert!(
            error.message.contains("api.example.com"),
            "{}",
            error.message
        );
        v.kind = EndpointKind::Anthropic;
        assert!(
            build_draft(&v)
                .unwrap_err()
                .message
                .contains("api.anthropic.com")
        );

        for bad in [
            "api.example.com/v1",
            "ftp://x.example/v1",
            "https://a b.example",
        ] {
            let mut v = values();
            v.base_url = bad;
            assert_eq!(build_draft(&v).unwrap_err().field, Field::BaseUrl, "{bad}");
        }

        let mut v = values();
        v.api_key = "   ";
        assert_eq!(build_draft(&v).unwrap_err().field, Field::ApiKey);
        v.api_key = "sk-test\n0001";
        let error = build_draft(&v).unwrap_err();
        assert_eq!(error.field, Field::ApiKey);
        assert!(error.message.contains("空格或换行"), "{}", error.message);
    }

    #[test]
    fn editing_may_leave_the_key_blank_only_when_one_is_stored() {
        let mut v = values();
        v.id = Some(3);
        v.api_key = "";
        v.has_stored_key = true;
        let draft = build_draft(&v).expect("留空表示不改");
        assert_eq!(draft.api_key, None, "None = 不改已存的 Key");
        v.has_stored_key = false;
        assert_eq!(
            build_draft(&v).unwrap_err().field,
            Field::ApiKey,
            "库里没有 Key 时编辑也得补上"
        );
        // 新建（没有 id）不管 has_stored_key 都必填。
        let mut v = values();
        v.api_key = "";
        v.has_stored_key = true;
        assert_eq!(build_draft(&v).unwrap_err().field, Field::ApiKey);
    }

    #[test]
    fn an_openai_endpoint_can_say_it_needs_no_key_and_then_drops_any_stored_key() {
        let mut v = values();
        v.api_key = "";
        v.options.no_key = true;
        let draft = build_draft(&v).expect("不需要 Key 就不用填");
        assert_eq!(
            draft.api_key.as_deref(),
            Some(""),
            "空串 = 清掉库里已存的 Key"
        );
        assert!(draft.openai_options.no_key);
        // 勾了不需要，即使框里残留着内容也不存。
        v.api_key = "sk-leftover-0001";
        let draft = build_draft(&v).expect("合法");
        assert_eq!(draft.api_key.as_deref(), Some(""));
        // 没勾又没填：报错并提示可以勾选。
        let mut v = values();
        v.api_key = "";
        let error = build_draft(&v).unwrap_err();
        assert_eq!(error.field, Field::ApiKey);
        assert!(error.message.contains("不需要 Key"), "{}", error.message);
    }

    #[test]
    fn anthropic_always_needs_a_key_even_if_the_flag_was_left_on() {
        let mut v = values();
        v.kind = EndpointKind::Anthropic;
        v.api_key = "";
        v.options.no_key = true;
        let error = build_draft(&v).unwrap_err();
        assert_eq!(error.field, Field::ApiKey);
        assert!(!error.message.contains("不需要 Key"), "{}", error.message);
        v.api_key = "sk-ant-test-0001";
        let draft = build_draft(&v).expect("合法");
        assert!(
            !draft.openai_options.no_key,
            "Anthropic 存下来不带不需要 Key 的标记"
        );
        assert_eq!(draft.api_key.as_deref(), Some("sk-ant-test-0001"));
    }

    #[test]
    fn the_override_target_follows_the_default_model_until_another_one_is_picked() {
        use super::{normalize_pick, override_target, switch_override_target};
        assert_eq!(override_target(None, "gpt-5.5"), "gpt-5.5");
        assert_eq!(override_target(Some("other"), "gpt-5.5"), "other");
        // 选的就是默认模型 = 回到跟着默认模型；空也是。
        assert_eq!(normalize_pick("gpt-5.5", "gpt-5.5"), None);
        assert_eq!(normalize_pick(" gpt-5.5 ", " gpt-5.5"), None);
        assert_eq!(normalize_pick("", "gpt-5.5"), None);
        assert_eq!(normalize_pick("other", "gpt-5.5"), Some("other".to_owned()));

        let mut table: BTreeMap<String, CapsOverride> = BTreeMap::new();
        let context = |n: u32| CapsOverride {
            context_window: Some(n),
            ..CapsOverride::default()
        };
        table.insert("other".to_owned(), context(64_000));
        // 默认模型 → other：默认模型正在编辑的值先存回，other 已有的覆盖读出来。
        let loaded = switch_override_target(&mut table, "gpt-5.5", Some(context(400_000)), "other");
        assert_eq!(loaded.context_window, Some(64_000));
        assert_eq!(table["gpt-5.5"].context_window, Some(400_000));
        // other → 一个没有覆盖的模型：other 改成全空就从表里删掉；新目标读到全空。
        let loaded =
            switch_override_target(&mut table, "other", Some(CapsOverride::default()), "m3");
        assert_eq!(loaded, CapsOverride::default());
        assert!(!table.contains_key("other"));
        assert_eq!(
            table["gpt-5.5"].context_window,
            Some(400_000),
            "别的模型的覆盖不动"
        );
        // 表单里的数字读不懂（旧项是 None）：旧模型的已存值不动。
        let loaded = switch_override_target(&mut table, "gpt-5.5", None, "m3");
        assert_eq!(loaded, CapsOverride::default());
        assert_eq!(table["gpt-5.5"].context_window, Some(400_000));
    }

    #[test]
    fn token_counts_accept_plain_numbers_and_k_m_suffixes() {
        assert_eq!(parse_tokens(""), Ok(None));
        assert_eq!(parse_tokens(" 128000 "), Ok(Some(128_000)));
        assert_eq!(parse_tokens("128k"), Ok(Some(128_000)));
        assert_eq!(parse_tokens("1M"), Ok(Some(1_000_000)));
        assert_eq!(parse_tokens("1,048,576"), Ok(Some(1_048_576)));
        assert!(parse_tokens("abc").is_err());
        assert!(parse_tokens("0").is_err());
        assert!(parse_tokens("99999999999").is_err());
    }

    #[test]
    fn manual_overrides_round_trip_and_an_empty_one_is_dropped() {
        let fields = OverrideFields {
            context: "64K".to_owned(),
            output: "8192".to_owned(),
            tools: Force::No,
            image: Force::Yes,
            thinking: Some(ReasoningSupport::Toggle),
            pick: Some("openai/gpt-5.5".to_owned()),
        };
        let item = override_from_fields(&fields).expect("合法");
        assert_eq!(item.context_window, Some(64_000));
        assert_eq!(item.max_output, Some(8192));
        assert_eq!(item.tool_call, Some(false));
        assert_eq!(item.image_input, Some(true));
        assert_eq!(item.reasoning, Some(ReasoningSupport::Toggle));
        assert_eq!(item.pick.as_deref(), Some("openai/gpt-5.5"));
        // 读回表单：数字写成完整数字，别的不变。
        let back = OverrideFields::from_override(&item);
        assert_eq!(back.context, "64000");
        assert_eq!(back.tools, Force::No);

        let mut all = BTreeMap::new();
        store_override(&mut all, "gpt-5.5", item.clone());
        store_override(
            &mut all,
            "other",
            CapsOverride {
                tool_call: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(all.len(), 2);
        // 把这个模型改回全自动：这一项删掉，别的模型的覆盖不动。
        store_override(
            &mut all,
            "gpt-5.5",
            override_from_fields(&OverrideFields::default()).unwrap(),
        );
        assert_eq!(all.keys().collect::<Vec<_>>(), ["other"]);
        // 没填模型名时不存。
        store_override(&mut all, "  ", item);
        assert_eq!(all.len(), 1);

        let too_big = OverrideFields {
            context: "4096".to_owned(),
            output: "8192".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            override_from_fields(&too_big).unwrap_err().field,
            Field::Overrides
        );
        let junk = OverrideFields {
            context: "很多".to_owned(),
            ..Default::default()
        };
        assert!(
            override_from_fields(&junk)
                .unwrap_err()
                .message
                .contains("上下文窗口")
        );
    }

    #[test]
    fn the_thinking_override_keeps_details_when_the_same_choice_is_clicked_again() {
        assert_eq!(
            thinking_from_choice(0, &Some(ReasoningSupport::Toggle)),
            None
        );
        let levels = Some(ReasoningSupport::Levels(vec!["low".into(), "xhigh".into()]));
        assert_eq!(
            thinking_from_choice(3, &levels),
            levels,
            "点已选的一档不丢等级清单"
        );
        assert_eq!(thinking_choice_index(&levels), 3);
        match thinking_from_choice(3, &None) {
            Some(ReasoningSupport::Levels(list)) => assert_eq!(list, ["low", "medium", "high"]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            thinking_from_choice(4, &None),
            Some(ReasoningSupport::Budget {
                min: 1024,
                max: 32_000
            })
        ));
        assert_eq!(thinking_from_choice(1, &None), Some(ReasoningSupport::None));
    }

    fn sample_caps() -> ModelCaps {
        let catalog = Catalog::builtin();
        resolve("gpt-5.5", None, None, &catalog)
    }

    #[test]
    fn a_passing_probe_reports_time_model_reply_capabilities_and_the_model_list() {
        use zssh_core::ai::agent::builtin::probe::ReplyProbe;
        let caps = sample_caps();
        let probe = EndpointProbe {
            models: Some(vec![("gpt-5.5".to_owned(), PartialCaps::default())]),
            reply: Ok(ReplyProbe {
                elapsed: Duration::from_millis(1234),
                text: "OK".to_owned(),
                usage: None,
            }),
        };
        let result = describe_probe(&probe, "gpt-5.5", &caps);
        assert!(result.ok);
        assert_eq!(
            result.lines[0],
            "测试通过：用时 1.2 秒 · 模型 gpt-5.5 · 回复「OK」"
        );
        assert!(
            result.lines[1].starts_with("识别为 "),
            "{}",
            result.lines[1]
        );
        assert!(result.lines[1].contains("来源："), "{}", result.lines[1]);
        assert!(
            result.lines[1].contains("上下文窗口"),
            "{}",
            result.lines[1]
        );
        assert_eq!(
            result.lines[2],
            "接口返回了 1 个模型，可以在「默认模型」旁的下拉里选"
        );
    }

    #[test]
    fn a_failing_probe_shows_the_chinese_reason_server_text_and_code() {
        let caps = sample_caps();
        let probe = EndpointProbe {
            models: None,
            reply: Err(ProviderError {
                kind: ErrorKind::Auth,
                message: "API Key 不对或没有权限（HTTP 401）。".to_owned(),
                detail: Some("invalid x-api-key".to_owned()),
                retryable: false,
                retry_after: None,
                status: Some(401),
                code: Some("invalid_api_key".to_owned()),
            }),
        };
        let result = describe_probe(&probe, "m", &caps);
        assert!(!result.ok);
        assert_eq!(
            result.lines[0],
            "测试失败：API Key 不对或没有权限（HTTP 401）。"
        );
        assert_eq!(result.lines[1], "服务端返回：invalid x-api-key");
        assert_eq!(result.lines[2], "错误码：invalid_api_key");
        assert!(
            result.lines[3].contains("没有提供模型列表"),
            "{}",
            result.lines[3]
        );
    }

    #[test]
    fn an_empty_reply_is_still_a_pass_with_an_explanation() {
        use zssh_core::ai::agent::builtin::probe::ReplyProbe;
        let probe = EndpointProbe {
            models: None,
            reply: Ok(ReplyProbe {
                elapsed: Duration::from_secs(2),
                text: String::new(),
                usage: None,
            }),
        };
        let result = describe_probe(&probe, "m", &sample_caps());
        assert!(result.ok);
        assert!(
            result.lines[0].contains("模型没有输出文字"),
            "{}",
            result.lines[0]
        );
    }

    #[test]
    fn a_saved_form_comes_back_from_the_database_with_its_overrides_and_switches() {
        use zssh_core::store::Database;
        let mut overrides = BTreeMap::new();
        store_override(
            &mut overrides,
            "gpt-5.5",
            override_from_fields(&OverrideFields {
                context: "64K".into(),
                tools: Force::No,
                thinking: Some(ReasoningSupport::Toggle),
                pick: Some("openai/gpt-5.4".into()),
                ..Default::default()
            })
            .expect("合法"),
        );
        let options = OpenAiOptionsConfig {
            max_completion_tokens: zssh_core::ai::agent::builtin::config::Tri::On,
            include_usage: false,
            ..Default::default()
        };
        let mut form = values();
        form.overrides = overrides;
        form.options = options.clone();
        let db = Database::open_in_memory().expect("内存库");
        let saved = db
            .ai_endpoint_save(&build_draft(&form).expect("合法"))
            .expect("保存");
        let back = db.ai_endpoint_get(saved.id).expect("读");
        assert_eq!(back.openai_options, options);
        let item = &back.overrides["gpt-5.5"];
        assert_eq!(item.context_window, Some(64_000));
        assert_eq!(item.tool_call, Some(false));
        assert_eq!(item.reasoning, Some(ReasoningSupport::Toggle));
        assert_eq!(item.pick.as_deref(), Some("openai/gpt-5.4"));
        // 编辑再存、Key 留空：覆盖和 Key 都保留。
        let mut edit = values();
        edit.id = Some(saved.id);
        edit.api_key = "";
        edit.has_stored_key = true;
        edit.overrides = back.overrides.clone();
        let again = db
            .ai_endpoint_save(&build_draft(&edit).expect("合法"))
            .expect("保存");
        assert_eq!(again.overrides, back.overrides);
        assert!(again.has_key);
        assert_eq!(
            db.ai_endpoint_api_key(again.id).expect("读").as_deref(),
            Some("sk-test-0001")
        );
    }

    #[test]
    fn the_source_labels_cover_every_layer() {
        for (source, text) in [
            (CapsSource::Manual, "手动"),
            (CapsSource::Endpoint, "接口"),
            (CapsSource::ModelsDev, "models.dev"),
            (CapsSource::Builtin, "内置快照"),
            (CapsSource::Heuristic, "估计"),
        ] {
            assert_eq!(source_label(source), text);
        }
    }

    #[test]
    fn example_urls_follow_the_kind() {
        assert_eq!(
            example_url(EndpointKind::OpenAiChat),
            "https://api.example.com/v1"
        );
        assert_eq!(
            example_url(EndpointKind::Anthropic),
            "https://api.anthropic.com"
        );
        assert_eq!(kind_label(EndpointKind::OpenAiChat), "OpenAI 兼容");
    }
}
