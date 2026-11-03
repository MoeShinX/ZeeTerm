//! 更新窗口的界面：顶部（图标 + 标题 + 副标题 + 小图标按钮）、中间（按状态变化的内容区）、
//! 底部（复选框 + 右对齐的按钮）。画哪一屏、写什么字、有哪些按钮由 `layout` 决定，
//! 更新说明由 `notes` 改写成安全的 Markdown 后交给 `TextView`（和 AI 面板同一个渲染器）。
//!
//! 所有颜色取自当前皮肤（`Tokens`），不写死。

use std::sync::{Arc, OnceLock};

use ::gpui::{
    AnyElement, Context, FontWeight, Image, ImageFormat, ImageSource, Render, Resource,
    SharedString, StyleRefinement, Window, img, px, rems,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::text::{SelectionFormat, TextView, TextViewStyle};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::layout::{self, Action, Screen};
use super::{
    DownloadState, InstallState, UpdateDemo, WINDOW_TITLE, command_line, file_summary, notes,
    quit_after_paint, source_label,
};
use crate::app_theme::Tokens;
use crate::main_window::titlebar::window_controls_with;
use crate::typography::{Role, Typo as _};

/// 自绘标题栏高度（和文件窗口一致）。
const TITLEBAR_HEIGHT: f32 = 40.0;

/// 应用 Logo（64px 的 PNG，显示 36px，高分屏也清楚）。
static LOGO: OnceLock<Arc<Image>> = OnceLock::new();

fn logo() -> Arc<Image> {
    LOGO.get_or_init(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../../assets/brand/zeeterm-logo-64.png").to_vec(),
        ))
    })
    .clone()
}

/// 更新内容的字号：比 AI 面板（14px）小一档，它是次要信息。
const NOTES_TEXT_SIZE: f32 = 13.0;

/// 更新内容里标题的字号：`#`、`##` 稍大一点，`###` 以下和正文同大、靠加粗区分，不抢戏。
fn notes_heading_size(level: u8) -> f32 {
    match level {
        1 | 2 => 15.0,
        _ => NOTES_TEXT_SIZE,
    }
}

impl Render for UpdateDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 锁屏（V2-U26）：锁着时整个窗口只画锁屏页；这个窗口是自绘标题栏，锁屏页自己带窗口按钮。
        if let Some(cover) = crate::app_lock::cover(&mut self.lock_screen, true, window, cx) {
            return cover;
        }
        let tokens = Tokens::current(cx);
        let screen = self.screen();
        let version = self
            .package
            .as_ref()
            .map(|package| package.version.clone())
            .unwrap_or_default();
        let published = self
            .checked
            .as_ref()
            .and_then(|checked| checked.published.clone());
        let title = layout::title(screen, &version);
        let subtitle = layout::subtitle(screen, &self.current, published.as_deref());

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            // 文字色取皮肤的色板（不写的话深色皮肤下是黑字）。
            .text_color(tokens.text)
            .child(titlebar(&tokens, window))
            .child(self.header(title, subtitle, &tokens, cx))
            .child(self.body(screen, &tokens))
            .child(self.footer(screen, &tokens, cx))
            .into_any_element()
    }
}

/// 自绘标题栏（和独立文件窗口、终端窗口同一种写法）：背景取主题色，空白处可拖动窗口，
/// 右边只放最小化 / 关闭（更新窗口不需要最大化）。系统不再画标题栏，所以深 / 浅皮肤和切换皮肤时都即时跟着变；
/// 系统标题只留给任务栏和 Alt+Tab。
fn titlebar(tokens: &Tokens, window: &Window) -> AnyElement {
    h_flex()
        .id("update-titlebar")
        .flex_none()
        .h(px(TITLEBAR_HEIGHT))
        .w_full()
        .items_center()
        .bg(tokens.background)
        .border_b_1()
        .border_color(tokens.divider)
        .child(
            h_flex()
                .id("update-titlebar-drag")
                .flex_1()
                .min_w_0()
                .h_full()
                .items_center()
                .gap_2()
                .pl_3()
                .window_control_area(WindowControlArea::Drag)
                .child(img(logo()).size(px(16.)).flex_none())
                .child(div().typo(Role::ItemName, tokens).child(WINDOW_TITLE)),
        )
        .child(div().h_full().flex_none().child(window_controls_with(
            tokens,
            window.is_maximized(),
            false,
        )))
        .into_any_element()
}

