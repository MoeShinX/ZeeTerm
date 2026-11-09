//! 标题栏标签条放不下时的处理（V2-U1 第 4 条）：
//!
//! 1. 标签先等比缩窄，缩到最小宽度（标题省略号，悬停显示全名）；
//! 2. 还放不下，标签条改成可以横向滚动，右边出现「溢出按钮」（显示有几个标签露不全），点开列出
//!    所有标签（终端 / 文件 / 编辑器），可以切换也可以关闭；
//! 3. 当前标签始终在可见范围里（切换、新开时滚到可见）；
//! 4. 鼠标滚轮在标签条上横向滚动（竖滚轮也转成横向）。
//!
//! 这个文件有两部分：纯函数（可见性、滚轮换算，不依赖 GPUI，单测钉住规则）和主窗口上的界面。

use std::cell::Cell;
use std::rc::Rc;

use ::gpui::{
    Bounds, MouseButton, Pixels, ScrollEvent, ScrollHandle, SharedString, canvas, point, px,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use super::titlebar::BAR_HEIGHT;
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::terminal_workspace::{TabInfo, TabStatus};

/// 本文件（标签条溢出按钮、标签列表）的界面文案。
#[derive(Clone, Copy, Debug)]
enum TabStripMsg {
    OverflowPrefix,
    OverflowSuffix,
    CloseTab,
}

impl Msg for TabStripMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::OverflowPrefix => "还有 ",
            Self::OverflowSuffix => " 个标签没显示全，点开看全部",
            Self::CloseTab => "关闭标签",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::OverflowPrefix => "",
            Self::OverflowSuffix => " more tabs are not shown in full. Open the list to see all.",
            Self::CloseTab => "Close Tab",
        }
    }
}

/// 标签最小宽度：缩到这里就不再缩，改成溢出。
pub const TAB_MIN_WIDTH: f32 = 112.0;
/// 溢出菜单的宽度。
const MENU_WIDTH: f32 = 300.0;
/// 判断「看得见」时容忍的像素误差。
const SLACK: f32 = 1.0;
/// 切换 / 新开标签后，滚到可见要重复请求多少帧（等布局稳定）。
const SETTLE_FRAMES: u8 = 10;

/// 标签条里的一项：终端标签 / 文件（SFTP）标签 / 编辑器标签，各用自己的编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKey {
    Term(u64),
    Sftp(u64),
    Editor(u64),
}

/// 一个标签在条里的左右边（未滚动时的布局位置）。
pub type Span = (f32, f32);

/// 条的可见区域是 `[left, right]`，内容向左滚了 `offset`（≤ 0）：有几个标签没有完整露出来。
pub fn hidden_count(left: f32, right: f32, offset: f32, items: &[Span]) -> usize {
    items
        .iter()
        .filter(|(item_left, item_right)| {
            item_left + offset < left - SLACK || item_right + offset > right + SLACK
        })
        .count()
}

/// 滚轮换算：竖滚轮和横滚轮都转成横向滚动。`delta` 是 GPUI 的滚动量（向下 / 向右滚内容为负）；
/// 竖向和横向同时有时取绝对值大的那个。结果夹在 `[-max_scroll, 0]`。
pub fn wheel_scroll(offset: f32, delta_x: f32, delta_y: f32, max_scroll: f32) -> f32 {
    let delta = if delta_x.abs() > delta_y.abs() {
        delta_x
    } else {
        delta_y
    };
    (offset + delta).clamp(-max_scroll.max(0.0), 0.0)
}

/// 标签条里的一项要画的东西。
#[derive(Clone, Debug)]
pub struct Entry {
    pub key: EntryKey,
    pub title: String,
    /// 终端标签的连接状态（画状态点）；文件 / 编辑器标签没有。
    pub status: Option<TabStatus>,
}

/// 标签条的界面状态。
pub struct TabStrip {
    pub scroll: ScrollHandle,
    /// 溢出菜单开着没有。
    pub menu_open: bool,
    /// 溢出按钮上一帧的位置（菜单靠它对齐）。
    pub button: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 上一次画的时候当前标签是哪个：变了就滚到可见。
    last_active: Option<EntryKey>,
    /// 上一帧算出来有几个标签没完整露出来。
    pub hidden: usize,
    /// 还要再画几帧：滚到可见的请求要等布局量出新标签的位置才生效，所以请求之后多画几帧。
    pub settle_frames: u8,
}

impl Default for TabStrip {
    fn default() -> Self {
        Self {
            scroll: ScrollHandle::new(),
            menu_open: false,
            button: Rc::new(Cell::new(None)),
            last_active: None,
            hidden: 0,
            settle_frames: 0,
        }
    }
}

