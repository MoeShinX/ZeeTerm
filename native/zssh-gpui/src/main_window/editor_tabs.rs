//! 主视图里的编辑器标签（0.2.14 `App.tsx` 的 `EditorTabSlot` + `EditorTabView.tsx` 外面那一层）。
//!
//! 远端文件的编辑器是主窗口的标签，和终端标签、SFTP 标签并列：
//! - 从 SFTP 视图的菜单 / 双击打开（`open_in_main_window`，用文件视图已经连好的通道）；
//!   同一台主机的同一个路径已经开着标签就切过去，不重复开（[`MainWindow::open_editor_tab`]）；
//! - 标签标题是文件名，有未保存修改时前面加 `●`，悬停显示 `主机：路径`；
//! - 关标签 / 退出时有未保存修改走合并的三选一确认（保存 / 不保存 / 取消，说法同 0.2.14 的
//!   `unsavedPrompt`）：一个确认框列出所有有改动的文件，退出时还带上传输、AI 回合这些别的后果；
//! - 编辑器自己带着通道，关掉终端标签 / SFTP 标签不会让它断线；真断了会出现「重新连接」。
//!
//! 编辑器本身在 `remote_editor`。

use ::gpui::{App, MouseButton, SharedString, Subscription, Window, px};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::modal::Modal;
use super::nav::MainView;
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::remote_editor::{CloseSave, EditorEvent, OpenSpec, Phase, RemoteEditor};

/// 本文件（编辑器标签、未保存修改确认）的界面文案。文件名、主机名、路径、保存失败原因来自
/// 用户、远端或核心库，照原样显示。
#[derive(Clone, Copy, Debug)]
enum EditorTabMsg {
    ReasonCloseTab,
    ReasonQuit,
    ExtraPrefix,
    ExtraMiddle,
    ExtraSuffix,
    QuoteOpen,
    QuoteClose,
    TitleSuffix,
    SingleSavePrompt,
    MultiSavePrompt,
    NameSeparator,
    EtcPrefix,
    FileCountSuffix,
    Ellipsis,
    SentenceEnd,
    SaveFailedSuffix,
    PathSeparator,
    CloseTab,
    Close,
    Cancel,
    DontSave,
    Saving,
    Save,
}

impl Msg for EditorTabMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ReasonCloseTab => "关闭这个标签",
            Self::ReasonQuit => "退出程序",
            Self::ExtraPrefix => "\n\n",
            Self::ExtraMiddle => "\n",
            Self::ExtraSuffix => "选「保存」或「不保存」都会继续退出，选「取消」留下。",
            Self::QuoteOpen => "「",
            Self::QuoteClose => "」",
            Self::TitleSuffix => "的修改还没有保存",
            Self::SingleSavePrompt => "之前要先保存吗？选「不保存」会丢掉这次的修改。",
            Self::MultiSavePrompt => "之前要先保存吗？选「不保存」会丢掉这些修改。",
            Self::NameSeparator => "、",
            Self::EtcPrefix => "等 ",
            Self::FileCountSuffix => " 个文件",
            Self::Ellipsis => "……",
            Self::SentenceEnd => "。",
            Self::SaveFailedSuffix => "没有保存：",
            Self::PathSeparator => "：",
            Self::CloseTab => "关闭标签",
            Self::Close => "关闭",
            Self::Cancel => "取消",
            Self::DontSave => "不保存",
            Self::Saving => "保存中…",
            Self::Save => "保存",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ReasonCloseTab => "Closing this tab",
            Self::ReasonQuit => "Quitting the app",
            Self::ExtraPrefix => "\n\n",
            Self::ExtraMiddle => "\n",
            Self::ExtraSuffix => {
                "Choosing “Save” or “Don't Save” continues quitting; choosing “Cancel” stays."
            }
            Self::QuoteOpen => "“",
            Self::QuoteClose => "”",
            Self::TitleSuffix => " — unsaved changes",
            Self::SingleSavePrompt => {
                " — save before continuing? Choosing “Don't Save” discards this change."
            }
            Self::MultiSavePrompt => {
                " — save before continuing? Choosing “Don't Save” discards these changes."
            }
            Self::NameSeparator => ", ",
            Self::EtcPrefix => "",
            Self::FileCountSuffix => " files",
            Self::Ellipsis => "",
            Self::SentenceEnd => ". ",
            Self::SaveFailedSuffix => " was not saved: ",
            Self::PathSeparator => ": ",
            Self::CloseTab => "Close Tab",
            Self::Close => "Close",
            Self::Cancel => "Cancel",
            Self::DontSave => "Don't Save",
            Self::Saving => "Saving…",
            Self::Save => "Save",
        }
    }
}

