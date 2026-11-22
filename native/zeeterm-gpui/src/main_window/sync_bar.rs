//! 底部同步状态条（0.2.14 `SyncStatusBar.tsx`）：连接了存储才出现，一行小字 + 一个状态点；
//! 失败原因只在这里和设置页的同步一节显示，不弹窗；点一下打开设置页并滚到「同步」那一节。
//!
//! 文案和颜色的规则在 `sync::status`（纯函数，有单测），这里只画和接线。

use ::gpui::{Hsla, px};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use crate::app_theme::Tokens;
use crate::sync::SyncTone;
use crate::typography::{Role, Typo as _};

fn dot_color(tone: SyncTone, tokens: &Tokens) -> Hsla {
    match tone {
        SyncTone::Idle => tokens.dot_idle,
        SyncTone::Ok => tokens.dot_connected,
        SyncTone::Busy => tokens.accent,
        SyncTone::Warn => tokens.dot_connecting,
        SyncTone::Error => tokens.dot_disconnected,
    }
}

impl MainWindow {
    /// 每个节拍看一眼同步状态有没有变；变了就要重绘。
    pub(super) fn sync_sync_bar(&mut self) -> bool {
        let latest = crate::sync::bar_model();
        if latest == self.sync_bar {
            return false;
        }
        self.sync_bar = latest;
        true
    }

    /// 没连存储时整条隐藏，不占地方。
    pub(super) fn render_sync_bar(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let model = &self.sync_bar.1;
        if !model.visible {
            return None;
        }
        let entity = cx.entity();
        let hover = tokens.hover;
        Some(
            h_flex()
                .id("sync-status-bar")
                .h(px(24.0))
                .w_full()
                .flex_none()
                .px_3()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.divider)
                .bg(tokens.background)
                .typo(Role::Caption, tokens)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    div()
                        .flex_none()
                        .size(px(8.0))
                        .rounded_full()
                        .bg(dot_color(model.tone, tokens)),
                )
                .child(div().min_w_0().truncate().child(model.text.clone()))
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.open_sync_settings(window, cx));
                }),
        )
    }

    /// 打开设置页并切到「同步」分类（状态条点击的去向，同 0.2.14 的 `wantSettingsSection`）。
    pub(crate) fn open_sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_view(MainView::Settings, window, cx);
        let section = crate::settings_nav::SettingsSection::on_open(self.settings_section, true);
        self.set_settings_section(section, cx);
    }
}