impl UpdateDemo {
    /// 顶部：图标、标题 / 副标题、右上角两个小图标按钮（更新说明 / 重新检查）。
    fn header(
        &self,
        title: String,
        subtitle: String,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let recheck = cx.entity();
        let tip = format!(
            "更新来自{}。安装包下载后会先校验签名，你确认后才会启动安装程序。",
            source_label()
        );
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_3()
            .px_5()
            .pt_5()
            .pb_3()
            .child(img(logo()).size(px(36.)).flex_none())
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().typo(Role::PageTitle, tokens).truncate().child(title))
                    .child(div().typo(Role::Caption, tokens).truncate().child(subtitle)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new("update-about-source")
                            .ghost()
                            .compact()
                            .icon(IconName::Info)
                            .tooltip(tip),
                    )
                    .child(
                        Button::new("update-recheck")
                            .ghost()
                            .compact()
                            .icon(IconName::RefreshCw)
                            .tooltip("重新检查")
                            .on_click(move |_, _, cx| {
                                recheck.update(cx, |this, cx| this.reload(cx));
                            }),
                    ),
            )
            .into_any_element()
    }

    /// 中间：按这一屏画内容。内容区自己不滚，只有"更新内容"卡片在放不下时才滚动。
    fn body(&self, screen: Screen, tokens: &Tokens) -> AnyElement {
        let body = v_flex().flex_1().min_h_0().w_full().px_5().pb_3().gap_3();
        match screen {
            Screen::Checking => body
                .items_center()
                .justify_center()
                .child(
                    Progress::new("update-checking")
                        .loading(true)
                        .color(tokens.accent)
                        .w(px(160.)),
                )
                .child(
                    div()
                        .typo(Role::Caption, tokens)
                        .child("正在向发布页询问最新版本…"),
                )
                .into_any_element(),
            Screen::Latest => body
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::CircleCheck)
                        .size_12()
                        .text_color(tokens.dot_connected),
                )
                .child(
                    div()
                        .typo(Role::Body, tokens)
                        .child("你用的已经是最新版本，无需更新。"),
                )
                .into_any_element(),
            Screen::CheckFailed => {
                let reason = self
                    .checked
                    .as_ref()
                    .map(|checked| checked.failure_reason().to_owned())
                    .unwrap_or_default();
                body.items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::CircleX)
                            .size_12()
                            .text_color(tokens.danger),
                    )
                    .child(danger_card(reason, tokens))
                    .into_any_element()
            }
            Screen::Found
            | Screen::Downloading
            | Screen::Verifying
            | Screen::Verified
            | Screen::DownloadFailed => body
                .children(self.status_block(screen, tokens))
                .children(self.notices(tokens))
                .child(self.notes_section(tokens))
                .into_any_element(),
        }
    }

    /// 更新内容卡片上面的那一块：下载进度 / 签名已验证 / 失败原因 / 取消提示。
    fn status_block(&self, screen: Screen, tokens: &Tokens) -> Option<AnyElement> {
        match (screen, &self.phase) {
            (Screen::Downloading, DownloadState::Downloading { received, total }) => Some(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(progress_bar(
                        layout::progress_percent(*received, *total),
                        tokens,
                    ))
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .child(layout::progress_line(*received, *total, self.speed.rate())),
                    )
                    .into_any_element(),
            ),
            (Screen::Verifying, _) => Some(
                v_flex()
                    .flex_none()
                    .gap_2()
                    .child(progress_bar(None, tokens))
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .child("下载完成，正在校验签名…"),
                    )
                    .into_any_element(),
            ),
            (Screen::Verified, DownloadState::Verified { path, size, .. }) => {
                let summary = SharedString::from(file_summary(path, *size));
                Some(
                    h_flex()
                        .id("update-verified")
                        .flex_none()
                        .gap_2()
                        .items_center()
                        .tooltip(move |window, cx| Tooltip::new(summary.clone()).build(window, cx))
                        .child(
                            Icon::new(IconName::CircleCheck)
                                .size_4()
                                .text_color(tokens.dot_connected),
                        )
                        .child(div().typo(Role::ItemName, tokens).child("签名已验证"))
                        .into_any_element(),
                )
            }
            (Screen::DownloadFailed, DownloadState::Failed(message)) => {
                Some(danger_card(message.clone(), tokens))
            }
            (Screen::Found, DownloadState::Cancelled) => self.phase.text().map(|text| {
                div()
                    .flex_none()
                    .typo(Role::Caption, tokens)
                    .child(text)
                    .into_any_element()
            }),
            _ => None,
        }
    }

    /// 安装相关的提示行：等确认、已启动、没能准备、被拦下、没存成跳过。
    fn notices(&self, tokens: &Tokens) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        let line = |text: String, color| {
            div()
                .flex_none()
                .typo(Role::Caption, tokens)
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        match &self.install {
            InstallState::Idle => {}
            InstallState::Confirming(plan) => {
                notices.push(line(
                    "再点一次「确认安装」：ZeeTerm 会退出，并运行已验证的安装程序。".to_owned(),
                    tokens.text_muted,
                ));
                notices.push(line(command_line(plan), tokens.text_faint));
            }
            InstallState::Launched => notices.push(line(
                "安装器已启动，程序即将退出".to_owned(),
                tokens.text_muted,
            )),
            InstallState::Failed(err) => notices.push(line(err.clone(), tokens.danger)),
        }
        if let Some(text) = &self.queued {
            notices.push(line(text.clone(), tokens.text_muted));
        }
        if let Some(text) = &self.blocked {
            notices.push(line(text.clone(), tokens.danger));
        }
        if let Some(text) = &self.skip_error {
            notices.push(line(text.clone(), tokens.danger));
        }
        notices
    }

    /// "更新内容"小标题 + 圆角卡片：卡片高度按内容，最多到可用高度，再多就在卡片里滚动。
    fn notes_section(&self, tokens: &Tokens) -> AnyElement {
        let content = match &self.notes_markdown {
            Some(markdown) => notes_view(markdown.clone(), tokens).into_any_element(),
            None => div()
                .typo(Role::Caption, tokens)
                .child(notes::EMPTY_NOTES)
                .into_any_element(),
        };
        v_flex()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .flex_none()
                    .typo(Role::FieldLabel, tokens)
                    .child("更新内容"),
            )
            .child(
                div()
                    .id("update-notes")
                    .min_h_0()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .p_3()
                    .child(content)
                    .overflow_y_scrollbar(),
            )
            .into_any_element()
    }

    /// 底部：左边是"安装后重启"复选框，右边是右对齐的按钮。
    fn footer(&self, screen: Screen, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let launched = matches!(self.install, InstallState::Launched);
        let specs = if launched {
            Vec::new()
        } else {
            layout::buttons(screen, self.install.awaiting_confirm())
        };
        let entity = cx.entity();
        let restart = self.restart;
        let toggle = entity.clone();

        let mut row = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap_2()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(tokens.divider)
            .when(layout::shows_restart_choice(screen) && !launched, |row| {
                row.child(
                    Checkbox::new("update-restart")
                        .label("安装后重启 ZeeTerm")
                        .checked(restart)
                        .on_click(move |_, _, cx| {
                            toggle.update(cx, |this, cx| this.toggle_restart(cx));
                        }),
                )
            })
            .child(div().flex_1());

        for (index, spec) in specs.into_iter().enumerate() {
            let entity = entity.clone();
            let action = spec.action;
            let mut button = Button::new(("update-action", index))
                .label(spec.label)
                .on_click(move |_, window, cx| {
                    let installing =
                        entity.update(cx, |this, cx| this.run_action(action, window, cx));
                    if installing {
                        quit_after_paint(cx);
                    }
                });
            button = if spec.primary {
                button.primary()
            } else if spec.text_only {
                button.ghost()
            } else {
                button.outline()
            };
            row = row.child(button);
        }
        row.into_any_element()
    }

    /// 按钮的动作。返回 `true` 表示安装器已经启动、调用方要安排程序退出（只有「立即安装」会）。
    fn run_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match action {
            Action::Close => self.close_window(window),
            Action::RecheckAgain => self.reload(cx),
            Action::RetryDownload | Action::Download => self.start_download(cx),
            Action::Skip => self.skip_version(window, cx),
            Action::Later => self.later(window, cx),
            Action::Cancel => self.cancel_download(cx),
            Action::Install => return self.click_install(cx),
        }
        false
    }
}