/// 主视图里的一个编辑器标签。
pub struct EditorTab {
    pub id: u64,
    pub view: Entity<RemoteEditor>,
    _sub: Subscription,
}

/// 这个询问是为了什么：关一个标签，还是退出整个程序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedPurpose {
    CloseTab(u64),
    Quit,
}

/// 未保存修改的三选一确认。
pub struct UnsavedAsk {
    pub purpose: UnsavedPurpose,
    pub title: String,
    pub message: String,
    /// 有未保存修改的编辑器标签。
    pub tabs: Vec<u64>,
    /// 选了「保存」，正在等保存结果。
    pub busy: bool,
    pub error: Option<String>,
}

/// 询问里一个文件的说法：文件名 + 路径（路径只在出错时有用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsavedFile {
    pub name: String,
    pub path: String,
}

/// 询问的原因（决定「……之前要先保存吗」那半句）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsavedReason {
    CloseTab,
    Quit,
}

/// 标题和说明，说法同 0.2.14 的 `unsavedPrompt`：一个文件点名，多个文件最多列 6 个。
/// `extra` 是退出时顺带要说明的其他后果（传输、AI 回合……），接在询问后面。
pub fn unsaved_prompt(
    files: &[UnsavedFile],
    reason: UnsavedReason,
    extra: Option<&str>,
) -> (String, String) {
    let what = match reason {
        UnsavedReason::CloseTab => t(EditorTabMsg::ReasonCloseTab),
        UnsavedReason::Quit => t(EditorTabMsg::ReasonQuit),
    };
    let tail = extra
        .map(|extra| {
            format!(
                "{}{extra}{}{}",
                t(EditorTabMsg::ExtraPrefix),
                t(EditorTabMsg::ExtraMiddle),
                t(EditorTabMsg::ExtraSuffix)
            )
        })
        .unwrap_or_default();
    if let [only] = files {
        return (
            format!(
                "{}{}{}{}",
                t(EditorTabMsg::QuoteOpen),
                only.name,
                t(EditorTabMsg::QuoteClose),
                t(EditorTabMsg::TitleSuffix)
            ),
            format!("{what}{}{tail}", t(EditorTabMsg::SingleSavePrompt)),
        );
    }
    let names = files
        .iter()
        .take(6)
        .map(|file| {
            format!(
                "{}{}{}",
                t(EditorTabMsg::QuoteOpen),
                file.name,
                t(EditorTabMsg::QuoteClose)
            )
        })
        .collect::<Vec<_>>()
        .join(t(EditorTabMsg::NameSeparator));
    let more = if files.len() > 6 {
        format!(
            "{}{}{}",
            t(EditorTabMsg::EtcPrefix),
            files.len(),
            t(EditorTabMsg::FileCountSuffix)
        )
    } else {
        format!("{}{}", files.len(), t(EditorTabMsg::FileCountSuffix))
    };
    let ellipsis = if files.len() > 6 {
        t(EditorTabMsg::Ellipsis)
    } else {
        ""
    };
    (
        format!("{more}{}", t(EditorTabMsg::TitleSuffix)),
        format!(
            "{names}{ellipsis}{}{what}{}{tail}",
            t(EditorTabMsg::SentenceEnd),
            t(EditorTabMsg::MultiSavePrompt)
        ),
    )
}

