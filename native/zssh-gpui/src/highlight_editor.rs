//! 设置页「终端 → 关键词高亮」卡：总开关 + 规则列表（模式、颜色、正则 / 区分大小写 / 涂背景、
//! 启停、删除）+ 添加规则 / 添加常用规则。对齐 0.2.14 的 `HighlightSection.tsx`。
//!
//! 规则本身归 `TerminalWorkspace`（改完它负责重编译、存盘、通知）；这里只持有每条规则的输入框
//! 和「这条规则能不能编译」的缓存。模式输入框失焦或回车时才提交：用户正在输入的正则往往还没写完，
//! 编译不了，不能每敲一个字就重编译（0.2.14 也是失焦才存）。

use std::collections::HashMap;

use ::gpui::{AnyElement, Context, Entity, SharedString, Subscription, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::highlight::{HIGHLIGHT_COLORS, HighlightRule, MAX_RULES, color_label, missing_samples};
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::main_window::widgets::{Choice, icon_button, select};
use crate::settings_view::card;
use crate::terminal_workspace::TerminalWorkspace;
use crate::typography::{Role, Typo as _};

/// 一条规则的输入框。
struct RuleInput {
    id: u64,
    input: Entity<InputState>,
    _sub: Subscription,
}

/// 编译检查的缓存：键是规则里影响编译的三项，值是错误说明（`None` = 能编译）。
type ErrorCache = HashMap<u64, ((String, bool, bool), Option<String>)>;

/// 关键词高亮卡自己的界面文案。
///
/// 规则模式、颜色名、正则库的错误原文属于用户数据或别的模块，原样显示。
#[derive(Clone, Copy, Debug)]
enum HighlightMsg {
    PatternPlaceholder,
    Enable,
    CardTitle,
    CardDescription,
    DisableRule,
    EnableRule,
    Regex,
    RegexTooltip,
    CaseSensitive,
    Background,
    BackgroundTooltip,
    Delete,
    NotEffectiveSuffix,
    AddRule,
    AddSamples,
    AddSamplesTooltipPrefix,
    MaxRulesPrefix,
    MaxRulesSuffix,
}

impl Msg for HighlightMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::PatternPlaceholder => "关键词或正则，例如 error|fail",
            Self::Enable => "开启高亮",
            Self::CardTitle => "关键词高亮",
            Self::CardDescription => {
                "把终端里看得见的关键词标上颜色（比如 error、warning）。只标屏幕上显示的那几十行，不改输出内容，大量输出时也不拖慢终端。vim、less 这类全屏程序里不标。"
            }
            Self::DisableRule => "关闭这条规则",
            Self::EnableRule => "开启这条规则",
            Self::Regex => "正则",
            Self::RegexTooltip => "按正则表达式匹配；不勾就按字面的关键词",
            Self::CaseSensitive => "区分大小写",
            Self::Background => "背景",
            Self::BackgroundTooltip => "涂背景色（不勾只改文字颜色）",
            Self::Delete => "删除",
            Self::NotEffectiveSuffix => "（这条先不生效）",
            Self::AddRule => "添加规则",
            Self::AddSamples => "添加常用规则",
            Self::AddSamplesTooltipPrefix => "补上还没有的常用规则：",
            Self::MaxRulesPrefix => "最多 ",
            Self::MaxRulesSuffix => " 条规则",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::PatternPlaceholder => "Keyword or regex, e.g. error|fail",
            Self::Enable => "Enable highlighting",
            Self::CardTitle => "Keyword Highlighting",
            Self::CardDescription => {
                "Colors keywords visible in the terminal (such as error or warning). Only the few dozen lines on screen are highlighted; output is not changed and heavy output is not slowed down. Full-screen programs such as vim or less are not highlighted."
            }
            Self::DisableRule => "Disable this rule",
            Self::EnableRule => "Enable this rule",
            Self::Regex => "Regex",
            Self::RegexTooltip => {
                "Match as a regular expression; unchecked matches the literal keyword"
            }
            Self::CaseSensitive => "Case sensitive",
            Self::Background => "Background",
            Self::BackgroundTooltip => {
                "Fill the background (unchecked changes only the text color)"
            }
            Self::Delete => "Delete",
            Self::NotEffectiveSuffix => " (this rule has no effect for now)",
            Self::AddRule => "Add Rule",
            Self::AddSamples => "Add Common Rules",
            Self::AddSamplesTooltipPrefix => "Add common rules that are missing: ",
            Self::MaxRulesPrefix => "At most ",
            Self::MaxRulesSuffix => " rules",
        }
    }
}