impl TabStrip {
    /// 每一帧：当前标签变了就请求滚到可见，并按上一帧量到的位置数一下有几个没露全。
    /// `active` 是当前标签和它在条里的下标。返回有几个没露全（变了要重画，调用方比较前后值）。
    pub fn sync(&mut self, active: Option<(EntryKey, usize)>, count: usize) -> usize {
        let key = active.map(|(key, _)| key);
        if key != self.last_active {
            self.last_active = key;
            if let Some((_, index)) = active {
                self.scroll.scroll_to_item(index);
                self.settle_frames = SETTLE_FRAMES;
            }
        } else if self.settle_frames > 0
            && let Some((_, index)) = active
        {
            // 新标签出现、溢出按钮出现都会改条的宽度：位置没稳定之前每帧重新请求一次。
            self.scroll.scroll_to_item(index);
        }
        let viewport = self.scroll.bounds();
        let offset = f32::from(self.scroll.offset().x);
        let spans: Vec<Span> = (0..count)
            .filter_map(|index| self.scroll.bounds_for_item(index))
            .map(|bounds| (f32::from(bounds.left()), f32::from(bounds.right())))
            .collect();
        self.hidden = hidden_count(
            f32::from(viewport.left()),
            f32::from(viewport.right()),
            offset,
            &spans,
        );
        self.hidden
    }

    /// 滚轮：把竖滚轮转成横向滚动。返回是否真的动了。
    pub fn wheel(&self, delta_x: f32, delta_y: f32) -> bool {
        let offset = self.scroll.offset();
        let max = f32::from(self.scroll.max_offset().x).abs();
        let next = wheel_scroll(f32::from(offset.x), delta_x, delta_y, max);
        if (next - f32::from(offset.x)).abs() < 0.01 {
            return false;
        }
        self.scroll.set_offset(point(px(next), offset.y));
        true
    }
}

impl MainWindow {
    /// 标签条里所有标签，顺序就是画的顺序：终端、文件、编辑器。
    pub(super) fn tab_entries(&self, infos: &[TabInfo], cx: &App) -> Vec<Entry> {
        let mut entries: Vec<Entry> = infos
            .iter()
            .map(|info| Entry {
                key: EntryKey::Term(info.id),
                title: info.title.clone(),
                status: Some(info.status),
            })
            .collect();
        entries.extend(self.docks.sftp_tabs.iter().map(|tab| Entry {
            key: EntryKey::Sftp(tab.id),
            title: tab.title.clone(),
            status: None,
        }));
        entries.extend(self.docks.editor_tabs.iter().map(|tab| Entry {
            key: EntryKey::Editor(tab.id),
            title: tab.view.read(cx).file_name(),
            status: None,
        }));
        entries
    }

    /// 当前高亮的那个标签（主机页 / 设置页没有）。
    pub(super) fn active_entry(
        &self,
        entries: &[Entry],
        infos: &[TabInfo],
        active_term: Option<usize>,
    ) -> Option<(EntryKey, usize)> {
        if self.nav.view != MainView::Terminal {
            return None;
        }
        let key = if let Some(id) = self.docks.active_sftp {
            EntryKey::Sftp(id)
        } else if let Some(id) = self.docks.active_editor {
            EntryKey::Editor(id)
        } else {
            EntryKey::Term(infos.get(active_term?)?.id)
        };
        entries
            .iter()
            .position(|entry| entry.key == key)
            .map(|index| (key, index))
    }

