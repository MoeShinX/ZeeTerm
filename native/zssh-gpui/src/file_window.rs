//! 独立文件窗口（0.2.14 `components/files/FileWindowApp.tsx` + `src-tauri/src/file_window.rs`）：像 Xftp 那样
//! 的单独窗口，整个程序最多一个，里面用标签放多个 SFTP 视图。
//!
//! 标签从主窗口交过来（标签右键「移到文件窗口」，或者文件窗口开着时新开的 SFTP / 面板里的「在新标签中
//! 打开」）。**交过来的是同一个视图实体**（`Entity<SftpDemo>`）：已认证的连接、传输引擎和队列、两栏的
//! 目录都原样带过去，不重连、传输不中断；换窗口只重建和旧窗口绑着的东西（输入框、输入法订阅、轮询
//! 任务，见 `SftpDemo::rebind_window`）。标签随时只属于一个地方（主窗口的标签、停靠面板，或者这里），
//! 所以每个传输任务都有明确的归属：
//!
//! - 标签「移回主窗口」（右键）：视图交回主窗口的标签条，任务不中断；
//! - 关标签：断开这个标签的 SFTP（取消没传完的任务，连接回到池里给终端用），有传输先问；
//! - 关窗口：设置「文件窗口关闭时」选了合并就把标签全交回主窗口，否则有传输先问、再断开全部。
//!
//! 内置编辑器照旧是各自的编辑窗口，所以这里不用转交。

use ::gpui::{
    AnyWindowHandle, App, FocusHandle, Global, SharedString, TitlebarOptions, WeakEntity, Window,
    WindowBounds, WindowControlArea, WindowHandle, WindowOptions, px,
};
use gpui_kit::component::TitleBar;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::main_window::MainWindow;
use crate::main_window::titlebar::window_controls;
use crate::settings::data_dir;
use crate::sftp_demo::SftpDemo;
use crate::typography::UiFontStyled as _;
use zssh_core::store::Database;

/// 窗口标题：产品名 + 「文件」。0.2.14 的窗口叫 `Z-SFTP`（品牌缩写 + SFTP，写死在 `FileWindowApp.tsx`），
/// 原生版换成 ZeeTerm 的品牌，界面里这个功能叫「文件」（标题栏的文件按钮、`Ctrl+Shift+F`），所以是
/// 「ZeeTerm 文件」；窗口里标签仍是 0.2.14 的 `<主机名>`（`fileTabLabel` 去掉 `(SFTP)` 后缀）。
/// 文件窗口标题栏的高度（拖标签时判断指针是不是落在它的标签栏上）。
pub const STRIP_HEIGHT: f32 = 40.0;

pub fn window_title() -> String {
    format!("{} 文件", crate::product::NAME)
}

/// 设置 `file_window_close` 的取值：关文件窗口时把标签合并回主窗口。
const CLOSE_MERGE: &str = "merge";

/// 登记在应用全局里的东西：主窗口（标签要交回去）和现在开着的文件窗口。
#[derive(Default)]
struct Registry {
    main: Option<MainRef>,
    window: Option<WindowHandle<FileWindow>>,
}

#[derive(Clone)]
struct MainRef {
    handle: AnyWindowHandle,
    view: WeakEntity<MainWindow>,
}

impl Global for Registry {}

fn registry(cx: &mut App) -> &mut Registry {
    if !cx.has_global::<Registry>() {
        cx.set_global(Registry::default());
    }
    cx.global_mut::<Registry>()
}

/// 启动时登记主窗口（标签「移回主窗口」要找到它）。
pub fn register_main(cx: &mut App, handle: AnyWindowHandle, view: WeakEntity<MainWindow>) {
    registry(cx).main = Some(MainRef { handle, view });
}

/// 主窗口（编辑器标签、文件视图要交回去时找它）：窗口句柄和主窗口实体。
pub fn main_window(cx: &App) -> Option<(AnyWindowHandle, WeakEntity<MainWindow>)> {
    let main = cx.try_global::<Registry>()?.main.clone()?;
    Some((main.handle, main.view))
}

