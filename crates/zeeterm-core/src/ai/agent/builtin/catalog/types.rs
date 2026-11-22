//! 能力识别的公开数据类型。A3（对话循环）和 A4（设置页 / AI 面板）直接用这些。

use serde::{Deserialize, Serialize};

/// 模型的思考（推理）支持情况。
///
/// 来自 models.dev 的 `reasoning` + `reasoning_options`。后者是一个列表，可以同时给出
/// `toggle` / `effort`（带 `values`）/ `budget_tokens`（带可选 `min`、`max`）几种控制方式；
/// 这里只能表达一种，取最能指导界面的那个，优先级 `Levels` > `Budget` > `Toggle`。
/// `reasoning: true` 但没有任何选项的模型（常开、不可调）归为 `None`：没有可控的东西，
/// 界面不该给思考强度下拉，请求里也不该带思考参数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSupport {
    /// 不支持，或者支持但没有可调的开关。
    #[default]
    None,
    /// 只能开 / 关。
    Toggle,
    /// 离散的强度等级，按数据里给的顺序（如 `low` / `medium` / `high` / `xhigh` / `max`）。
    Levels(Vec<String>),
    /// 按 token 预算控制。数据里没给 `min` 时取 1024（Anthropic 的下限），没给 `max` 时取
    /// 该模型的最大输出。
    Budget { min: u32, max: u32 },
}

/// 价格，美元 / 百万 token。只用来显示"本对话约花费"。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// 能力数据最终来自哪一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsSource {
    /// 用户手动填的。
    Manual,
    /// 接口（`GET /models`）自报的。
    Endpoint,
    /// 来自运行时更新的 models.dev 缓存。
    ModelsDev,
    /// 来自安装包内置的 models.dev 快照。
    Builtin,
    /// 按模型家族估计的（界面标"估计"）。
    Heuristic,
}

/// 合成后的模型能力。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCaps {
    /// 上下文窗口（token）。
    pub context_window: u32,
    /// 最大输入（token）；多数模型没有单独的限制，是 None。
    pub max_input: Option<u32>,
    /// 单次最大输出（token），已保证不超过上下文窗口。
    pub max_output: u32,
    pub reasoning: ReasoningSupport,
    pub tool_call: bool,
    pub image_input: bool,
    pub cost: Option<Cost>,
    /// 上下文窗口这个最关键的数字来自哪一层；其他字段可能来自别的层（逐字段合并）。
    pub source: CapsSource,
    /// 界面上"识别为：xxx"用的名字；没匹配到数据库时是用户填的原名。
    pub display_name: String,
    /// 匹配到的数据库条目键（`厂商/模型id`，可用 [`super::Catalog::get`] 取回）；没匹配到是 None。
    pub matched_id: Option<String>,
}

/// 某一层给出的部分能力（接口自报用）。字段都可选：没给就是没给，不猜。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PartialCaps {
    pub context_window: Option<u32>,
    pub max_input: Option<u32>,
    pub max_output: Option<u32>,
    pub reasoning: Option<ReasoningSupport>,
    pub tool_call: Option<bool>,
    pub image_input: Option<bool>,
    pub display_name: Option<String>,
}

/// 用户在"高级"里手动填的值；每项都可选，填了就覆盖识别结果。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CapsOverride {
    pub context_window: Option<u32>,
    pub max_input: Option<u32>,
    pub max_output: Option<u32>,
    pub reasoning: Option<ReasoningSupport>,
    pub tool_call: Option<bool>,
    pub image_input: Option<bool>,
    pub cost: Option<Cost>,
    /// 用户在"识别为……"里改选的数据库条目（[`ModelCaps::matched_id`] 的值）；
    /// 填了就不再按名字匹配。键在数据库里找不到时回到按名字匹配。
    pub pick: Option<String>,
}
