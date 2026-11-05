//! AI 面板顶部的「模型」「思考强度」两个选择（0.2.14 `ai/modelPicker.ts` + `modelOptions.ts`）：
//! 纯函数，不依赖 GPUI。
//!
//! 后端的模型目录是平铺的：「模型」一项，加上「模型/强度」若干项（Claude Code 是这个约定，名字是
//! 「模型名 · 强度」）。这里按模型分组：第一个框选模型，第二个框选这个模型支持的强度。
//! 发给后端的仍是平铺的那个 id（`模型` 或 `模型/强度`）。

use zssh_core::ai::agent::AgentModelCatalog;

use crate::i18n::t;

/// 没有智能体默认 id 时给「默认」那一项用的占位值：发送时不带模型。
pub const DEFAULT_VALUE: &str = "__zssh_default__";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelGroup {
    /// 模型本身的 id（不带强度）。
    pub value: String,
    pub label: String,
    pub unavailable: bool,
    pub is_default: bool,
    /// 能不能不指定强度（= 用智能体自己的默认强度）。
    pub plain: bool,
    /// 这个模型支持的强度，按目录里的顺序。
    pub efforts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelPicker {
    pub groups: Vec<ModelGroup>,
    /// 初始选中的平铺 id。
    pub initial: String,
}

/// 「默认」那一项的文字。
pub fn default_text(actual: Option<&str>) -> String {
    match actual.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!(
            "{}{name}{}",
            t(ModelPickerMsg::DefaultWithPrefix),
            t(ModelPickerMsg::DefaultWithSuffix)
        ),
        None => t(ModelPickerMsg::DefaultModel).to_owned(),
    }
}

/// `模型/强度` + `模型名 · 强度` 才算强度变体（`provider/model` 不是）。
fn split_effort<'a>(id: &'a str, name: &'a str) -> Option<(&'a str, &'a str, &'a str)> {
    let slash = id.rfind('/')?;
    let dot = name.rfind(" · ")?;
    if slash == 0 || dot == 0 {
        return None;
    }
    let effort = &id[slash + 1..];
    if effort.is_empty() || &name[dot + " · ".len()..] != effort {
        return None;
    }
    Some((&id[..slash], &name[..dot], effort))
}

pub fn build(catalog: Option<&AgentModelCatalog>) -> ModelPicker {
    let Some(catalog) = catalog else {
        return ModelPicker::default();
    };
    let mut groups: Vec<ModelGroup> = Vec::new();
    fn group_of<'a>(
        groups: &'a mut Vec<ModelGroup>,
        value: &str,
        label: &str,
    ) -> &'a mut ModelGroup {
        if let Some(index) = groups.iter().position(|group| group.value == value) {
            return &mut groups[index];
        }
        groups.push(ModelGroup {
            value: value.to_owned(),
            label: label.to_owned(),
            unavailable: false,
            is_default: false,
            plain: false,
            efforts: Vec::new(),
        });
        let last = groups.len() - 1;
        &mut groups[last]
    }
    for model in &catalog.models {
        if let Some((base, label, effort)) = split_effort(&model.id, &model.name) {
            let group = group_of(&mut groups, base, label);
            group.efforts.push(effort.to_owned());
            group.unavailable |= model.unavailable;
        } else {
            let group = group_of(&mut groups, &model.id, &model.name);
            group.plain = true;
            group.label = model.name.clone();
            group.unavailable |= model.unavailable;
        }
    }

    let default_id = catalog.default.as_deref();
    let head = match default_id {
        None => ModelGroup {
            value: DEFAULT_VALUE.to_owned(),
            label: default_text(catalog.default_label.as_deref()),
            unavailable: false,
            is_default: true,
            plain: true,
            efforts: Vec::new(),
        },
        Some(default_id) => {
            let own = groups.iter().position(|group| {
                group.value == default_id
                    || group
                        .efforts
                        .iter()
                        .any(|effort| format!("{}/{effort}", group.value) == default_id)
            });
            match own {
                Some(index) => {
                    let mut own = groups.remove(index);
                    // Claude Code 的 `default` 名字是「默认（Claude Code 的设置）」，不是模型名
                    let actual = catalog
                        .default_label
                        .clone()
                        .or_else(|| (own.value != "default").then(|| own.label.clone()));
                    own.is_default = true;
                    own.label = default_text(actual.as_deref());
                    own
                }
                None => ModelGroup {
                    value: default_id.to_owned(),
                    label: default_text(Some(
                        catalog.default_label.as_deref().unwrap_or(default_id),
                    )),
                    unavailable: false,
                    is_default: true,
                    plain: true,
                    efforts: Vec::new(),
                },
            }
        }
    };
    let initial = default_id.unwrap_or(DEFAULT_VALUE).to_owned();
    let mut all = vec![head];
    all.extend(groups);
    ModelPicker {
        groups: all,
        initial,
    }
}