    /// 溢出按钮：有标签露不全时出现，写着露不全的个数；点开列出全部标签。
    pub(super) fn render_tab_overflow_button(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let hidden = self.tab_strip.hidden;
        if hidden == 0 && !self.tab_strip.menu_open {
            return None;
        }
        let entity = cx.entity();
        let hover = tokens.hover;
        let text = tokens.text;
        let cell = Rc::clone(&self.tab_strip.button);
        let open = self.tab_strip.menu_open;
        Some(
            div()
                .id("tab-overflow")
                .occlude()
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .h(px(26.0))
                .px_1p5()
                .mb_1()
                .ml_0p5()
                .flex_none()
                .rounded_full()
                .text_xs()
                .text_color(if open { text } else { tokens.text_muted })
                .when(open, |d| d.bg(hover))
                .cursor_pointer()
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(move |window, cx| {
                    Tooltip::new(format!(
                        "{}{hidden}{}",
                        t(TabStripMsg::OverflowPrefix),
                        t(TabStripMsg::OverflowSuffix)
                    ))
                    .build(window, cx)
                })
                .child(
                    canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .child(Icon::new(IconName::ChevronDown).size_3p5())
                .child(SharedString::from(hidden.to_string()))
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| this.toggle_tab_menu(cx));
                }),
        )
    }

    pub(super) fn toggle_tab_menu(&mut self, cx: &mut Context<Self>) {
        self.tab_strip.menu_open = !self.tab_strip.menu_open;
        cx.notify();
    }

    pub(super) fn close_tab_menu(&mut self, cx: &mut Context<Self>) {
        if self.tab_strip.menu_open {
            self.tab_strip.menu_open = false;
            cx.notify();
        }
    }

    /// 溢出菜单：所有标签一行一个，点行切换，点 ✕ 关闭；点菜单外面收起。
    pub(super) fn render_tab_menu(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<::gpui::AnyElement> {
        if !self.tab_strip.menu_open {
            return None;
        }
        let entity = cx.entity();
        let infos = self.workspace.read(cx).tab_infos();
        let entries = self.tab_entries(&infos, cx);
        let active_term = self.workspace.read(cx).active_index();
        let active = self
            .active_entry(&entries, &infos, active_term)
            .map(|(key, _)| key);
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let left = self
            .tab_strip
            .button
            .get()
            .map_or(self.window_width - MENU_WIDTH - 8.0, |bounds| {
                f32::from(bounds.right()) - MENU_WIDTH
            })
            .clamp(8.0, (self.window_width - MENU_WIDTH - 8.0).max(8.0));
        let rows = entries.into_iter().enumerate().map(|(row, entry)| {
            let key = entry.key;
            let is_active = active == Some(key);
            let select = entity.clone();
            let close = entity.clone();
            let title: SharedString = entry.title.clone().into();
            let dot = entry.status.map(|status| match status {
                TabStatus::Connecting => tokens.dot_connecting,
                TabStatus::Connected => tokens.dot_connected,
                TabStatus::Disconnected => tokens.dot_disconnected,
                TabStatus::Empty => tokens.dot_idle,
            });
            h_flex()
                .id(("tab-menu-row", row))
                .items_center()
                .gap_2()
                .h(px(30.0))
                .px_2()
                .rounded_full()
                .text_sm()
                .cursor_pointer()
                .text_color(if is_active { text } else { muted })
                .when(is_active, |d| d.bg(tokens.accent_soft))
                .hover(move |style| style.bg(hover).text_color(text))
                .child(match dot {
                    Some(color) => div()
                        .size(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(color)
                        .into_any_element(),
                    None => Icon::new(crate::icons::Ico::FolderTree)
                        .size_3p5()
                        .text_color(tokens.accent)
                        .into_any_element(),
                })
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(
                    div()
                        .id(("tab-menu-close", row))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(22.0))
                        .flex_none()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover).text_color(text))
                        .tooltip(|window, cx| {
                            Tooltip::new(t(TabStripMsg::CloseTab)).build(window, cx)
                        })
                        .child(Icon::new(IconName::Close).size_3())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            close.update(cx, |this, cx| this.close_tab_entry(key, window, cx));
                        }),
                )
                .on_click(move |_, window, cx| {
                    select.update(cx, |this, cx| this.select_tab_entry(key, window, cx));
                })
        });
        let backdrop = {
            let entity = entity.clone();
            div()
                .id("tab-menu-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    entity.update(cx, |this, cx| this.close_tab_menu(cx));
                })
        };
        Some(
            div()
                .id("tab-menu-layer")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(backdrop)
                .child(
                    v_flex()
                        .id("tab-menu")
                        .absolute()
                        .top(px(BAR_HEIGHT))
                        .left(px(left))
                        .w(px(MENU_WIDTH))
                        .max_h(px((self.window_height - BAR_HEIGHT - 16.0).max(120.0)))
                        .overflow_y_scroll()
                        .gap_0p5()
                        .p_1()
                        .rounded(::gpui::px(crate::app_theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(tokens.border)
                        .bg(tokens.surface)
                        .shadow_md()
                        .occlude()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// 在菜单里点了一个标签：切过去（终端 / 文件 / 编辑器各走自己的切换），菜单收起。
    pub(crate) fn select_tab_entry(
        &mut self,
        key: EntryKey,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_strip.menu_open = false;
        match key {
            EntryKey::Term(id) => {
                let index = self
                    .workspace
                    .read(cx)
                    .tab_infos()
                    .iter()
                    .position(|info| info.id == id);
                if let Some(index) = index {
                    self.docks.active_sftp = None;
                    self.docks.active_editor = None;
                    self.select_tab(index, window, cx);
                }
            }
            EntryKey::Sftp(id) => self.select_sftp_tab(id, window, cx),
            EntryKey::Editor(id) => self.select_editor_tab(id, window, cx),
        }
        cx.notify();
    }

    /// 在菜单里点了 ✕：关这个标签（有未完成的传输 / 未保存的修改照旧先问）。菜单留着，
    /// 剩下的标签还在里面；没有标签露不全了菜单自己收起。
    pub(crate) fn close_tab_entry(
        &mut self,
        key: EntryKey,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            EntryKey::Term(id) => {
                let index = self
                    .workspace
                    .read(cx)
                    .tab_infos()
                    .iter()
                    .position(|info| info.id == id);
                if let Some(index) = index {
                    self.close_tab(index, window, cx);
                }
            }
            EntryKey::Sftp(id) => self.close_sftp_tab(id, window, cx),
            EntryKey::Editor(id) => self.close_editor_tab(id, window, cx),
        }
        cx.notify();
    }
}

/// 滚轮事件换算成像素：行滚动按行高算。
pub fn wheel_deltas(event: &ScrollEvent, line_height: Pixels) -> (f32, f32) {
    let delta = event.delta.pixel_delta(line_height);
    (f32::from(delta.x), f32::from(delta.y))
}

#[cfg(test)]
mod tests {
    use super::{hidden_count, wheel_scroll};

    fn strip(count: usize, width: f32, gap: f32) -> Vec<(f32, f32)> {
        (0..count)
            .map(|i| {
                let left = i as f32 * (width + gap);
                (left, left + width)
            })
            .collect()
    }

    #[test]
    fn nothing_is_hidden_when_everything_fits() {
        let items = strip(4, 112.0, 2.0);
        assert_eq!(hidden_count(0.0, 800.0, 0.0, &items), 0);
    }

    #[test]
    fn tabs_cut_off_at_the_right_edge_are_counted() {
        // 10 个最小宽度的标签，条只有 600 宽：第 6 个（下标 5）起放不下（5*114=570..682）。
        let items = strip(10, 112.0, 2.0);
        assert_eq!(hidden_count(0.0, 600.0, 0.0, &items), 5);
        // 只有一部分露出来也算没露全。
        assert_eq!(hidden_count(0.0, 650.0, 0.0, &items), 5);
        assert_eq!(hidden_count(0.0, 682.0, 0.0, &items), 4);
    }

    #[test]
    fn scrolling_moves_the_hidden_ones_to_the_left_side() {
        let items = strip(10, 112.0, 2.0);
        // 往右滚 300：左边有露不全的，右边也有。
        let hidden = hidden_count(0.0, 600.0, -300.0, &items);
        // 可见范围 300..900（布局坐标）：下标 2（228..340）、下标 7（798..910）都只露一半。
        assert_eq!(hidden, 3 + 3, "左边 0、1、2 号，右边 7、8、9 号");
    }

    #[test]
    fn the_last_scroll_position_hides_only_the_left_side() {
        let items = strip(10, 112.0, 2.0);
        let total = 10.0 * 112.0 + 9.0 * 2.0; // 1138
        let max = total - 600.0;
        let hidden = hidden_count(0.0, 600.0, -max, &items);
        assert!(hidden >= 4, "{hidden}");
        // 最右边一个一定完整露出来。
        let last = items[9];
        assert!(last.1 - max <= 600.0 + 1.0);
    }

    #[test]
    fn the_wheel_scrolls_sideways_and_stays_inside_the_range() {
        // 竖滚轮向下（delta.y < 0）= 内容向左移 = 偏移更负。
        assert_eq!(wheel_scroll(0.0, 0.0, -40.0, 500.0), -40.0);
        // 向上滚回来。
        assert_eq!(wheel_scroll(-40.0, 0.0, 40.0, 500.0), 0.0);
        // 到头了不再动。
        assert_eq!(wheel_scroll(0.0, 0.0, 40.0, 500.0), 0.0);
        assert_eq!(wheel_scroll(-490.0, 0.0, -40.0, 500.0), -500.0);
        // 横向和竖向同时来取大的那个。
        assert_eq!(wheel_scroll(0.0, -50.0, -10.0, 500.0), -50.0);
        assert_eq!(wheel_scroll(0.0, -10.0, -50.0, 500.0), -50.0);
        // 不能滚的时候（内容没超出）不动。
        assert_eq!(wheel_scroll(0.0, 0.0, -40.0, 0.0), 0.0);
    }
}
