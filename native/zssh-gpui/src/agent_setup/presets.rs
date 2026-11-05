//! 设置 → AI → 内置助手 →「添加接口」里的预设列表（V2-A5）。
//!
//! Tokenway 排第一，带"推荐"和一句话介绍（地址和文案来自验过签的清单或内置清单，见
//! [`super::tokenway`]）；后面是常见的几家和"自定义"，**一个都不隐藏、不降级**。非 Tokenway 的预设
//! 只是帮用户预填名称 / 类型 / 地址的表单起点（用户在表单里随便改），不涉及任何一键授权。

use zssh_core::ai::agent::builtin::config::EndpointKind;
use zssh_core::ai::agent::builtin::tokenway::Preset;

use crate::i18n::{Msg, t};

/// 本文件（添加接口时的预设列表）自己的界面文案。
///
/// 只放应用自己的固定文案；服务商名、地址和清单里带来的介绍一律原样显示，不进这张表。
#[derive(Clone, Copy, Debug)]
enum PresetsMsg {
    Custom,
    TaglineOpenAi,
    TaglineAnthropic,
    TaglineDeepSeek,
    TaglineOpenRouter,
    TaglineOllama,
    TaglineCustom,
    TaglineTokenwayFallback,
}

impl Msg for PresetsMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::Custom => "自定义",
            Self::TaglineOpenAi => "OpenAI 官方接口",
            Self::TaglineAnthropic => "Claude 官方接口（Messages）",
            Self::TaglineDeepSeek => "DeepSeek 官方接口",
            Self::TaglineOpenRouter => "一个 Key 用很多家的模型",
            Self::TaglineOllama => "本机运行的模型，不需要 Key",
            Self::TaglineCustom => "OpenAI 兼容或 Anthropic 接口，地址自己填",
            Self::TaglineTokenwayFallback => "浏览器一键授权，不用复制 Key",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::Custom => "Custom",
            Self::TaglineOpenAi => "Official OpenAI API",
            Self::TaglineAnthropic => "Official Claude API (Messages)",
            Self::TaglineDeepSeek => "Official DeepSeek API",
            Self::TaglineOpenRouter => "Many providers' models with one key",
            Self::TaglineOllama => "Models running locally; no key needed",
            Self::TaglineCustom => "OpenAI-compatible or Anthropic API with your own URL",
            Self::TaglineTokenwayFallback => {
                "One-click authorization in the browser; no key to copy"
            }
        }
    }
}

/// 非 Tokenway 的预设：预填表单用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPreset {
    /// 存进接口的 `preset_id`（自定义是 `None`）。
    pub id: Option<&'static str>,
    pub name: &'static str,
    pub tagline: &'static str,
    pub kind: EndpointKind,
    /// 预填的 Base URL（自定义为空）。
    pub base_url: &'static str,
    /// 本机服务不校验 Key（Ollama）：预填「不需要 Key」。
    pub no_key: bool,
}

/// 列表里 Tokenway 后面的预设，按显示顺序。
pub fn local_presets() -> Vec<LocalPreset> {
    vec![
        LocalPreset {
            id: Some("openai"),
            name: "OpenAI",
            tagline: t(PresetsMsg::TaglineOpenAi),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.openai.com/v1",
            no_key: false,
        },
        LocalPreset {
            id: Some("anthropic"),
            name: "Anthropic",
            tagline: t(PresetsMsg::TaglineAnthropic),
            kind: EndpointKind::Anthropic,
            base_url: "https://api.anthropic.com",
            no_key: false,
        },
        LocalPreset {
            id: Some("deepseek"),
            name: "DeepSeek",
            tagline: t(PresetsMsg::TaglineDeepSeek),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.deepseek.com/v1",
            no_key: false,
        },
        LocalPreset {
            id: Some("openrouter"),
            name: "OpenRouter",
            tagline: t(PresetsMsg::TaglineOpenRouter),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://openrouter.ai/api/v1",
            no_key: false,
        },
        LocalPreset {
            id: Some("ollama"),
            name: "Ollama",
            tagline: t(PresetsMsg::TaglineOllama),
            kind: EndpointKind::OpenAiChat,
            base_url: "http://localhost:11434/v1",
            no_key: true,
        },
        LocalPreset {
            id: None,
            name: t(PresetsMsg::Custom),
            tagline: t(PresetsMsg::TaglineCustom),
            kind: EndpointKind::OpenAiChat,
            base_url: "",
            no_key: false,
        },
    ]
}

/// 列表里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetRow {
    /// Tokenway：推荐项，走一键连接。
    Tokenway {
        name: String,
        tagline: String,
        recommended: bool,
    },
    Local(LocalPreset),
}