pub struct HighlightEditor {
    ws: Entity<TerminalWorkspace>,
    inputs: Vec<RuleInput>,
    errors: ErrorCache,
    _watch: Subscription,
}

/// 规则现在能不能编译；空模式不算错（还没填）。
pub fn rule_error(rule: &HighlightRule) -> Option<String> {
    if rule.pattern.trim().is_empty() {
        return None;
    }
    zssh_regex::compile(&rule.effective_pattern(), rule.case_sensitive)
        .err()
        .map(|err| err.to_string())
}

impl HighlightEditor {
    pub fn new(ws: Entity<TerminalWorkspace>, cx: &mut Context<Self>) -> Self {
        // 工作区变了（规则被别处改了、开关变了）这张卡跟着重画。
        let watch = cx.observe(&ws, |_this, _ws, cx| cx.notify());
        Self {
            ws,
            inputs: Vec::new(),
            errors: HashMap::new(),
            _watch: watch,
        }
    }

    /// 让输入框和工作区里的规则一一对应：新规则建输入框，没了的规则丢掉。
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rules: Vec<HighlightRule> = self.ws.read(cx).highlight_rules().to_vec();
        self.inputs
            .retain(|existing| rules.iter().any(|rule| rule.id == existing.id));
        self.errors
            .retain(|id, _| rules.iter().any(|rule| rule.id == *id));
        for rule in &rules {
            if self.inputs.iter().any(|existing| existing.id == rule.id) {
                continue;
            }
            let id = rule.id;
            let pattern = rule.pattern.clone();
            let input = cx.new(|cx| {
                let mut state =
                    InputState::new(window, cx).placeholder(t(HighlightMsg::PatternPlaceholder));
                state.set_value(pattern, window, cx);
                state
            });
            let sub = cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, _window, cx| {
                    if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                        let text = input.read(cx).value().to_string();
                        this.commit_pattern(id, text, cx);
                    }
                },
            );
            self.inputs.push(RuleInput {
                id,
                input,
                _sub: sub,
            });
        }
    }

    /// 模式输入框提交：和规则里存的不一样才写（失焦也会触发，不能每次都重编译）。
    fn commit_pattern(&mut self, id: u64, text: String, cx: &mut Context<Self>) {
        let unchanged = self
            .ws
            .read(cx)
            .highlight_rules()
            .iter()
            .find(|rule| rule.id == id)
            .is_none_or(|rule| rule.pattern == text);
        if !unchanged {
            self.ws
                .update(cx, |ws, cx| ws.set_highlight_pattern(id, text, cx));
        }
    }

    fn error_of(&mut self, rule: &HighlightRule) -> Option<String> {
        let key = (rule.pattern.clone(), rule.regex, rule.case_sensitive);
        if let Some((cached, error)) = self.errors.get(&rule.id)
            && *cached == key
        {
            return error.clone();
        }
        let error = rule_error(rule);
        self.errors.insert(rule.id, (key, error.clone()));
        error
    }

    /// 驱动脚本用：模拟在某条规则的输入框里打字再提交（和失焦 / 回车同一条路径）。
    #[cfg(debug_assertions)]
    pub fn dev_type_pattern(
        &mut self,
        index: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.sync_inputs(window, cx);
        let id = self
            .ws
            .read(cx)
            .highlight_rules()
            .get(index)
            .map(|rule| rule.id)
            .ok_or_else(|| format!("没有第 {index} 条规则"))?;
        let input = self
            .inputs
            .iter()
            .find(|existing| existing.id == id)
            .map(|existing| existing.input.clone())
            .ok_or("这条规则还没有输入框")?;
        input.update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
        self.commit_pattern(id, text.to_owned(), cx);
        Ok(())
    }
}

