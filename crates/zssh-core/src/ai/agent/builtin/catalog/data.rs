//! 裁剪后的 models.dev 数据：条目格式、从 `api.json` 裁剪、快照 / 缓存文件的读写格式。
//!
//! 内置快照和运行时缓存用同一种格式，所以生成快照的小工具和运行时更新共用 [`trim_api_json`]。

use std::collections::BTreeMap;

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::types::{Cost, ReasoningSupport};

/// 文件格式版本。条目字段有不兼容改动时加一；读到不认识的版本按"文件无效"处理。
pub const FORMAT_VERSION: u32 = 1;

/// 直接收录的厂商自家接口（models.dev 的 provider id）。
const PRIMARY_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "alibaba",
    "alibaba-cn",
    "moonshotai",
    "moonshotai-cn",
    "zhipuai",
    "zai",
    "minimax",
    "minimax-cn",
    "xai",
    "meta",
    "mistral",
];

/// OpenRouter 条目只收这些厂商前缀（`~` 开头的是它的"最新版"别名，前缀照样算）。
const OPENROUTER_VENDORS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "qwen",
    "moonshotai",
    "z-ai",
    "minimax",
    "x-ai",
    "meta",
    "meta-llama",
    "mistralai",
];

/// 单个模型的裁剪后条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// models.dev 的 provider id（`anthropic`、`openrouter` ……）。
    pub provider: String,
    /// 该 provider 下的模型 id（OpenRouter 下形如 `anthropic/claude-opus-5`）。
    pub id: String,
    pub name: String,
    /// models.dev 的 `canonical_model_id`，跨 provider 指向同一个模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub context: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u32>,
    pub output: u32,
    #[serde(default, skip_serializing_if = "is_no_reasoning")]
    pub reasoning: ReasoningSupport,
    pub tool_call: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub image: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// models.dev 标了 `deprecated`：仍然收录（用户可能还在用这个名字），匹配时排后。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
}

fn is_no_reasoning(r: &ReasoningSupport) -> bool {
    *r == ReasoningSupport::None
}

impl CatalogEntry {
    /// 条目在数据库里的唯一键（[`super::ModelCaps::matched_id`]）。
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// 快照 / 缓存文件。`generated` 与 `fetched_at` 二选一出现：快照写日期，缓存写时间戳。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogFile {
    pub version: u32,
    pub source: String,
    /// 快照生成日期，`YYYY-MM-DD`（UTC）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<String>,
    /// 缓存拉取时间，Unix 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    pub entries: Vec<CatalogEntry>,
}

impl CatalogFile {
    /// 文件对应的时间（Unix 秒）：缓存用 `fetched_at`，快照用 `generated` 当天 0 点。
    pub fn timestamp(&self) -> Option<u64> {
        self.fetched_at
            .or_else(|| self.generated.as_deref().and_then(date_to_unix))
    }

    /// 序列化成一行一个条目的 JSON，便于看 diff；快照生成和缓存写入都用它。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut out = String::with_capacity(self.entries.len() * 260 + 128);
        out.push_str("{\n");
        out.push_str(&format!("\"version\":{},\n", self.version));
        out.push_str(&format!(
            "\"source\":{},\n",
            serde_json::to_string(&self.source)?
        ));
        if let Some(g) = &self.generated {
            out.push_str(&format!("\"generated\":{},\n", serde_json::to_string(g)?));
        }
        if let Some(t) = self.fetched_at {
            out.push_str(&format!("\"fetched_at\":{t},\n"));
        }
        out.push_str("\"entries\":[\n");
        for (i, e) in self.entries.iter().enumerate() {
            out.push_str(&serde_json::to_string(e)?);
            out.push_str(if i + 1 == self.entries.len() {
                "\n"
            } else {
                ",\n"
            });
        }
        out.push_str("]\n}\n");
        Ok(out)
    }
}

/// 数据处理出错。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("models.dev 数据解析失败：{0}")]
    Parse(String),
    #[error("models.dev 数据不可用：{0}")]
    Invalid(String),
    #[error("缓存文件读写失败：{0}")]
    Io(String),
}

// ---- 原始 api.json 的宽松解析 ----

#[derive(Deserialize)]
struct RawModel {
    id: String,
    name: Option<String>,
    family: Option<String>,
    reasoning: Option<bool>,
    reasoning_options: Option<Vec<RawReasoningOption>>,
    tool_call: Option<bool>,
    modalities: Option<RawModalities>,
    limit: Option<RawLimit>,
    cost: Option<RawCost>,
    status: Option<String>,
    canonical_model_id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RawReasoningOption {
    #[serde(rename = "type")]
    kind: String,
    values: Option<Vec<String>>,
    min: Option<u64>,
    max: Option<u64>,
}

#[derive(Deserialize, Default)]
struct RawModalities {
    #[serde(default)]
    input: Vec<String>,
    #[serde(default)]
    output: Vec<String>,
}

#[derive(Deserialize)]
struct RawLimit {
    context: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
}

#[derive(Deserialize)]
struct RawCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

/// 单个模型坏了（字段类型不对）只丢这一个，不让整份数据作废。
struct Lenient<T>(Option<T>);

impl<'de, T: de::DeserializeOwned> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(Lenient(serde_json::from_value(v).ok()))
    }
}

