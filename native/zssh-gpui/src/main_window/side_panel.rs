//! 主窗口右侧的面板容器：同一时间一个面板，左边缘可拖宽（双击恢复默认），记住宽度，
//! 右上角 ✕ 收起；焦点在面板里（或在主机页）时 Esc 也收起。
//!
//! 接口按通用写：面板只交出 [`PanelContent`]（标题、副标题、返回键、右上角按钮、正文、底部按钮），
//! 外壳（宽度、拖动把手、边框、标题栏、滚动）都在这里。V2-U3 起「管理 + 设置」页（`settings_page`）
//! 拿同一份 [`PanelContent`] 画在整页里（凭据、快捷命令、代理、端口转发），所以这些面板的列表 / 表单
//! 只有一份实现。
//!
//! 谁盖谁（同 0.2.14 的 `hostPanel` + `sidePanel` 两层）：主机详情单独一层；代理（从主机详情进来管理）/
//! 快捷命令（终端命令栏的「管理」「新建」「编辑」）这类侧栏另一层，开着侧栏时主机详情只是被藏到下面
//! （里面没保存的编辑还在），关掉侧栏回来；打开主机详情会先把侧栏收掉。

use ::gpui::{MouseButton, SharedString, px};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use super::panel_width::{self, DragStart, PanelKind};
use super::widgets::icon_button;
use crate::app_theme::Tokens;

/// 面板编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelId(pub &'static str);

pub const HOST_DETAILS: PanelId = PanelId("host-details");
pub const PROXIES: PanelId = PanelId("proxies");
pub const QUICK_COMMANDS: PanelId = PanelId("quick-commands");

/// 哪些面板开着。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelState {
    /// 主机详情（新建 / 编辑）开着。
    host: bool,
    /// 凭据 / 代理 / 转发这类侧栏。
    side: Option<PanelId>,
}

impl PanelState {
    /// 屏幕上现在显示哪个：侧栏盖在主机详情上面。
    pub fn visible(self) -> Option<PanelId> {
        self.side.or(self.host.then_some(HOST_DETAILS))
    }

    pub fn host_details_open(self) -> bool {
        self.host
    }

    /// 打开主机详情：先把侧栏收掉，再显示它。
    pub fn open_host_details(&mut self) {
        self.side = None;
        self.host = true;
    }

    /// 关掉主机详情（保存、删除、取消之后）。
    pub fn close_host_details(&mut self) {
        self.host = false;
    }

    /// 打开某个侧栏；已经开着别的就换成它。
    pub fn open_side(&mut self, id: PanelId) {
        self.side = Some(id);
    }

    pub fn close_side(&mut self) {
        self.side = None;
    }

    /// ✕ 和 Esc：关掉屏幕上显示的那个；返回关掉了谁。侧栏关掉后，下面的主机详情重新露出来。
    pub fn close_visible(&mut self) -> Option<PanelId> {
        let shown = self.visible()?;
        if self.side.is_some() {
            self.side = None;
        } else {
            self.host = false;
        }
        Some(shown)
    }
}

/// 一个面板交给外壳的内容。
pub struct PanelContent {
    pub title: SharedString,
    pub subtitle: Option<SharedString>,
    /// 标题前面的返回键（从主机详情进代理管理时回主机详情）。
    pub back: Option<AnyElement>,
    /// 标题栏右边、✕ 前面的按钮。
    pub actions: Vec<AnyElement>,
    pub body: AnyElement,
    /// 底部按钮区；没有就不画那一条。
    pub footer: Option<AnyElement>,
}

impl MainWindow {
    /// 显示面板时给终端 / 主机页留的宽度（面板的上限要给它们让位）。
    pub(super) fn panel_kind(&self) -> PanelKind {
        PanelKind::Aside
    }

    pub(super) fn panel_width_now(&self, window_width: f32) -> f32 {
        panel_width::clamp(self.panel_kind(), self.aside_width, window_width, 0.0)
    }

    /// 要不要在这一页显示右侧面板。设置与管理页占满整个区域：管理组的列表和表单直接画在页面里
    /// （`settings_page`），不再开侧面板。
    pub(super) fn panels_on_page(&self) -> bool {
        match self.nav.view {
            MainView::Welcome | MainView::Vault | MainView::Terminal => true,
            MainView::Settings => false,
        }
    }