/// 在主窗口里开一个编辑器标签。主窗口不在（独立 SFTP 调试窗口）返回 `false`，调用方退回独立编辑窗口。
///
/// 放到下一个事件循环里做：调用方多半正在更新某个实体（文件视图），那一刻不能再更新主窗口。
pub(crate) fn open_in_main_window(cx: &mut App, spec: OpenSpec) -> bool {
    let Some((handle, view)) = crate::file_window::main_window(cx) else {
        return false;
    };
    cx.defer(move |cx| {
        let _ = handle.update(cx, move |_, window, cx| {
            let _ = view.update(cx, |main, cx| main.open_editor_tab(spec, window, cx));
            window.activate_window();
        });
    });
    true
}

impl MainWindow {
    // ---- 打开 / 切换 ----

    /// 开一个编辑器标签；同一个文件已经开着就切过去。
    pub(crate) fn open_editor_tab(
        &mut self,
        spec: OpenSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let identity = (spec.host_id, zssh_core::sftp::path::normalize(&spec.path));
        let existing = self
            .docks
            .editor_tabs
            .iter()
            .find(|tab| tab.view.read(cx).identity() == identity)
            .map(|tab| tab.id);
        if let Some(id) = existing {
            self.select_editor_tab(id, window, cx);
            return;
        }
        let id = self.docks.next_tab_id();
        let view = cx.new(|cx| RemoteEditor::new(window, cx, spec, false));
        let sub = cx.subscribe_in(
            &view,
            window,
            move |this, _view, event: &EditorEvent, window, cx| {
                this.on_editor_event(id, event, window, cx);
            },
        );
        self.docks.editor_tabs.push(EditorTab {
            id,
            view,
            _sub: sub,
        });
        self.select_editor_tab(id, window, cx);
    }

    pub(crate) fn select_editor_tab(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.docks.editor_tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        let focus = tab.view.read(cx).editor_focus(cx);
        self.nav = self.nav.show(MainView::Terminal);
        self.docks.active_sftp = None;
        self.docks.active_editor = Some(id);
        self.save_layout();
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(super) fn active_editor_view(&self) -> Option<Entity<RemoteEditor>> {
        let id = self.docks.active_editor?;
        self.docks
            .editor_tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.view.clone())
    }

    fn editor_tab_index(&self, id: u64) -> Option<usize> {
        self.docks.editor_tabs.iter().position(|tab| tab.id == id)
    }

    // ---- 关闭 ----

    /// 关一个编辑器标签：有未保存修改先问。
    pub(crate) fn close_editor_tab(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.editor_tab_index(id) else {
            return;
        };
        let view = self.docks.editor_tabs[index].view.clone();
        let unsaved = view.update(cx, |view, cx| view.has_unsaved(cx));
        if !unsaved {
            self.do_close_editor_tab(id, false, window, cx);
            return;
        }
        let file = view.read(cx);
        let files = [UnsavedFile {
            name: file.file_name(),
            path: file.path().to_owned(),
        }];
        let (title, message) = unsaved_prompt(&files, UnsavedReason::CloseTab, None);
        self.modal = Some(Modal::Unsaved(UnsavedAsk {
            purpose: UnsavedPurpose::CloseTab(id),
            title,
            message,
            tabs: vec![id],
            busy: false,
            error: None,
        }));
        cx.notify();
    }

    /// 真的关掉。`discard` = 用户选了「不保存」：未保存的草稿一起删。
    pub(crate) fn do_close_editor_tab(
        &mut self,
        id: u64,
        discard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.editor_tab_index(id) else {
            return;
        };
        let tab = self.docks.editor_tabs.remove(index);
        tab.view.update(cx, |view, _| {
            if discard {
                view.discard_unsaved_state();
            }
            view.shutdown();
        });
        if self.docks.active_editor == Some(id) {
            self.docks.active_editor = None;
            // 回到旁边的编辑器标签，没有就回 SFTP / 终端 / 主机页。
            let next = self
                .docks
                .editor_tabs
                .get(index)
                .or_else(|| self.docks.editor_tabs.last())
                .map(|tab| tab.id);
            match next {
                Some(next) => self.select_editor_tab(next, window, cx),
                None => self.fall_back_after_extra_tab(window, cx),
            }
        }
        cx.notify();
    }

