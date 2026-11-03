//! 从 Z-SSH 迁移的入口（V2-U19）：欢迎页上方的提示卡、按钮行下面的文字链接，以及设置页「同步」
//! 最上面那一块的创建和打开。迁移本身的逻辑和画法在 `crate::migrate_demo`，这里只管入口和主窗口的接线。
//!
//! 提示卡的出现条件见 [`crate::migrate_demo::tip_visible`]：本机有 Z-SSH 数据、ZeeTerm 的主机库还是空的、
//! 没点过「不再提示」。找 Z-SSH 数据只看文件在不在（后台线程里 stat），不打开库。

use std::rc::Rc;

use ::gpui::{Window, px};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::bridge::on_blocking;
use super::nav::MainView;
use super::widgets::{ButtonLook, act, text_button};
use crate::app_theme::Tokens;
use crate::migrate_demo::{self, MigrateDemo, MigrateNotice};
use crate::typography::{Role, Typo as _};

/// 提示卡的标题和说明。
pub const TIP_TITLE: &str = "检测到 Z-SSH 的数据";
pub const TIP_TEXT: &str =
    "一键导入到 ZeeTerm：先预览，导入前会自动备份，不会改动 Z-SSH 里的数据。";
/// 欢迎页按钮行下面的链接文字。
pub const WELCOME_LINK: &str = "从 Z-SSH 导入";

/// 欢迎页提示卡的状态。
#[derive(Default)]
pub struct MigrateHint {
    /// 启动后查过一次本机有没有 Z-SSH 数据。
    pub scanned: bool,
    pub found: bool,
    /// 点过「不再提示」或已经导入过（库里的标记；这里是它的缓存）。
    pub dismissed: bool,
}

impl MainWindow {
    /// 启动后查一次本机有没有 Z-SSH 的数据（后台线程）。
    pub(super) fn scan_zssh_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.migrate_hint.scanned {
            return;
        }
        self.migrate_hint.scanned = true;
        // 读不到标记（库没打开）时按"点过不再提示"处理：宁可不提示，也不在读不了数据的时候弹卡。
        self.migrate_hint.dismissed = self
            .store
            .db()
            .map(migrate_demo::is_dismissed)
            .unwrap_or(true);
        let Ok(rx) = on_blocking(|| migrate_demo::detect_zssh(None).is_some()) else {
            return;
        };
        self.await_result(rx, window, cx, |this, found, _window, cx| {
            this.migrate_hint.found = found;
            cx.notify();
        });
    }

    /// 欢迎页现在要不要画提示卡。
    pub(super) fn zssh_tip_visible(&self) -> bool {
        migrate_demo::tip_visible(
            self.migrate_hint.found,
            self.store.hosts().is_empty(),
            self.migrate_hint.dismissed,
        )
    }

    /// 设置页的迁移块：第一次要用时建（欢迎页的入口也会走这里）。
    pub(super) fn ensure_migrate(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<MigrateDemo> {
        if let Some(migrate) = &self.migrate_settings {
            return migrate.clone();
        }
        let weak = cx.entity().downgrade();
        // 延后到当前这次更新结束再动主窗口：驱动脚本是在主窗口的更新里调用迁移块的，直接 update 会重入。
        let notice: migrate_demo::NoticeFn = Rc::new(move |notice, window, cx| {
            let weak = weak.clone();
            window.defer(cx, move |window, cx| {
                let _ = weak.update(cx, |this, cx| match notice {
                    MigrateNotice::OpenHosts => this.show_view(MainView::Vault, window, cx),
                    MigrateNotice::Imported => {
                        this.migrate_hint.dismissed = true;
                        cx.notify();
                    }
                });
            });
        });
        let migrate = cx.new(|cx| MigrateDemo::new_embedded(window, cx, notice));
        self.migrate_settings = Some(migrate.clone());
        migrate
    }

    /// 去设置 → 同步最上面的「从 Z-SSH 迁移」。`start` 为真时顺手读一遍本机的 Z-SSH 数据并预览
    /// （提示卡的「导入」、找到数据时的欢迎页链接）。
    pub(crate) fn open_migrate(
        &mut self,
        start: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let migrate = self.ensure_migrate(window, cx);
        self.open_sync_settings(window, cx);
        self.settings_scroll
            .set_offset(::gpui::point(px(0.0), px(0.0)));
        if start {
            migrate.update(cx, |migrate, cx| migrate.request_direct(cx));
        }
        cx.notify();
    }

    /// 提示卡的「不再提示」。
    pub(super) fn dismiss_zssh_tip(&mut self, cx: &mut Context<Self>) {
        self.migrate_hint.dismissed = true;
        if let Ok(db) = self.store.db() {
            // 记不上就只在这次运行里不再出现。
            let _ = migrate_demo::set_dismissed(db);
        }
        cx.notify();
    }

    /// 欢迎页上方的提示卡（绝对定位在顶部居中，不影响下面居中的标题和按钮）。
    pub(super) fn render_zssh_tip(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id("zssh-migrate-tip")
            .absolute()
            .top(px(20.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                h_flex()
                    .max_w(px(640.0))
                    .gap_4()
                    .items_center()
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().typo(Role::ItemName, tokens).child(TIP_TITLE))
                            .child(div().typo(Role::Caption, tokens).child(TIP_TEXT)),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .child(text_button(
                                "zssh-tip-import",
                                "导入",
                                ButtonLook::Primary,
                                act(&entity, |this, window, cx| {
                                    this.open_migrate(true, window, cx)
                                }),
                            ))
                            .child(text_button(
                                "zssh-tip-dismiss",
                                "不再提示",
                                ButtonLook::Ghost,
                                act(&entity, |this, _window, cx| this.dismiss_zssh_tip(cx)),
                            )),
                    ),
            )
    }

    /// 欢迎页按钮行下面的文字链接。
    pub(super) fn render_zssh_link(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .id("welcome-from-zssh")
            .mt_1()
            .text_sm()
            .text_color(tokens.accent)
            .cursor_pointer()
            .hover(|style| style.opacity(0.75))
            .child(WELCOME_LINK)
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    // 本机找到了 Z-SSH 的数据就直接读出来预览；没找到只打开页面，那里能指定文件夹或选备份文件。
                    let start = this.migrate_hint.found;
                    this.open_migrate(start, window, cx);
                });
            })
    }
}