/// 文件窗口开着吗。
pub fn is_open(cx: &App) -> bool {
    window_handle(cx).is_some()
}

/// 把一个文件视图交给文件窗口：窗口开着就放进去当前标签，没有就新开一个。
/// 视图原来所在的地方（主窗口的标签 / 停靠面板）要先摘干净，视图不能同时在两个地方。
pub fn adopt(cx: &mut App, host_id: i64, title: String, view: Entity<SftpDemo>) {
    let existing = window_handle(cx);
    if let Some(handle) = existing {
        let (title_in, view_in) = (title.clone(), view.clone());
        let added = handle.update(cx, move |window_view, window, cx| {
            window_view.add_tab(host_id, title_in, view_in, window, cx);
            window.activate_window();
        });
        if added.is_ok() {
            return;
        }
        // 窗口刚好关了：登记作废，往下新开一个。
        registry(cx).window = None;
    }
    open_window(cx, host_id, title, view);
}

fn open_window(cx: &mut App, host_id: i64, title: String, view: Entity<SftpDemo>) {
    // 标题栏自己画（和主窗口一样跟着深 / 浅主题）：系统不画标题栏，系统标题只留给任务栏和 Alt+Tab。
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(window_title().into()),
            ..TitleBar::title_bar_options()
        }),
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(1100.), ::gpui::px(760.)),
            cx,
        )),
        window_min_size: Some(size(::gpui::px(640.), ::gpui::px(420.))),
        ..Default::default()
    };
    let opened = cx.open_window(options, move |window, cx| {
        window.set_window_title(&window_title());
        let file_window = cx.new(FileWindow::new);
        file_window.update(cx, |this, cx| {
            this.add_tab(host_id, title, view, window, cx);
        });
        let close_view = file_window.clone();
        // 有传输时这里返回 false，窗口里出现确认条；确认之后由按钮自己关窗口。
        window.on_window_should_close(cx, move |window, cx| {
            close_view.update(cx, |this, cx| this.should_close(window, cx))
        });
        file_window
    });
    if let Ok(handle) = opened {
        registry(cx).window = Some(handle);
    }
}

struct FileTab {
    id: u64,
    host_id: i64,
    title: String,
    view: Entity<SftpDemo>,
}

/// 正在问的确认。
enum Confirm {
    CloseTab(u64),
    CloseWindow,
}

pub struct FileWindow {
    tabs: Vec<FileTab>,
    active: Option<u64>,
    next_id: u64,
    confirm: Option<Confirm>,
    focus: FocusHandle,
    /// 标签拖到主窗口的标签栏上 = 合并回主窗口（V2-U1 第 7 条，见 `tab_drag`）。
    dragger: crate::tab_drag::Dragger,
}

/// 标签上只显示主机名：文件窗口里全是 SFTP，"(SFTP)" 后缀是多余的（同 0.2.14 `fileTabLabel`）。
pub(crate) fn tab_label(title: &str) -> String {
    title
        .trim_end()
        .strip_suffix("(SFTP)")
        .map_or_else(|| title.to_owned(), |rest| rest.trim_end().to_owned())
}

/// 关掉一个标签后选中哪个：关的不是当前的就不变；是当前的就选左边那个，没有就右边那个。
pub(crate) fn active_after_close(ids: &[u64], closing: u64, active: Option<u64>) -> Option<u64> {
    if active != Some(closing) {
        return active;
    }
    let index = ids.iter().position(|id| *id == closing)?;
    let rest: Vec<u64> = ids.iter().copied().filter(|id| *id != closing).collect();
    index
        .checked_sub(1)
        .and_then(|left| rest.get(left))
        .or_else(|| rest.get(index))
        .or_else(|| rest.first())
        .copied()
}