    /// 主视图里没有「额外标签」（SFTP / 编辑器）可停了：回终端或主机页。
    pub(super) fn fall_back_after_extra_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.docks.sftp_tabs.last() {
            let id = tab.id;
            self.select_sftp_tab(id, window, cx);
            return;
        }
        if let Some(tab) = self.docks.editor_tabs.last() {
            let id = tab.id;
            self.select_editor_tab(id, window, cx);
            return;
        }
        if self.workspace.read(cx).tab_count() == 0 {
            self.nav = self.nav.tabs_emptied();
        }
        self.save_layout();
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 编辑器通知宿主：保存状态 / 未保存标记 / 路径变了。
    fn on_editor_event(
        &mut self,
        _id: u64,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, EditorEvent::Changed) {
            self.progress_unsaved_ask(window, cx);
        }
        cx.notify();
    }

    // ---- 未保存修改的合并确认 ----

    /// 所有有未保存修改的编辑器标签。
    pub(crate) fn unsaved_editor_tabs(&self, cx: &mut Context<Self>) -> Vec<u64> {
        let tabs: Vec<(u64, Entity<RemoteEditor>)> = self
            .docks
            .editor_tabs
            .iter()
            .map(|tab| (tab.id, tab.view.clone()))
            .collect();
        tabs.into_iter()
            .filter(|(_, view)| view.update(cx, |view, cx| view.has_unsaved(cx)))
            .map(|(id, _)| id)
            .collect()
    }

    /// 退出前：有未保存的编辑器就弹合并确认（`extra` 是别的后果）。返回 `true` = 弹了框，先别退。
    pub(crate) fn ask_unsaved_before_quit(
        &mut self,
        extra: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let tabs = self.unsaved_editor_tabs(cx);
        if tabs.is_empty() {
            return false;
        }
        let files: Vec<UnsavedFile> = tabs
            .iter()
            .filter_map(|id| {
                let tab = self.docks.editor_tabs.iter().find(|tab| tab.id == *id)?;
                let view = tab.view.read(cx);
                Some(UnsavedFile {
                    name: view.file_name(),
                    path: view.path().to_owned(),
                })
            })
            .collect();
        let (title, message) = unsaved_prompt(&files, UnsavedReason::Quit, extra.as_deref());
        self.modal = Some(Modal::Unsaved(UnsavedAsk {
            purpose: UnsavedPurpose::Quit,
            title,
            message,
            tabs,
            busy: false,
            error: None,
        }));
        cx.notify();
        true
    }

    /// 确认框上的按钮：`Some(true)` 保存、`Some(false)` 不保存；取消走 `close_modal`。
    pub(crate) fn answer_unsaved(
        &mut self,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Unsaved(ask)) = &self.modal else {
            return;
        };
        if ask.busy {
            return;
        }
        let (purpose, tabs) = (ask.purpose, ask.tabs.clone());
        if save {
            if let Some(Modal::Unsaved(ask)) = &mut self.modal {
                ask.busy = true;
                ask.error = None;
            }
            for id in &tabs {
                if let Some(tab) = self.docks.editor_tabs.iter().find(|tab| tab.id == *id) {
                    tab.view.update(cx, |view, cx| view.begin_close_save(cx));
                }
            }
            cx.notify();
            // 可能已经有结果了（保存没能开始、或者别的编辑器本来就没有改动）。
            self.progress_unsaved_ask(window, cx);
            return;
        }
        // 不保存：丢掉这些修改（草稿一起删），然后继续原来要做的事。
        self.modal = None;
        match purpose {
            UnsavedPurpose::CloseTab(id) => self.do_close_editor_tab(id, true, window, cx),
            UnsavedPurpose::Quit => {
                for id in &tabs {
                    if let Some(tab) = self.docks.editor_tabs.iter().find(|tab| tab.id == *id) {
                        tab.view.update(cx, |view, _| view.discard_unsaved_state());
                    }
                }
                self.do_quit(window, cx);
            }
        }
        cx.notify();
    }

    /// 选了「保存」之后看各编辑器的保存结果：全成功就继续，有失败就把原因写在框里、让用户重选。
    pub(crate) fn progress_unsaved_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Unsaved(ask)) = &self.modal else {
            return;
        };
        if !ask.busy {
            return;
        }
        let (purpose, tabs) = (ask.purpose, ask.tabs.clone());
        let mut failure: Option<String> = None;
        let mut waiting = false;
        for id in &tabs {
            let Some(tab) = self.docks.editor_tabs.iter().find(|tab| tab.id == *id) else {
                continue;
            };
            let view = tab.view.read(cx);
            match view.close_save_state() {
                Some(CloseSave::Done) | None => {}
                Some(CloseSave::Saving) => waiting = true,
                Some(CloseSave::Failed(reason)) => {
                    failure = Some(format!(
                        "{}{}{}{}{reason}",
                        t(EditorTabMsg::QuoteOpen),
                        view.file_name(),
                        t(EditorTabMsg::QuoteClose),
                        t(EditorTabMsg::SaveFailedSuffix)
                    ));
                }
            }
        }
        if let Some(message) = failure {
            for id in &tabs {
                if let Some(tab) = self.docks.editor_tabs.iter().find(|tab| tab.id == *id) {
                    tab.view.update(cx, |view, _| view.clear_close_save());
                }
            }
            if let Some(Modal::Unsaved(ask)) = &mut self.modal {
                ask.busy = false;
                ask.error = Some(message);
            }
            cx.notify();
            return;
        }
        if waiting {
            return;
        }
        self.modal = None;
        match purpose {
            UnsavedPurpose::CloseTab(id) => self.do_close_editor_tab(id, false, window, cx),
            UnsavedPurpose::Quit => self.do_quit(window, cx),
        }
        cx.notify();
    }

    /// 窗口真正关闭（退出）：编辑器收尾。没保存的内容先落草稿并等写线程处理完，下次启动能找回来。
    pub(super) fn shutdown_editors(&mut self, cx: &mut Context<Self>) {
        for tab in self.docks.editor_tabs.drain(..) {
            tab.view.update(cx, |view, cx| {
                if view.has_unsaved(cx) {
                    view.persist_draft_now(cx);
                }
                view.shutdown();
            });
        }
        self.docks.active_editor = None;
    }

    // ---- 标题栏上的标签 ----

    pub(super) fn render_editor_tab(
        &self,
        tab: &EditorTab,
        index: usize,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let active =
            self.docks.active_editor == Some(tab.id) && self.nav.view == MainView::Terminal;
        let id = tab.id;
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let select = entity.clone();
        let close = entity.clone();
        let middle = entity.clone();
        let menu_close = entity.clone();
        let (name, tip, dirty, loading) = {
            let view = tab.view.read(cx);
            (
                view.file_name(),
                format!(
                    "{}{}{}",
                    view.host_label(),
                    t(EditorTabMsg::PathSeparator),
                    view.path()
                ),
                view.unsaved_hint(),
                view.phase() != Phase::Ready,
            )
        };
        let label: SharedString = if dirty {
            format!("● {name}").into()
        } else {
            name.into()
        };
        let tip: SharedString = tip.into();
        div()
            .id(("editor-tab", index))
            .occlude()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.0))
            .min_w(px(112.0))
            .max_w(px(220.0))
            .pl_3()
            .pr_1p5()
            .flex_shrink(1.0)
            .rounded_t_md()
            .border_x_1()
            .border_t_1()
            .text_sm()
            .cursor_pointer()
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
            .when(active, |d| {
                d.child(super::titlebar::active_tab_indicator(tokens))
            })
            .tooltip({
                let tip = tip.clone();
                move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
            })
            .child(Icon::new(Ico::FileText).size_3p5().text_color(if loading {
                muted
            } else {
                tokens.accent
            }))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                div()
                    .id(("editor-tab-close", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .flex_none()
                    .rounded_sm()
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(text))
                    .tooltip(|window, cx| Tooltip::new(t(EditorTabMsg::CloseTab)).build(window, cx))
                    .child(Icon::new(IconName::Close).size_3())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close.update(cx, |this, cx| this.close_editor_tab(id, window, cx));
                    }),
            )
            .on_click(move |_, window, cx| {
                select.update(cx, |this, cx| this.select_editor_tab(id, window, cx));
            })
            .on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                middle.update(cx, |this, cx| this.close_editor_tab(id, window, cx));
            })
            .context_menu(move |menu, _window, cx| {
                let tokens = Tokens::current(cx);
                let close = menu_close.clone();
                menu.item(
                    crate::menu_kit::row(&tokens, t(EditorTabMsg::Close), Ico::X).on_click(
                        move |_, window, cx| {
                            close.update(cx, |this, cx| this.close_editor_tab(id, window, cx));
                        },
                    ),
                )
            })
            .into_any_element()
    }

    // ---- 询问框 ----

    pub(super) fn unsaved_body(
        &self,
        ask: &UnsavedAsk,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use super::widgets::{ButtonLook, error_note, text_button};
        let cancel = cx.entity();
        let discard = cancel.clone();
        let save = cancel.clone();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(ask.title.clone())),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(tokens.text_muted)
                            .child(SharedString::from(ask.message.clone())),
                    ),
            )
            .children(
                ask.error
                    .clone()
                    .map(|error| error_note(error, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "unsaved-cancel",
                        t(EditorTabMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "unsaved-discard",
                        t(EditorTabMsg::DontSave),
                        ButtonLook::Danger,
                        move |_, window, cx| {
                            discard.update(cx, |this, cx| this.answer_unsaved(false, window, cx));
                        },
                    ))
                    .child(text_button(
                        "unsaved-save",
                        if ask.busy {
                            t(EditorTabMsg::Saving)
                        } else {
                            t(EditorTabMsg::Save)
                        },
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            save.update(cx, |this, cx| this.answer_unsaved(true, window, cx));
                        },
                    )),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{UnsavedFile, UnsavedReason, unsaved_prompt};

    fn file(name: &str) -> UnsavedFile {
        UnsavedFile {
            name: name.to_owned(),
            path: format!("/srv/{name}"),
        }
    }

    #[test]
    fn one_file_is_named_in_the_title() {
        let (title, message) = unsaved_prompt(&[file("a.conf")], UnsavedReason::CloseTab, None);
        assert_eq!(title, "「a.conf」的修改还没有保存");
        assert!(
            message.starts_with("关闭这个标签之前要先保存吗？"),
            "{message}"
        );
        assert!(
            message.contains("选「不保存」会丢掉这次的修改"),
            "{message}"
        );
    }

    #[test]
    fn several_files_are_listed_up_to_six() {
        let files: Vec<_> = ["a", "b", "c"].into_iter().map(file).collect();
        let (title, message) = unsaved_prompt(&files, UnsavedReason::Quit, None);
        assert_eq!(title, "3 个文件的修改还没有保存");
        assert!(
            message.starts_with("「a」、「b」、「c」。退出程序之前"),
            "{message}"
        );
        let many: Vec<_> = (0..8).map(|n| file(&format!("f{n}"))).collect();
        let (title, message) = unsaved_prompt(&many, UnsavedReason::Quit, None);
        assert_eq!(title, "等 8 个文件的修改还没有保存");
        assert!(message.contains("「f5」……"), "{message}");
        assert!(!message.contains("f6"), "{message}");
    }

    /// 退出时把别的后果（传输、AI 回合）写进同一个框，选保存 / 不保存都会继续退出。
    #[test]
    fn quitting_merges_the_other_consequences_into_one_prompt() {
        let (_, message) = unsaved_prompt(
            &[file("a")],
            UnsavedReason::Quit,
            Some("还有 2 个文件传输在进行，现在退出会中断它们。"),
        );
        assert!(message.contains("还有 2 个文件传输"), "{message}");
        assert!(message.contains("都会继续退出"), "{message}");
    }
}