    /// 关掉屏幕上的面板（✕、Esc）。
    pub(crate) fn close_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.proxy_ui.from_host_form && self.panels.visible() == Some(PROXIES) {
            // 从主机详情进来的代理管理：✕ 和返回键一样回到主机详情。
            self.back_to_host_details(cx);
            return;
        }
        if self.panels.close_visible().is_some() {
            if !self.panels.host_details_open() {
                self.host_form = None;
            }
            self.focus_content(window, cx);
            cx.notify();
        }
    }

    /// 在框架里画出当前显示的面板；没有面板返回 `None`。
    pub(super) fn render_side_panel(
        &mut self,
        tokens: &Tokens,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.panels_on_page() {
            return None;
        }
        let id = self.panels.visible()?;
        let content = match id {
            HOST_DETAILS => self.host_panel_content(tokens, window, cx),
            PROXIES => self.proxy_panel_content(tokens, window, cx),
            QUICK_COMMANDS => self.quick_panel_content(tokens, window, cx),
            _ => return None,
        };
        Some(self.panel_shell(content, tokens, window, cx))
    }

    fn panel_shell(
        &mut self,
        content: PanelContent,
        tokens: &Tokens,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let window_width = f32::from(window.viewport_size().width);
        let width = self.panel_width_now(window_width);
        let kind = self.panel_kind();
        let entity = cx.entity();
        let close = {
            let entity = entity.clone();
            icon_button(
                "panel-close",
                IconName::Close,
                "关闭",
                move |_, window, cx| {
                    entity.update(cx, |this, cx| this.close_panel(window, cx));
                },
            )
        };
        let handle = {
            let down = entity.clone();
            let reset = entity.clone();
            div()
                .id("panel-resize")
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-3.0))
                .w(px(6.0))
                .cursor_col_resize()
                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                    cx.stop_propagation();
                    if event.click_count >= 2 {
                        reset.update(cx, |this, cx| this.reset_panel_width(cx));
                    } else {
                        let start = DragStart {
                            pointer_x: f32::from(event.position.x),
                            width,
                        };
                        down.update(cx, |this, cx| {
                            this.panel_drag = Some(start);
                            cx.notify();
                        });
                    }
                })
        };
        let _ = kind;
        v_flex()
            .id("side-panel")
            .relative()
            .flex_none()
            .h_full()
            .w(px(width))
            .track_focus(&self.panel_focus)
            .bg(tokens.surface)
            .border_l_1()
            .border_color(tokens.border)
            .child(handle)
            .child(
                h_flex()
                    .flex_none()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(tokens.border)
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .children(content.back)
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(content.title),
                                    )
                                    .children(content.subtitle.map(|text| {
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .text_color(tokens.text_muted)
                                            .child(text)
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .children(content.actions)
                            .child(close),
                    ),
            )
            .child(
                div()
                    .id("panel-scroll")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.panel_scroll)
                    .child(
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .p_4()
                            .gap_4()
                            .child(content.body),
                    ),
            )
            .children(content.footer.map(|footer| {
                div()
                    .flex_none()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(tokens.border)
                    .child(footer)
            }))
            .into_any_element()
    }

    pub(super) fn reset_panel_width(&mut self, cx: &mut Context<Self>) {
        self.aside_width = self.panel_kind().default_width();
        self.save_panel_width();
        cx.notify();
    }

    pub(super) fn save_panel_width(&mut self) {
        let key = self.panel_kind().setting_key();
        let text = format!("{}", self.aside_width.round() as i64);
        if let Ok(db) = self.store.db()
            && let Err(err) = db.setting_set(key, &text)
        {
            tracing::warn!(%err, "保存面板宽度失败");
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{HOST_DETAILS, PROXIES, PanelState, QUICK_COMMANDS};

    #[test]
    fn nothing_is_open_at_first_and_opening_host_details_shows_it() {
        let mut state = PanelState::default();
        assert_eq!(state.visible(), None);
        state.open_host_details();
        assert_eq!(state.visible(), Some(HOST_DETAILS));
        assert!(state.host_details_open());
    }

    #[test]
    fn a_side_panel_covers_the_host_details_without_closing_them() {
        let mut state = PanelState::default();
        state.open_host_details();
        state.open_side(QUICK_COMMANDS);
        assert_eq!(state.visible(), Some(QUICK_COMMANDS), "侧栏盖在上面");
        assert!(state.host_details_open(), "主机详情只是藏在下面，没丢");
        assert_eq!(state.close_visible(), Some(QUICK_COMMANDS));
        assert_eq!(
            state.visible(),
            Some(HOST_DETAILS),
            "关掉侧栏，主机详情重新露出来"
        );
        assert_eq!(state.close_visible(), Some(HOST_DETAILS));
        assert_eq!(state.visible(), None);
        assert_eq!(state.close_visible(), None, "什么都没开时关闭不做事");
    }

    #[test]
    fn opening_host_details_closes_any_side_panel_first() {
        let mut state = PanelState::default();
        state.open_side(QUICK_COMMANDS);
        state.open_host_details();
        assert_eq!(state.visible(), Some(HOST_DETAILS));
        state.open_side(PROXIES);
        state.open_side(QUICK_COMMANDS);
        assert_eq!(
            state.visible(),
            Some(QUICK_COMMANDS),
            "同一时间只有一个侧栏，后开的换掉先开的"
        );
    }
}