impl Render for HighlightEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_inputs(window, cx);
        let tokens = Tokens::current(cx);
        let (enabled, rules) = {
            let ws = self.ws.read(cx);
            (
                ws.settings().highlight_enabled,
                ws.highlight_rules().to_vec(),
            )
        };
        let toggle_ws = self.ws.clone();
        let mut rows: Vec<AnyElement> = vec![
            h_flex()
                .w_full()
                .min_h(px(28.0))
                .gap_4()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .typo(Role::ItemName, &tokens)
                        .child(t(HighlightMsg::Enable)),
                )
                .child(
                    Switch::new("highlight-enabled")
                        .checked(enabled)
                        .accessibility_label(t(HighlightMsg::Enable))
                        .on_click(move |_, _, cx| {
                            toggle_ws.update(cx, |ws, cx| ws.toggle_highlight_enabled(cx));
                        }),
                )
                .into_any_element(),
        ];
        if enabled {
            for rule in &rules {
                let error = self.error_of(rule);
                rows.push(self.rule_row(rule, error, &tokens));
            }
            rows.push(self.footer(&rules, &tokens));
        }
        card(
            "settings-highlight",
            t(HighlightMsg::CardTitle),
            Some(t(HighlightMsg::CardDescription)),
            &tokens,
            rows,
        )
    }
}

impl HighlightEditor {
    #[inline(never)]
    fn rule_row(&self, rule: &HighlightRule, error: Option<String>, tokens: &Tokens) -> AnyElement {
        let id = rule.id;
        let Some(input) = self
            .inputs
            .iter()
            .find(|existing| existing.id == id)
            .map(|existing| existing.input.clone())
        else {
            return div().into_any_element();
        };
        let (ws_on, ws_color, ws_regex, ws_case, ws_bg, ws_del) = (
            self.ws.clone(),
            self.ws.clone(),
            self.ws.clone(),
            self.ws.clone(),
            self.ws.clone(),
            self.ws.clone(),
        );
        let color = rule.color();
        let color_choices: Vec<Choice> = HIGHLIGHT_COLORS
            .iter()
            .map(|(value, label)| Choice::new(*label, *value == color))
            .collect();
        let color_name = color_label(color);
        let enabled = rule.enabled;
        v_flex()
            .id(("highlight-rule", id))
            .w_full()
            .gap_1()
            .py_1()
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        Switch::new(SharedString::from(format!("hl-on-{id}")))
                            .checked(enabled)
                            .accessibility_label(if enabled {
                                t(HighlightMsg::DisableRule)
                            } else {
                                t(HighlightMsg::EnableRule)
                            })
                            .on_click(move |_, _, cx| {
                                ws_on.update(cx, |ws, cx| ws.toggle_highlight_rule(id, cx));
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .when(!enabled, |d| d.opacity(0.5))
                            .child(Input::new(&input)),
                    )
                    .child(div().w(px(76.0)).child(select(
                        SharedString::from(format!("hl-color-{id}")),
                        color_name,
                        color_choices,
                        move |index, _, cx| {
                            if let Some((value, _)) = HIGHLIGHT_COLORS.get(index) {
                                let value = *value;
                                ws_color.update(cx, |ws, cx| ws.set_highlight_color(id, value, cx));
                            }
                        },
                    )))
                    .child(
                        Checkbox::new(SharedString::from(format!("hl-regex-{id}")))
                            .checked(rule.regex)
                            .label(t(HighlightMsg::Regex))
                            .tooltip(t(HighlightMsg::RegexTooltip))
                            .on_click(move |checked, _, cx| {
                                let checked = *checked;
                                ws_regex
                                    .update(cx, |ws, cx| ws.set_highlight_regex(id, checked, cx));
                            }),
                    )
                    .child(
                        Checkbox::new(SharedString::from(format!("hl-case-{id}")))
                            .checked(rule.case_sensitive)
                            .label("Aa")
                            .tooltip(t(HighlightMsg::CaseSensitive))
                            .on_click(move |checked, _, cx| {
                                let checked = *checked;
                                ws_case.update(cx, |ws, cx| ws.set_highlight_case(id, checked, cx));
                            }),
                    )
                    .child(
                        Checkbox::new(SharedString::from(format!("hl-bg-{id}")))
                            .checked(rule.background.is_some())
                            .label(t(HighlightMsg::Background))
                            .tooltip(t(HighlightMsg::BackgroundTooltip))
                            .on_click(move |checked, _, cx| {
                                let checked = *checked;
                                ws_bg.update(cx, |ws, cx| {
                                    ws.set_highlight_background(id, checked, cx);
                                });
                            }),
                    )
                    .child(icon_button(
                        SharedString::from(format!("hl-del-{id}")),
                        Ico::Trash,
                        t(HighlightMsg::Delete),
                        move |_, _, cx| {
                            ws_del.update(cx, |ws, cx| ws.remove_highlight_rule(id, cx));
                        },
                    )),
            )
            .children(error.map(|error| {
                div()
                    .text_xs()
                    .text_color(tokens.danger)
                    .child(format!("{error}{}", t(HighlightMsg::NotEffectiveSuffix)))
            }))
            .into_any_element()
    }

