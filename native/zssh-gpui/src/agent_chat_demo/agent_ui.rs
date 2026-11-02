//! 多智能体相关的界面片段：智能体下拉、没找到 / 没登录 / 还在检测时的整块提示、顶部的
//! "版本过低"和"登录状态不确定"两条提示（0.2.14 `AIChatPanel.tsx` 的 `content` 与顶部提示）。
//!
//! 状态和文案来自 `agent_setup`（面板、设置页读同一份）；这里只管画和接线。

use ::gpui::ClickEvent;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::*;

use super::{AgentChatDemo, redact_paths};
use crate::agent_setup::{self, AgentKind, Availability, InstallState};

impl AgentChatDemo {
    /// 智能体下拉（面板顶部）：每项带"未安装 / 未登录 / 版本过低"的备注，不可用的灰掉点不了；
    /// 对话开始后换智能体 = 新开一个对话（见 [`AgentChatDemo::switch_agent`]）。
    pub(super) fn agent_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let current = self.agent;
        let items: Vec<(AgentKind, Option<&'static str>)> = AgentKind::ALL
            .iter()
            .map(|kind| (*kind, self.status.get(*kind).availability().selector_note()))
            .collect();
        Button::new("ai-agent")
            .ghost()
            .compact()
            .label(current.label())
            .dropdown_caret(true)
            .tooltip("智能体")
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu.min_w(::gpui::px(180.0));
                for (kind, note) in &items {
                    let entity = entity.clone();
                    let kind = *kind;
                    let label = match note {
                        Some(note) => format!("{}  ·  {note}", kind.label()),
                        None => kind.label().to_owned(),
                    };
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(current == kind)
                            .disabled(note.is_some())
                            .on_click(move |_: &ClickEvent, _window, cx| {
                                entity.update(cx, |this, cx| this.switch_agent(kind, cx));
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    /// 当前智能体不能用时消息区里的整块提示；能用返回 `None`。
    ///
    /// - 还在检测：一句话；
    /// - 没找到：安装方法（opencode 另有"一键下载"，在设置页），加"去设置"；
    /// - 没登录：说明加该跑的命令，底下带探测给的提示。
    pub(super) fn unavailable_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.status.get(self.agent);
        let muted = cx.theme().muted_foreground;
        let warning = cx.theme().warning;
        let centered = |children: Vec<AnyElement>| {
            v_flex()
                .id("ai-unavailable")
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .text_center()
                .children(children)
                .into_any_element()
        };
        match status.availability() {
            Availability::Ready | Availability::TooOld => None,
            Availability::Checking => Some(centered(vec![
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("正在检测 {}…", self.agent.label()))
                    .into_any_element(),
            ])),
            Availability::Missing => {
                let mut children: Vec<AnyElement> = vec![
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(agent_setup::missing_text(self.agent))
                        .into_any_element(),
                ];
                // 手动指定的路径不能用：把原因说出来（用户明确指定的坏了不会悄悄回退到自动发现）。
                if let InstallState::Invalid(reason) = &status.install {
                    children.push(
                        div()
                            .text_xs()
                            .text_color(warning)
                            .child(redact_paths(reason))
                            .into_any_element(),
                    );
                }
                children.push(
                    Button::new("ai-open-settings")
                        .label("去设置")
                        .compact()
                        .on_click(|_, _, _| agent_setup::request_settings())
                        .into_any_element(),
                );
                Some(centered(children))
            }
            Availability::NeedsLogin => {
                let (head, command) = agent_setup::status::login_required_lines(self.agent);
                let mut children: Vec<AnyElement> = vec![
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(head)
                        .into_any_element(),
                    div()
                        .text_xs()
                        .font_family("Consolas")
                        .text_color(muted)
                        .child(command)
                        .into_any_element(),
                ];
                if let Some(hint) = status.login.as_ref().and_then(|login| login.hint.clone()) {
                    children.push(
                        div()
                            .text_xs()
                            .text_color(warning)
                            .child(redact_paths(&hint))
                            .into_any_element(),
                    );
                }
                Some(centered(children))
            }
        }
    }

    /// 顶部的两条提示：版本过低（仍可用，带"下载 / 去设置"）、登录状态不确定（只提示，不拦着用）。
    pub(super) fn agent_banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let status = self.status.get(self.agent);
        let warning = cx.theme().warning;
        let mut out: Vec<AnyElement> = Vec::new();
        if status.availability() == Availability::TooOld
            && let Some(install) = status.found()
        {
            let action = if self.agent == AgentKind::OpenCode {
                "下载"
            } else {
                "去设置"
            };
            out.push(
                h_flex()
                    .id("ai-banner-too-old")
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(warning)
                    .child(format!("版本过低（{}），建议升级", install.version.trim()))
                    .child(
                        Button::new("ai-banner-settings")
                            .label(action)
                            .compact()
                            .on_click(|_, _, _| agent_setup::request_settings()),
                    )
                    .into_any_element(),
            );
        }
        if let Some(hint) = status.login_unknown_hint() {
            out.push(
                div()
                    .id("ai-banner-login-unknown")
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(warning)
                    .child(redact_paths(hint))
                    .into_any_element(),
            );
        }
        out
    }
}