/// 平铺 id → 选中的模型（下标）和强度（`None` = 不指定强度）。找不到返回 `None`。
pub fn split_selection(value: &str, groups: &[ModelGroup]) -> Option<(usize, Option<String>)> {
    for (index, group) in groups.iter().enumerate() {
        if group.plain && group.value == value {
            return Some((index, None));
        }
        for effort in &group.efforts {
            if format!("{}/{effort}", group.value) == value {
                return Some((index, Some(effort.clone())));
            }
        }
    }
    None
}

/// 选中的模型和强度 → 平铺 id。
pub fn join_selection(group: &ModelGroup, effort: Option<&str>) -> String {
    match effort {
        None => group.value.clone(),
        Some(effort) => format!("{}/{effort}", group.value),
    }
}

/// 这个模型的强度框里能选的强度（`None` = 「默认强度」）。
pub fn effort_choices(group: &ModelGroup) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = Vec::new();
    if group.plain {
        out.push(None);
    }
    out.extend(group.efforts.iter().cloned().map(Some));
    out
}

/// 换了模型之后用哪个强度：想要的强度这个模型支持就保持不变；不支持时能不指定就不指定
/// （用智能体自己的默认），否则优先 medium，再不行取第一个。
pub fn pick_effort(group: &ModelGroup, wanted: Option<&str>) -> Option<String> {
    let choices = effort_choices(group);
    if let Some(found) = choices.iter().find(|choice| choice.as_deref() == wanted) {
        return found.clone();
    }
    if group.plain {
        return None;
    }
    if group.efforts.iter().any(|effort| effort == "medium") {
        return Some("medium".to_owned());
    }
    group.efforts.first().cloned()
}

/// 强度的显示文字。
pub fn effort_label(effort: Option<&str>) -> String {
    match effort {
        None => t(ModelPickerMsg::DefaultEffort).to_owned(),
        Some("xhigh") => "XHigh".to_owned(),
        Some(effort) => {
            let mut chars = effort.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// 模型 / 强度选择框里的「默认」文案。
#[derive(Clone, Copy, Debug)]
#[allow(clippy::enum_variant_names)]
enum ModelPickerMsg {
    /// 「默认（」这类前缀：中间夹模型名。
    DefaultWithPrefix,
    /// 模型名后面的右括号。
    DefaultWithSuffix,
    DefaultModel,
    DefaultEffort,
}

impl crate::i18n::Msg for ModelPickerMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::DefaultWithPrefix => "默认（",
            Self::DefaultWithSuffix => "）",
            Self::DefaultModel => "默认模型",
            Self::DefaultEffort => "默认强度",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::DefaultWithPrefix => "Default (",
            Self::DefaultWithSuffix => ")",
            Self::DefaultModel => "Default model",
            Self::DefaultEffort => "Default effort",
        }
    }
}

/// 换了目录（新对话、续接）后的选中值：原来选的还在就留着；不在就用初始模型，
/// 强度按 [`pick_effort`] 尽量保持用户之前选的。
pub fn selection_after_catalog(
    current: &str,
    picker: &ModelPicker,
    wanted_effort: Option<&str>,
) -> String {
    if !current.is_empty() && split_selection(current, &picker.groups).is_some() {
        return current.to_owned();
    }
    let initial = split_selection(&picker.initial, &picker.groups);
    let (index, effort) = match &initial {
        Some((index, effort)) => (*index, effort.as_deref()),
        None => (0, None),
    };
    let Some(group) = picker.groups.get(index) else {
        return picker.initial.clone();
    };
    join_selection(
        group,
        pick_effort(group, wanted_effort.or(effort)).as_deref(),
    )
}