#[derive(Deserialize, Default)]
struct RawProvider {
    #[serde(default)]
    models: BTreeMap<String, Lenient<RawModel>>,
}

/// 顶层 `{provider_id: {...}}`：不收录的厂商直接 `IgnoredAny` 跳过，不去解析它们的几千个模型。
struct ProvidersSeed;

impl<'de> DeserializeSeed<'de> for ProvidersSeed {
    type Value = Vec<CatalogEntry>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<CatalogEntry>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("models.dev api.json")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(provider) = map.next_key::<String>()? {
                    let wanted =
                        provider == "openrouter" || PRIMARY_PROVIDERS.contains(&provider.as_str());
                    if !wanted {
                        map.next_value::<IgnoredAny>()?;
                        continue;
                    }
                    let Lenient(parsed) = map.next_value::<Lenient<RawProvider>>()?;
                    let Some(raw) = parsed else { continue };
                    for (_, Lenient(model)) in raw.models {
                        if let Some(entry) = model.and_then(|m| convert(&provider, m)) {
                            out.push(entry);
                        }
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_map(V)
    }
}

fn to_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn convert(provider: &str, m: RawModel) -> Option<CatalogEntry> {
    // 只要聊天用的文本模型：图片 / 向量 / 语音 / 重排等没有意义。
    if m.kind.as_deref().is_some_and(|k| k != "chat") {
        return None;
    }
    let modalities = m.modalities.unwrap_or_default();
    if !modalities.output.iter().any(|x| x == "text") {
        return None;
    }
    if provider == "openrouter" {
        let vendor = m.id.trim_start_matches('~').split('/').next().unwrap_or("");
        if !OPENROUTER_VENDORS.contains(&vendor) {
            return None;
        }
    }
    let limit = m.limit?;
    let context = to_u32(limit.context.filter(|&c| c > 0)?);
    let output = to_u32(limit.output.unwrap_or(0));
    let reasoning = if m.reasoning.unwrap_or(false) {
        reasoning_support(m.reasoning_options.as_deref().unwrap_or(&[]), output)
    } else {
        ReasoningSupport::None
    };
    let cost = m.cost.and_then(|c| {
        Some(Cost {
            input: c.input?,
            output: c.output?,
            cache_read: c.cache_read,
            cache_write: c.cache_write,
        })
    });
    Some(CatalogEntry {
        provider: provider.to_string(),
        name: m.name.unwrap_or_else(|| m.id.clone()),
        id: m.id,
        canonical: m.canonical_model_id,
        family: m.family,
        context,
        input: limit.input.filter(|&i| i > 0).map(to_u32),
        output,
        reasoning,
        tool_call: m.tool_call.unwrap_or(false),
        image: modalities.input.iter().any(|x| x == "image"),
        cost,
        deprecated: m.status.as_deref() == Some("deprecated"),
    })
}

fn reasoning_support(options: &[RawReasoningOption], max_output: u32) -> ReasoningSupport {
    if let Some(levels) = options
        .iter()
        .filter(|o| o.kind == "effort")
        .filter_map(|o| o.values.as_ref())
        .find(|v| !v.is_empty())
    {
        return ReasoningSupport::Levels(levels.clone());
    }
    if let Some(b) = options.iter().find(|o| o.kind == "budget_tokens") {
        return ReasoningSupport::Budget {
            min: b.min.map(to_u32).unwrap_or(1024),
            max: b.max.map(to_u32).unwrap_or(max_output),
        };
    }
    if options.iter().any(|o| o.kind == "toggle") {
        return ReasoningSupport::Toggle;
    }
    ReasoningSupport::None
}

/// 把完整的 `api.json` 裁成收录的条目（按 provider、id 排序，结果稳定）。
pub fn trim_api_json(bytes: &[u8]) -> Result<Vec<CatalogEntry>, CatalogError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let mut entries = ProvidersSeed
        .deserialize(&mut de)
        .map_err(|e| CatalogError::Parse(e.to_string()))?;
    de.end().map_err(|e| CatalogError::Parse(e.to_string()))?;
    entries.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    Ok(entries)
}

// ---- 日期换算（不引 chrono）----

/// `YYYY-MM-DD` → 当天 0 点（UTC）的 Unix 秒。
pub fn date_to_unix(date: &str) -> Option<u64> {
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant 的 days_from_civil。
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok().map(|d| d * 86_400)
}

/// Unix 秒 → `YYYY-MM-DD`（UTC）。
pub fn unix_to_date(secs: u64) -> String {
    let z = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 生成内置快照的 JSON 文本：`api` 是完整的 `api.json`，`generated` 是 `YYYY-MM-DD`。
/// 快照生成小工具（`examples/models_dev_snapshot.rs`）用它；运行时更新不走这里。
pub fn build_snapshot_json(api: &[u8], generated: &str) -> Result<String, CatalogError> {
    if date_to_unix(generated).is_none() {
        return Err(CatalogError::Invalid(format!(
            "日期格式应为 YYYY-MM-DD：{generated}"
        )));
    }
    let file = CatalogFile {
        version: FORMAT_VERSION,
        source: "models.dev".to_string(),
        generated: Some(generated.to_string()),
        fetched_at: None,
        entries: trim_api_json(api)?,
    };
    file.to_json().map_err(|e| CatalogError::Io(e.to_string()))
}