impl PresetRow {
    pub fn title(&self) -> &str {
        match self {
            Self::Tokenway { name, .. } => name,
            Self::Local(preset) => preset.name,
        }
    }

    pub fn tagline(&self) -> &str {
        match self {
            Self::Tokenway { tagline, .. } => tagline,
            Self::Local(preset) => preset.tagline,
        }
    }

    pub fn recommended(&self) -> bool {
        matches!(
            self,
            Self::Tokenway {
                recommended: true,
                ..
            }
        )
    }
}

/// 「添加接口」列表：Tokenway 在第一位（清单里有的话），其余在后面。
pub fn preset_rows(tokenway: Option<&Preset>) -> Vec<PresetRow> {
    let mut rows = Vec::new();
    if let Some(preset) = tokenway {
        rows.push(PresetRow::Tokenway {
            name: preset.name.clone(),
            tagline: preset
                .tagline
                .clone()
                .unwrap_or_else(|| t(PresetsMsg::TaglineTokenwayFallback).to_owned()),
            recommended: preset.recommended,
        });
    }
    rows.extend(local_presets().into_iter().map(PresetRow::Local));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ai::agent::builtin::tokenway::builtin_manifest;

    #[test]
    fn tokenway_is_first_and_recommended_and_the_others_are_all_kept() {
        let manifest = builtin_manifest();
        let rows = preset_rows(manifest.tokenway());
        assert!(rows.len() >= 7, "Tokenway + 五家 + 自定义");
        assert!(matches!(&rows[0], PresetRow::Tokenway { .. }));
        assert!(rows[0].recommended());
        assert_eq!(rows[0].title(), "Tokenway");
        assert_eq!(rows[0].tagline(), "国内直连，按量计费");
        // 其余都不是推荐项，一个不少。
        let names: Vec<&str> = rows[1..].iter().map(PresetRow::title).collect();
        for expected in [
            "OpenAI",
            "Anthropic",
            "DeepSeek",
            "OpenRouter",
            "Ollama",
            "自定义",
        ] {
            assert!(names.contains(&expected), "缺 {expected}：{names:?}");
        }
        assert!(rows[1..].iter().all(|row| !row.recommended()));
        assert_eq!(*names.last().unwrap(), "自定义", "自定义放最后");
    }

    #[test]
    fn without_a_tokenway_entry_the_rest_still_show() {
        let rows = preset_rows(None);
        assert!(rows.iter().all(|row| matches!(row, PresetRow::Local(_))));
        assert_eq!(rows.len(), local_presets().len());
    }

    #[test]
    fn local_presets_prefill_a_valid_form_start() {
        for preset in local_presets() {
            if let Some(id) = preset.id {
                assert!(id.chars().all(|c| c.is_ascii_lowercase()), "{id}");
                assert!(
                    zssh_core::ai::agent::builtin::config::valid_base_url(preset.base_url),
                    "{}",
                    preset.name
                );
            } else {
                assert!(preset.base_url.is_empty(), "自定义不预填地址");
            }
            // 只有 OpenAI 兼容的可以预填「不需要 Key」。
            assert!(!preset.no_key || preset.kind == EndpointKind::OpenAiChat);
            // 不和 Tokenway 的两个 preset_id 撞。
            assert!(!matches!(preset.id, Some("tokenway" | "tokenway_key")));
        }
        let ollama = local_presets()
            .into_iter()
            .find(|p| p.id == Some("ollama"))
            .unwrap();
        assert!(ollama.no_key && ollama.base_url.starts_with("http://localhost"));
    }

    #[test]
    fn tokenway_without_a_tagline_gets_a_neutral_default() {
        let mut preset = builtin_manifest().tokenway().unwrap().clone();
        preset.tagline = None;
        preset.recommended = false;
        let rows = preset_rows(Some(&preset));
        assert!(!rows[0].tagline().is_empty());
        assert!(!rows[0].recommended());
    }

    #[test]
    fn every_preset_message_has_both_languages() {
        use crate::i18n::Msg as _;
        for key in [
            PresetsMsg::Custom,
            PresetsMsg::TaglineOpenAi,
            PresetsMsg::TaglineAnthropic,
            PresetsMsg::TaglineDeepSeek,
            PresetsMsg::TaglineOpenRouter,
            PresetsMsg::TaglineOllama,
            PresetsMsg::TaglineCustom,
            PresetsMsg::TaglineTokenwayFallback,
        ] {
            assert!(!key.zh().is_empty(), "{key:?} 缺中文");
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }
}