    #[inline(never)]
    fn footer(&self, rules: &[HighlightRule], tokens: &Tokens) -> AnyElement {
        let full = rules.len() >= MAX_RULES;
        let missing = missing_samples(rules);
        let (add_ws, sample_ws) = (self.ws.clone(), self.ws.clone());
        let sample_names = missing
            .iter()
            .map(HighlightRule::label)
            .collect::<Vec<_>>()
            .join("、");
        h_flex()
            .id("highlight-footer")
            .w_full()
            .gap_2()
            .pt_1()
            .items_center()
            .child(
                Button::new("highlight-add")
                    .outline()
                    .compact()
                    .icon(IconName::Plus)
                    .label(t(HighlightMsg::AddRule))
                    .disabled(full)
                    .on_click(move |_, _, cx| {
                        add_ws.update(cx, |ws, cx| {
                            ws.add_blank_highlight_rule(cx);
                        });
                    }),
            )
            .when(!missing.is_empty(), |d| {
                d.child(
                    Button::new("highlight-add-samples")
                        .ghost()
                        .compact()
                        .label(t(HighlightMsg::AddSamples))
                        .tooltip(SharedString::from(format!(
                            "{}{sample_names}",
                            t(HighlightMsg::AddSamplesTooltipPrefix)
                        )))
                        .disabled(full)
                        .on_click(move |_, _, cx| {
                            sample_ws.update(cx, |ws, cx| {
                                ws.add_sample_highlight_rules(cx);
                            });
                        }),
                )
            })
            .when(full, |d| {
                d.child(div().typo(Role::Caption, tokens).child(format!(
                    "{}{MAX_RULES}{}",
                    t(HighlightMsg::MaxRulesPrefix),
                    t(HighlightMsg::MaxRulesSuffix)
                )))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{HighlightRule, rule_error};

    #[test]
    fn an_empty_pattern_is_not_an_error_but_a_broken_regex_is() {
        let mut rule = HighlightRule::new(1, "", false);
        assert_eq!(rule_error(&rule), None, "还没填不算错");
        rule.pattern = "error|fail".to_owned();
        assert_eq!(rule_error(&rule), None);
        rule.pattern = "(".to_owned();
        assert!(rule_error(&rule).is_some(), "括号没配对");
    }

    #[test]
    fn a_keyword_rule_matches_literally_so_special_characters_are_fine() {
        let mut rule = HighlightRule::new(1, "a(b", false);
        assert!(rule_error(&rule).is_some(), "当正则是坏的");
        rule.regex = false;
        assert_eq!(rule_error(&rule), None, "当关键词就是字面的 a(b");
    }
}