/// 进度条；总大小未知时画"不确定进度"的来回滑动。
fn progress_bar(percent: Option<f32>, tokens: &Tokens) -> AnyElement {
    let bar = Progress::new("update-progress")
        .color(tokens.accent)
        .w_full();
    match percent {
        Some(percent) => bar.value(percent),
        None => bar.loading(true),
    }
    .into_any_element()
}

/// 红色提示卡片（浅红底 + 红边 + 红字）：检查失败、下载 / 验签失败的原因。
fn danger_card(text: String, tokens: &Tokens) -> AnyElement {
    div()
        .flex_none()
        .max_w_full()
        .rounded_lg()
        .border_1()
        .border_color(tokens.danger.opacity(0.5))
        .bg(tokens.danger.opacity(0.08))
        .px_3()
        .py_2()
        .typo(Role::Body, tokens)
        .text_color(tokens.danger)
        .child(text)
        .into_any_element()
}

/// 更新说明的 Markdown 视图：和 AI 面板同一个 `TextView`，字号小一档，可选中复制。
///
/// 安全：内容已经过 `notes::sanitize_markdown`（无 HTML / 图片 / 非 https 链接）；这里再加两道——
/// 图片地址一律指向不存在的内嵌资源（既不联网也不读本地文件）；链接点击只认 https，
/// 过 core 的 `validate_external_url` 后用系统浏览器打开，其余什么都不做。
fn notes_view(markdown: SharedString, tokens: &Tokens) -> TextView {
    TextView::markdown("update-notes-markdown", markdown)
        .text_size(px(NOTES_TEXT_SIZE))
        .text_color(tokens.text)
        .selectable(true)
        .selection_format(SelectionFormat::Source)
        .style(
            TextViewStyle::default()
                .paragraph_gap(rems(0.5))
                .heading_font_size(|level, _| px(notes_heading_size(level)))
                // 表头文字取主文字色：组件库默认的表头色在深色皮肤下太淡。
                .table_head(
                    StyleRefinement::default()
                        .text_color(tokens.text)
                        .font_weight(FontWeight::SEMIBOLD),
                ),
        )
        .image_source(|_| ImageSource::Resource(Resource::Embedded("zssh-no-remote-images".into())))
        .on_link_click(move |url, event, _, _cx| {
            if !crate::agent_chat_demo::link_click_opens(event) {
                return;
            }
            if let Some(target) = notes::link_target(url) {
                // 打不开（没有浏览器等）不打扰用户：更新窗口里没有地方放这条错误。
                let _ = zssh_core::external_url::open_external_url(&target);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::{NOTES_TEXT_SIZE, notes_heading_size};

    /// 本文件里"生产代码"那一半（去掉测试模块）。
    fn production_source() -> &'static str {
        let source = include_str!("view.rs");
        source
            .split("#[cfg(test)]")
            .next()
            .expect("至少有一段生产代码")
    }

    #[test]
    fn notes_are_one_step_smaller_than_the_ai_panel_and_headings_do_not_shout() {
        // AI 面板正文是 14px（text_sm）。
        const { assert!(NOTES_TEXT_SIZE < 14.0) };
        for level in 1..=6 {
            let size = notes_heading_size(level);
            assert!(size >= NOTES_TEXT_SIZE, "标题不小于正文：{level}");
            assert!(size <= 16.0, "标题不能大到喧宾夺主：{level} 级 {size}px");
        }
        assert_eq!(notes_heading_size(3), NOTES_TEXT_SIZE, "### 只靠加粗区分");
    }

    #[test]
    fn the_notes_view_blocks_remote_images_and_routes_links_through_the_https_gate() {
        let body = production_source();
        // 图片：每个 TextView 都得覆盖 image_source，指向不存在的内嵌资源。
        assert!(body.contains(".image_source("));
        assert!(body.contains("zssh-no-remote-images"));
        // 链接：只经 notes::link_target（https + validate_external_url）再 open_external_url，
        // 不能有任何直接打开网址的路径（TextView 没挂 handler 时会走 App::open_url）。
        assert!(body.contains(".on_link_click("));
        assert!(body.contains("notes::link_target(url)"));
        assert!(body.contains("open_external_url(&target)"));
        let opens_directly = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| line.contains("open_url("));
        assert!(!opens_directly, "更新窗口里不该有直接打开网址的路径");
        // 内容必须先过改写才进 TextView。
        let update_demo = include_str!("../update_demo.rs");
        assert!(update_demo.contains("notes::display_markdown(checked.notes.as_deref())"));
    }

    #[test]
    fn the_title_bar_is_self_drawn_from_theme_tokens_with_minimize_and_close_only() {
        let body = production_source();
        // 第三个参数 false = 不放最大化按钮（rustfmt 会把调用拆成多行，所以去掉空白再比）。
        let squeezed: String = body.split_whitespace().collect();
        assert!(squeezed.contains("window_controls_with(tokens,window.is_maximized(),false,)"));
        assert!(body.contains("WindowControlArea::Drag"));
        assert!(body.contains(".bg(tokens.background)"));
        // 系统标题栏要关掉：窗口选项里走和文件窗口同一个 `title_bar_options`。
        let window_options = include_str!("../../src/update_demo.rs");
        assert!(window_options.contains("TitleBar::title_bar_options()"));
    }

    #[test]
    fn the_update_source_sentence_is_not_a_permanent_line() {
        let body = production_source();
        // 技术说明挪进了右上角信息图标的悬停提示：不再是一行常驻文字。
        assert!(body.contains("update-about-source"));
        assert!(!body.contains(".child(format!(\"更新源："));
    }
}