/// 关窗口时要不要先把标签合并回主窗口：设置选了合并、窗口里确实有标签、并且不是"合并已经做完、
/// 窗口正在真的关"那一次（同 0.2.14 `shouldMergeOnClose`）。
pub(crate) fn should_merge_on_close(setting: Option<&str>, tab_count: usize) -> bool {
    setting == Some(CLOSE_MERGE) && tab_count > 0
}

/// 设置里「关闭文件窗口时」的值。表里存的是 JSON 文本（`"merge"`，设置页和 0.2.14 都这样写），
/// 没带引号的老值也认。
fn close_setting() -> Option<String> {
    Database::open(&data_dir().join("zssh.db"))
        .ok()
        .and_then(|db| {
            db.setting_get(crate::app_prefs::FILE_WINDOW_CLOSE_KEY)
                .ok()
                .flatten()
        })
        .and_then(|raw| crate::app_prefs::text_of(Some(&raw)))
}

impl FileWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        // 窗口没了（不管怎么关的）：登记作废，下一次交标签时重新开。
        cx.on_release(|_, cx| {
            registry(cx).window = None;
        })
        .detach();
        Self {
            tabs: Vec::new(),
            active: None,
            next_id: 1,
            confirm: None,
            focus: cx.focus_handle(),
            dragger: crate::tab_drag::Dragger::default(),
        }
    }

    /// 放进来一个标签并选中它。视图在这个窗口里重建和窗口绑着的东西。
    fn add_tab(
        &mut self,
        host_id: i64,
        title: String,
        view: Entity<SftpDemo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        view.update(cx, |view, cx| {
            view.set_dock_layout(false, cx);
            view.rebind_window(window, cx);
        });
        self.tabs.push(FileTab {
            id,
            host_id,
            title,
            view,
        });
        self.active = Some(id);
        self.confirm = None;
        cx.notify();
    }

    fn unfinished_of(&self, id: u64, cx: &App) -> usize {
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map_or(0, |tab| tab.view.read(cx).unfinished())
    }

    fn unfinished_all(&self, cx: &App) -> usize {
        self.tabs
            .iter()
            .map(|tab| tab.view.read(cx).unfinished())
            .sum()
    }

    /// 这个窗口里每个标签的（主机 id，标题，还没结束的任务数）：驱动脚本和测试用。
    pub fn tab_summary(&self, cx: &App) -> Vec<(i64, String, usize)> {
        self.tabs
            .iter()
            .map(|tab| {
                (
                    tab.host_id,
                    tab.title.clone(),
                    tab.view.read(cx).unfinished(),
                )
            })
            .collect()
    }

    /// 现在显示的标签里的视图。
    pub fn active_view(&self) -> Option<Entity<SftpDemo>> {
        let id = self.active?;
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.view.clone())
    }

    /// 现在显示的标签的编号（驱动脚本用）。
    pub fn active_id(&self) -> Option<u64> {
        self.active
    }

    pub fn select(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active = Some(id);
            cx.notify();
        }
    }

    /// 摘掉一个标签（不处理它的视图），返回它。
    fn take_tab(&mut self, id: u64) -> Option<FileTab> {
        let index = self.tabs.iter().position(|tab| tab.id == id)?;
        let ids: Vec<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        self.active = active_after_close(&ids, id, self.active);
        Some(self.tabs.remove(index))
    }

    /// 关标签：有传输先问；断开这个标签的 SFTP；最后一个标签走了窗口也关掉。
    pub fn close_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.unfinished_of(id, cx) > 0 {
            self.confirm = Some(Confirm::CloseTab(id));
            cx.notify();
            return;
        }
        self.do_close_tab(id, window, cx);
    }

    fn do_close_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        if let Some(tab) = self.take_tab(id) {
            tab.view.update(cx, |view, _| view.shutdown_embedded());
        }
        if self.tabs.is_empty() {
            window.remove_window();
        }
        cx.notify();
    }

    /// 标签右键「移回主窗口」：视图交回主窗口的标签条（连接、传输都不断）。
    pub fn move_to_main(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.take_tab(id) else {
            return;
        };
        let Some(main) = cx
            .try_global::<Registry>()
            .and_then(|registry| registry.main.clone())
        else {
            self.restore_tab(tab, cx);
            return;
        };
        let (host_id, title, view) = (tab.host_id, tab.title.clone(), tab.view.clone());
        let handed = main.handle.update(cx, move |_, main_window, cx| {
            let adopted = main
                .view
                .update(cx, |main_view, cx| {
                    main_view.adopt_sftp_view(host_id, title, view, main_window, cx);
                })
                .is_ok();
            if adopted {
                main_window.activate_window();
            }
            adopted
        });
        if !matches!(handed, Ok(true)) {
            // 主窗口不在了：标签留在这里，不丢。
            self.restore_tab(tab, cx);
            return;
        }
        if self.tabs.is_empty() {
            window.remove_window();
        }
        cx.notify();
    }

    /// 交不出去的标签放回原处。
    fn restore_tab(&mut self, tab: FileTab, cx: &mut Context<Self>) {
        self.active = Some(tab.id);
        self.tabs.push(tab);
        cx.notify();
    }

    /// 窗口关闭按钮。设置选了合并：标签全交回主窗口；否则有传输先问，确认后断开这个窗口里的全部 SFTP。
    fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if should_merge_on_close(close_setting().as_deref(), self.tabs.len()) {
            let ids: Vec<u64> = self.tabs.iter().map(|tab| tab.id).collect();
            for id in ids {
                self.move_to_main(id, window, cx);
            }
            // 全部交过去了窗口已经被关掉（最后一个标签走时）；有交不过去的就留着。
            return self.tabs.is_empty();
        }
        if self.unfinished_all(cx) > 0 {
            self.confirm = Some(Confirm::CloseWindow);
            cx.notify();
            return false;
        }
        self.shutdown_all(cx);
        true
    }

    fn shutdown_all(&mut self, cx: &mut Context<Self>) {
        for tab in self.tabs.drain(..) {
            tab.view.update(cx, |view, _| view.shutdown_embedded());
        }
        self.active = None;
    }

    fn confirmed_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.shutdown_all(cx);
        window.remove_window();
    }

    fn render_tab(&self, tab: &FileTab, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let id = tab.id;
        let active = self.active == Some(id);
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let select = entity.clone();
        let close = entity.clone();
        let menu_back = entity.clone();
        let menu_close = entity.clone();
        let label: SharedString = tab_label(&tab.title).into();
        let press = entity.clone();
        let press_title = tab_label(&tab.title);
        let dragging = self.dragger.key() == Some(crate::tab_drag::DragKey::Sftp(id));
        div()
            .id(("file-tab", id))
            .occlude()
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.0))
            .min_w(px(112.0))
            .max_w(px(220.0))
            .pl_3()
            .pr_1p5()
            .flex_none()
            .rounded_t_md()
            .border_x_1()
            .border_t_1()
            .text_sm()
            .cursor_pointer()
            .when(dragging, |d| d.opacity(0.4))
            .when(active, |d| {
                d.border_color(tokens.border)
                    .bg(tokens.background)
                    .text_color(text)
            })
            .when(!active, |d| {
                d.border_color(::gpui::Hsla::transparent_black())
                    .text_color(muted)
                    .hover(move |style| style.bg(hover))
            })
            .child(
                Icon::new(Ico::FolderTree)
                    .size_3p5()
                    .text_color(tokens.accent),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                div()
                    .id(("file-tab-close", id))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .flex_none()
                    .rounded_sm()
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(text))
                    .child(Icon::new(IconName::Close).size_3())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close.update(cx, |this, cx| this.close_tab(id, window, cx));
                    }),
            )
            .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                let pos = (f32::from(event.position.x), f32::from(event.position.y));
                let title = press_title.clone();
                press.update(cx, |this, _cx| {
                    this.dragger
                        .press(crate::tab_drag::DragKey::Sftp(id), title, pos);
                });
            })
            .on_click(move |_, _, cx| {
                select.update(cx, |this, cx| this.select(id, cx));
            })
            .context_menu(move |menu, _window, cx| {
                let tokens = Tokens::current(cx);
                let back = menu_back.clone();
                let close = menu_close.clone();
                menu.item(
                    crate::menu_kit::row(&tokens, "移回主窗口", Ico::SquareArrowDownLeft).on_click(
                        move |_, window, cx| {
                            back.update(cx, |this, cx| this.move_to_main(id, window, cx));
                        },
                    ),
                )
                .item(crate::menu_kit::row(&tokens, "关闭", Ico::X).on_click(
                    move |_, window, cx| {
                        close.update(cx, |this, cx| this.close_tab(id, window, cx));
                    },
                ))
            })
            .into_any_element()
    }

    // ---- 拖动：拖到主窗口的标签栏上松手 = 合并回主窗口 ----

    fn on_drag_move(
        &mut self,
        event: &::gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = (f32::from(event.position.x), f32::from(event.position.y));
        let left = event.pressed_button == Some(MouseButton::Left);
        if self.dragger.moved(local, left, window, cx) {
            cx.notify();
        }
    }

    fn on_drag_up(
        &mut self,
        _event: &::gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((crate::tab_drag::DragKey::Sftp(id), drop)) = self.dragger.release(cx)
            && let crate::tab_drag::Drop::Merge(target) = drop
            && main_window(cx).is_some_and(|(handle, _)| handle.window_id() == target)
        {
            self.move_to_main(id, window, cx);
        }
        cx.notify();
    }

    fn on_drag_cancel(&mut self, cx: &mut Context<Self>) {
        self.dragger.cancel(cx);
        cx.notify();
    }

    fn render_confirm(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.confirm.as_ref()?;
        let (busy, message, ok_label) = match confirm {
            Confirm::CloseTab(id) => (
                self.unfinished_of(*id, cx),
                "关闭这个标签会取消它们，并断开这个 SFTP",
                "关闭并取消",
            ),
            Confirm::CloseWindow => (
                self.unfinished_all(cx),
                "关闭窗口会取消它们，并断开这个窗口里的 SFTP",
                "关闭并取消",
            ),
        };
        let keep = cx.entity();
        let ok = cx.entity();
        let target = match confirm {
            Confirm::CloseTab(id) => Some(*id),
            Confirm::CloseWindow => None,
        };
        Some(
            h_flex()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .child(format!("还有 {busy} 个传输在进行，{message}")),
                )
                .child(
                    Button::new("file-window-keep")
                        .label("继续传输")
                        .compact()
                        .on_click(move |_, _, cx| {
                            keep.update(cx, |this, cx| {
                                this.confirm = None;
                                cx.notify();
                            });
                        }),
                )
                .child(
                    Button::new("file-window-close-now")
                        .label(ok_label)
                        .compact()
                        .on_click(move |_, window, cx| {
                            ok.update(cx, |this, cx| match target {
                                Some(id) => this.do_close_tab(id, window, cx),
                                None => this.confirmed_close_window(window, cx),
                            });
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Render for FileWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .map(|tab| self.render_tab(tab, &tokens, cx))
            .collect();
        let content: AnyElement = match self.active_view() {
            Some(view) => view.into_any_element(),
            None => div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(tokens.text_muted)
                .child("正在打开…")
                .into_any_element(),
        };
        let me_entity = cx.entity();
        let sensor = crate::tab_drag::sensor(
            &me_entity,
            self.dragger.pending(),
            |this, event, window, cx| this.on_drag_move(event, window, cx),
            |this, event, window, cx| this.on_drag_up(event, window, cx),
        );
        let me = crate::tab_drag::id_of(window);
        let drop_target = crate::tab_drag::current(cx).is_some_and(|drag| {
            drag.hover == crate::tab_drag::Hover::Over(me) && drag.source != me
        });
        let hint = crate::tab_drag::hint_chip(window, cx, &tokens);
        v_flex()
            .id("file-window")
            .size_full()
            .track_focus(&self.focus)
            .capture_key_down(
                cx.listener(|this, event: &::gpui::KeyDownEvent, _window, cx| {
                    // 正在拖标签：Esc 取消。
                    if this.dragger.pending() && event.keystroke.key == "escape" {
                        this.on_drag_cancel(cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .bg(tokens.background)
            .text_color(tokens.text)
            .ui_font_fallbacks(cx)
            .child(sensor)
            // 自绘标题栏（V2-U1 第 6 条）：背景是主题的面板色，空白处可以拖动窗口、双击最大化，
            // 右边是和主窗口一样的最小化 / 最大化 / 关闭；标签和按钮 `occlude` 把拖动区挖掉。
            .child(
                h_flex()
                    .id("file-window-titlebar")
                    .relative()
                    .flex_none()
                    .h(px(40.0))
                    .w_full()
                    .items_end()
                    .bg(if drop_target {
                        tokens.accent_soft
                    } else {
                        tokens.surface
                    })
                    .border_b_1()
                    .border_color(tokens.border)
                    .child(
                        h_flex()
                            .id("file-window-drag")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .items_end()
                            .gap_0p5()
                            .pl_3()
                            .window_control_area(WindowControlArea::Drag)
                            .child(
                                h_flex()
                                    .mb_2()
                                    .mr_2()
                                    .flex_none()
                                    .gap_1p5()
                                    .items_center()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(tokens.text_muted)
                                    .child(
                                        Icon::new(Ico::FolderTree)
                                            .size_3p5()
                                            .text_color(tokens.accent),
                                    )
                                    .child(window_title()),
                            )
                            .children(tabs)
                            .children(hint),
                    )
                    .child(
                        div()
                            .h_full()
                            .flex_none()
                            .child(window_controls(&tokens, window.is_maximized())),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
            .children(self.render_confirm(&tokens, cx))
    }
}

/// 现在开着的文件窗口。登记的句柄只有还在应用的窗口列表里才算数（窗口刚关、登记还没清的空档不算）。
pub fn window_handle(cx: &App) -> Option<WindowHandle<FileWindow>> {
    let handle = cx
        .try_global::<Registry>()
        .and_then(|registry| registry.window)?;
    cx.windows()
        .iter()
        .any(|window| window.window_id() == handle.window_id())
        .then_some(handle)
}

#[cfg(test)]
mod tests {
    use super::{active_after_close, should_merge_on_close, tab_label};

    #[test]
    fn tab_label_drops_the_sftp_suffix() {
        assert_eq!(tab_label("web-1 (SFTP)"), "web-1");
        assert_eq!(tab_label("db-2"), "db-2");
        assert_eq!(tab_label("a (SFTP) b"), "a (SFTP) b");
    }

    #[test]
    fn closing_the_active_tab_picks_the_left_neighbour_then_the_right() {
        let ids = [1, 2, 3];
        // 关的不是当前的：不变。
        assert_eq!(active_after_close(&ids, 1, Some(3)), Some(3));
        // 关当前的：优先左边。
        assert_eq!(active_after_close(&ids, 3, Some(3)), Some(2));
        assert_eq!(active_after_close(&ids, 2, Some(2)), Some(1));
        // 没有左边就右边。
        assert_eq!(active_after_close(&ids, 1, Some(1)), Some(2));
        // 最后一个：没有了。
        assert_eq!(active_after_close(&[7], 7, Some(7)), None);
    }

    #[test]
    fn merging_needs_the_setting_and_at_least_one_tab() {
        assert!(should_merge_on_close(Some("merge"), 2));
        assert!(!should_merge_on_close(Some("merge"), 0));
        assert!(!should_merge_on_close(Some("close"), 2));
        assert!(!should_merge_on_close(None, 2));
        assert!(!should_merge_on_close(Some("乱七八糟"), 2));
    }
}