/// 发送时带的模型：占位的默认值不带（智能体用自己的默认）。
pub fn model_for_send(value: &str) -> Option<String> {
    (!value.is_empty() && value != DEFAULT_VALUE).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ai::agent::AgentModel;

    fn model(id: &str, name: &str) -> AgentModel {
        AgentModel {
            id: id.into(),
            name: name.into(),
            unavailable: false,
        }
    }

    fn claude_catalog() -> AgentModelCatalog {
        AgentModelCatalog {
            models: vec![
                model("default", "默认（Claude Code 的设置）"),
                model("opus/low", "Opus · low"),
                model("opus/high", "Opus · high"),
                model("sonnet/low", "Sonnet · low"),
                model("sonnet/medium", "Sonnet · medium"),
                model("haiku", "Haiku"),
            ],
            default: Some("default".into()),
            default_label: Some("opus · high".into()),
        }
    }

    #[test]
    fn groups_efforts_by_model_and_puts_default_first() {
        let picker = build(Some(&claude_catalog()));
        let names: Vec<_> = picker.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(
            names,
            vec!["默认（opus · high）", "Opus", "Sonnet", "Haiku"]
        );
        assert!(picker.groups[0].is_default && picker.groups[0].plain);
        assert_eq!(picker.groups[1].efforts, vec!["low", "high"]);
        assert_eq!(picker.initial, "default");
        assert!(build(None).groups.is_empty());
    }

    #[test]
    fn without_a_default_id_a_placeholder_is_sent_as_no_model() {
        let catalog = AgentModelCatalog {
            models: vec![model("a/b", "A B")],
            default: None,
            default_label: None,
        };
        let picker = build(Some(&catalog));
        assert_eq!(picker.groups[0].label, "默认模型");
        assert_eq!(picker.initial, DEFAULT_VALUE);
        assert_eq!(model_for_send(&picker.initial), None);
        assert_eq!(model_for_send("opus/high"), Some("opus/high".into()));
        assert_eq!(model_for_send(""), None);
        assert!(
            !picker.groups[1].plain || picker.groups[1].efforts.is_empty(),
            "provider/model 不是强度变体"
        );
    }

    #[test]
    fn selection_round_trips_through_the_flat_id() {
        let picker = build(Some(&claude_catalog()));
        let (index, effort) = split_selection("opus/high", &picker.groups).expect("在目录里");
        assert_eq!(picker.groups[index].value, "opus");
        assert_eq!(effort.as_deref(), Some("high"));
        assert_eq!(
            join_selection(&picker.groups[index], effort.as_deref()),
            "opus/high"
        );
        assert_eq!(
            split_selection("haiku", &picker.groups).map(|s| s.1),
            Some(None)
        );
        assert!(split_selection("gone/x", &picker.groups).is_none());
    }

    #[test]
    fn effort_follows_the_model_when_it_can() {
        let picker = build(Some(&claude_catalog()));
        let opus = &picker.groups[1];
        let sonnet = &picker.groups[2];
        let haiku = &picker.groups[3];
        assert_eq!(pick_effort(opus, Some("high")), Some("high".into()));
        assert_eq!(
            pick_effort(sonnet, Some("high")),
            Some("medium".into()),
            "不支持就优先 medium"
        );
        assert_eq!(pick_effort(haiku, Some("high")), None, "能不指定就不指定");
        assert_eq!(effort_choices(haiku), vec![None]);
        assert_eq!(
            effort_choices(opus),
            vec![Some("low".into()), Some("high".into())]
        );
        assert_eq!(effort_label(None), "默认强度");
        assert_eq!(effort_label(Some("xhigh")), "XHigh");
        assert_eq!(effort_label(Some("medium")), "Medium");
    }

    #[test]
    fn a_new_catalog_keeps_the_choice_that_is_still_there() {
        let picker = build(Some(&claude_catalog()));
        assert_eq!(
            selection_after_catalog("sonnet/low", &picker, None),
            "sonnet/low"
        );
        assert_eq!(selection_after_catalog("gone/x", &picker, None), "default");
        assert_eq!(selection_after_catalog("", &picker, None), "default");
    }
}
