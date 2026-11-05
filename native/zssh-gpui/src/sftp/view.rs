//! 文件视图的渲染：把 `SftpDemo` 的状态画成界面。
//!
//! 外观对照 0.2.14 的 `SftpFilePane.tsx`（工具栏、列头、虚拟列表、状态栏）和 `SftpTabView.tsx`
//! （两栏 / 停靠布局、传输队列）。颜色一律从 `Tokens` 取，深浅两套主题自动跟着变。
//! 对话框和右键菜单在 `view_dialogs.rs`。

use std::ops::Range;

use ::gpui::{ExternalPaths, MouseButton, MouseDownEvent, SharedString, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::sftp::path::file_name;
use zssh_core::sftp::transfer::{ConflictAction, Direction, TransferState};

use super::drag::{SftpDrag, SftpDragChip};
use super::engine::{conflict_compare, state_label};
use super::menu::Side;
use super::model::{
    Column, Entry, EntryKind, LinkBase, LinkInfo, adaptive_columns, cell_text, collapse_crumbs,
    compact_columns, local_crumbs, remote_crumbs,
};
use super::{Phase, SftpDemo, SftpMsg, queue};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::typography::{Role, Typo as _};

/// 本文件（文件视图的界面外壳）自己的文案。
#[derive(Clone, Copy, Debug)]
enum SftpViewMsg {
    CloseBusyPrefix,
    CloseBusySuffix,
    KeepTransferring,
    CancelAndClose,
    FollowCwd,
    FollowCwdTooltip,
    Back,
    Forward,
    Up,
    Refresh,
    HideHidden,
    ShowHidden,
    Filter,
    CloseLocalPane,
    ReadingDir,
    NoMatches,
    NotConnected,
    EmptyDir,
    ThisComputer,
    NotUtf8Badge,
    Cancelling,
    Cancel,
    RelayRetry,
    Close,
    CancelDelete,
    ConflictTitlePrefix,
    ConflictWaitingPrefix,
    ConflictWaitingSuffix,
    SourcePrefix,
    TargetPrefix,
    Overwrite,
    Skip,
    Resume,
    Rename,
    OverwriteAll,
    SkipAll,
    QueueTitle,
    ActiveSuffix,
    Collapse,
    QueueTotalPrefix,
    QueueTotalSuffix,
    QueueEmpty,
    ClearFinished,
    Pause,
    ResumeTransfer,
    Retry,
    CancelTransfer,
    Failed,
    SelectedPrefix,
}

impl Msg for SftpViewMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::CloseBusyPrefix => "还有 ",
            Self::CloseBusySuffix => " 项传输 / 删除没完成，关闭窗口会取消它们",
            Self::KeepTransferring => "继续传输",
            Self::CancelAndClose => "取消并关闭",
            Self::FollowCwd => "跟随终端目录",
            Self::FollowCwdTooltip => {
                "终端里 cd 之后，远端栏跟着换目录；在面板里手动换目录会自动关掉"
            }
            Self::Back => "后退 (Alt+←)",
            Self::Forward => "前进 (Alt+→)",
            Self::Up => "返回上级 (Backspace)",
            Self::Refresh => "刷新 (F5)",
            Self::HideHidden => "隐藏隐藏文件",
            Self::ShowHidden => "显示隐藏文件",
            Self::Filter => "过滤",
            Self::CloseLocalPane => "关闭本地栏",
            Self::ReadingDir => "读取目录中…",
            Self::NoMatches => "没有匹配的条目",
            Self::NotConnected => "还没连上",
            Self::EmptyDir => "空目录",
            Self::ThisComputer => "此电脑",
            Self::NotUtf8Badge => "非 UTF-8 名字",
            Self::Cancelling => "取消中…",
            Self::Cancel => "取消",
            Self::RelayRetry => "改为经本机中转重试",
            Self::Close => "关闭",
            Self::CancelDelete => "取消删除",
            Self::ConflictTitlePrefix => "同名文件：",
            Self::ConflictWaitingPrefix => "另有 ",
            Self::ConflictWaitingSuffix => " 个在等",
            Self::SourcePrefix => "源 ",
            Self::TargetPrefix => "目标 ",
            Self::Overwrite => "覆盖",
            Self::Skip => "跳过",
            Self::Resume => "续传",
            Self::Rename => "重命名",
            Self::OverwriteAll => "全部覆盖",
            Self::SkipAll => "全部跳过",
            Self::QueueTitle => "传输队列",
            Self::ActiveSuffix => " 个进行中",
            Self::Collapse => "收起",
            Self::QueueTotalPrefix => "共 ",
            Self::QueueTotalSuffix => " 个任务",
            Self::QueueEmpty => {
                "还没有传输任务。右键选“上传到远端 / 下载到本地”，或者把文件拖进来。"
            }
            Self::ClearFinished => "清除已完成",
            Self::Pause => "暂停",
            Self::ResumeTransfer => "继续",
            Self::Retry => "重试",
            Self::CancelTransfer => "取消",
            Self::Failed => "失败",
            Self::SelectedPrefix => "选中 ",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::CloseBusyPrefix => "",
            Self::CloseBusySuffix => {
                " transfer / delete tasks are unfinished; closing the window will cancel them"
            }
            Self::KeepTransferring => "Keep Transferring",
            Self::CancelAndClose => "Cancel and Close",
            Self::FollowCwd => "Follow Terminal Directory",
            Self::FollowCwdTooltip => {
                "After cd in the terminal, the remote pane follows; changing it manually in the pane turns this off"
            }
            Self::Back => "Back (Alt+←)",
            Self::Forward => "Forward (Alt+→)",
            Self::Up => "Up (Backspace)",
            Self::Refresh => "Refresh (F5)",
            Self::HideHidden => "Hide Hidden Files",
            Self::ShowHidden => "Show Hidden Files",
            Self::Filter => "Filter",
            Self::CloseLocalPane => "Close Local Pane",
            Self::ReadingDir => "Reading directory…",
            Self::NoMatches => "No matching entries",
            Self::NotConnected => "Not connected",
            Self::EmptyDir => "Empty directory",
            Self::ThisComputer => "This PC",
            Self::NotUtf8Badge => "Non-UTF-8 name",
            Self::Cancelling => "Cancelling…",
            Self::Cancel => "Cancel",
            Self::RelayRetry => "Retry via This Computer",
            Self::Close => "Close",
            Self::CancelDelete => "Cancel Delete",
            Self::ConflictTitlePrefix => "Name conflict: ",
            Self::ConflictWaitingPrefix => "",
            Self::ConflictWaitingSuffix => " more waiting",
            Self::SourcePrefix => "Source ",
            Self::TargetPrefix => "Target ",
            Self::Overwrite => "Overwrite",
            Self::Skip => "Skip",
            Self::Resume => "Resume",
            Self::Rename => "Rename",
            Self::OverwriteAll => "Overwrite All",
            Self::SkipAll => "Skip All",
            Self::QueueTitle => "Transfer Queue",
            Self::ActiveSuffix => " active",
            Self::Collapse => "Collapse",
            Self::QueueTotalPrefix => "",
            Self::QueueTotalSuffix => " tasks total",
            Self::QueueEmpty => {
                "No transfers yet. Right-click and choose \"Upload to Remote / Download to Local\", or drag files in."
            }
            Self::ClearFinished => "Clear Finished",
            Self::Pause => "Pause",
            Self::ResumeTransfer => "Resume",
            Self::Retry => "Retry",
            Self::CancelTransfer => "Cancel",
            Self::Failed => "Failed",
            Self::SelectedPrefix => "Selected ",
        }
    }
}

/// 顶部信息行的高度：和标签条、列头同高（32），不比它们高。
const HEADER_HEIGHT: f32 = 32.0;

/// 工具栏里过滤框的宽度：栏宽够就 120，窗口收窄时先让过滤框让位，免得它把路径栏挤没、
/// 还伸出栏外（独立文件窗口最窄 640，两栏各约 316）。停靠面板里一直是窄版。宽度还没量到（0）按宽版。
pub(super) fn filter_width(dock: bool, pane_width: f32) -> f32 {
    if dock {
        return 84.0;
    }
    match pane_width {
        w if w <= 0.0 || w >= 520.0 => 120.0,
        w if w >= 400.0 => 96.0,
        _ => 72.0,
    }
}

/// 窄栏（独立文件窗口两栏各不到 400）里过滤框收成一个放大镜按钮，把地方让给路径栏，点开才是输入框。
/// 停靠面板的工具栏一直是固定窄版，不收。宽度还没量到（0）按宽版。
pub(super) fn filter_compact(dock: bool, pane_width: f32) -> bool {
    !dock && pane_width > 0.0 && pane_width < 400.0
}

/// 这一栏的过滤框现在画成输入框还是放大镜按钮：窄栏里没点开、也没在过滤时才收起。
fn filter_collapsed(dock: bool, pane_width: f32, open: bool, text: &str) -> bool {
    filter_compact(dock, pane_width) && !open && text.is_empty()
}

/// 工具栏里过滤那一格占多宽：收起时是一个按钮（约 30），展开的窄栏给 120 好打字。
fn filter_slot(dock: bool, pane_width: f32, collapsed: bool) -> f32 {
    if collapsed {
        30.0
    } else if filter_compact(dock, pane_width) {
        120.0
    } else {
        filter_width(dock, pane_width)
    }
}

/// 列表一行的高度（0.2.14 是 26，V2-U14 起 14px 文字配 34px 行，不挤）。
const ROW_HEIGHT: f32 = 34.0;

/// 盘符图标的颜色（0.2.14 的 `text-sky-400`）和符号链接（`text-emerald-500`）；文件夹是固定的黄色，见 `icons::folder_icon`。
const FOLDER_COLOR: u32 = 0x38BDF8;
const LINK_COLOR: u32 = 0x10B981;

/// 链接的图标（Xftp 那种）：底是目标类型的图标（文件夹 / 文件），左下角叠一个小的快捷方式箭头
/// 表示「这是链接」；断链的角标换成警示色。
fn link_icon(info: &LinkInfo, tokens: &Tokens) -> impl IntoElement {
    let base = match info.base {
        LinkBase::Folder => crate::icons::folder_icon(false, 16.0).into_any_element(),
        LinkBase::File => Icon::new(IconName::File)
            .size_4()
            .text_color(tokens.text_muted)
            .into_any_element(),
    };
    // 角标：白底 + 蓝箭头；断链是琥珀底 + 深色箭头。颜色固定（深浅主题里都清楚）。
    let (badge_bg, arrow) = if info.broken {
        (rgb(0xFBBF24), rgb(0x7C2D12))
    } else {
        (rgb(0xFFFFFF), rgb(0x1D4ED8))
    };
    div()
        .relative()
        .size(px(16.0))
        .flex_none()
        .child(base)
        .child(
            div()
                .absolute()
                .left(px(-3.0))
                .bottom(px(-2.0))
                .flex()
                .items_center()
                .justify_center()
                .size(px(10.0))
                .rounded(px(2.0))
                .bg(badge_bg)
                .child(Icon::new(Ico::ArrowUpRight).size(px(8.0)).text_color(arrow)),
        )
}

fn rgb(hex: u32) -> Hsla {
    ::gpui::rgb(hex).into()
}

fn current_year(offset: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    crate::datetime::civil_from_days((now + offset).div_euclid(86_400)).0
}

fn tool_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tip: &'static str,
    disabled: bool,
) -> Button {
    Button::new(id)
        .ghost()
        .compact()
        .icon(icon.into())
        .tooltip(tip)
        .disabled(disabled)
}

impl Render for SftpDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        let entity = cx.entity();
        let dock = self.dock;

        // 顶部信息行：主机显示名 · 状态小点 + 状态。不再写"SFTP"：窗口 / 标签本身已经说明这是文件视图，
        // 大号字只会和主窗口的排版对不上（V2-U24）。字号字重一律走 `Role`。
        let header = h_flex()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(tokens.border)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .typo(Role::ItemName, &tokens)
                    .child(self.host_label.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .typo(Role::Caption, &tokens)
                    .text_color(tokens.text_faint)
                    .child("·"),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .items_center()
                    .typo(Role::Caption, &tokens)
                    .child(
                        div()
                            .size(px(8.0))
                            .flex_none()
                            .rounded_full()
                            .bg(super::header::phase_color(self.phase, &tokens)),
                    )
                    .child(super::header::phase_label(self.phase)),
            );

        let remote_pane = self.render_pane(Side::Remote, &tokens, cx);
        let local_pane = self.render_pane(Side::Local, &tokens, cx);
        let body = if dock {
            let mut body = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap_1p5()
                .p_1p5()
                .child(div().flex().flex_1().min_h_0().min_w_0().child(remote_pane));
            if !self.local_hidden {
                body = body.child(
                    div()
                        .flex()
                        .flex_none()
                        .h(px(190.0))
                        .min_w_0()
                        .child(local_pane),
                );
            }
            body
        } else {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .gap_2()
                .p_2()
                .child(div().flex().flex_1().min_w_0().child(local_pane))
                .child(div().flex().flex_1().min_w_0().child(remote_pane))
        };

        // 窗口外壳的确认条：关窗口时还有没完成的传输 / 删除。
        let close_bar = self.close_prompt.then(|| {
            let keep_entity = entity.clone();
            let close_entity = entity.clone();
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .child(div().flex_1().typo(Role::Body, &tokens).child(format!(
                    "{}{}{}",
                    t(SftpViewMsg::CloseBusyPrefix),
                    self.unfinished_work(),
                    t(SftpViewMsg::CloseBusySuffix)
                )))
                .child(
                    Button::new("sftp-close-keep")
                        .label(t(SftpViewMsg::KeepTransferring))
                        .compact()
                        .on_click(move |_, _, cx| {
                            keep_entity.update(cx, |this, cx| {
                                this.close_prompt = false;
                                cx.notify();
                            });
                        }),
                )
                .child(
                    Button::new("sftp-close-now")
                        .label(t(SftpViewMsg::CancelAndClose))
                        .compact()
                        .on_click(move |_, window, cx| {
                            close_entity.update(cx, |this, _cx| {
                                this.close_confirmed = true;
                                this.close();
                            });
                            window.remove_window();
                        }),
                )
                .into_any_element()
        });

        // 停靠面板里的「跟随终端目录」开关（0.2.14 面板工具条）：终端里 cd 之后远端栏跟着换，
        // 在面板里手动换远端目录会自动关掉。只在连上之后显示。
        let follow_bar = (dock && self.phase == Phase::Ready).then(|| {
            let follow_entity = entity.clone();
            h_flex()
                .px_3()
                .py_2()
                .typo(Role::Body, &tokens)
                .border_b_1()
                .border_color(tokens.border)
                .child(
                    Checkbox::new("sftp-follow-cwd")
                        .checked(self.follow)
                        .label(t(SftpViewMsg::FollowCwd))
                        .tooltip(t(SftpViewMsg::FollowCwdTooltip))
                        .on_click(move |checked, _, cx| {
                            let checked = *checked;
                            follow_entity.update(cx, |this, cx| this.set_follow(checked, cx));
                        }),
                )
                .into_any_element()
        });

        let overlay = self.render_dialog(&tokens, window, cx);
        v_flex()
            .id("sftp-root")
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .bg(tokens.background)
            .typo(Role::Body, &tokens)
            .when(!dock, |root| root.child(header))
            .children(follow_bar)
            .child(body)
            .children(self.render_busy_bar(&tokens, cx))
            .children(self.render_delete_bar(&tokens, cx))
            .children(self.render_paste_retry_bar(&tokens, cx))
            .children(close_bar)
            .children(self.render_conflict_panel(&tokens, cx))
            .child(self.render_queue(&tokens, cx))
            .child(
                h_flex()
                    .h(px(26.0))
                    .px_3()
                    .items_center()
                    .border_t_1()
                    .border_color(tokens.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .typo(Role::Caption, &tokens)
                            .text_color(if self.status_is_error {
                                tokens.danger
                            } else {
                                tokens.text_muted
                            })
                            .child(self.status.clone()),
                    ),
            )
            .children(overlay)
    }
}

// ---------------------------------------------------------------------------
// 一栏
// ---------------------------------------------------------------------------

impl SftpDemo {
    fn render_pane(&mut self, side: Side, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let dock = self.dock;
        let focused = self.focused_side == side;
        let is_remote = side == Side::Remote;
        let offset = crate::datetime::local_utc_offset();
        let now_year = current_year(offset);

        // 量到的宽度：和上一帧用的差得多就重画一次（面包屑折叠、窄版列精简靠它）。
        let used = {
            let pane = self.pane_mut(side);
            pane.used_width = pane.measured_width.get();
            pane.used_width
        };
        let columns = if dock {
            compact_columns(if used > 0.0 { used } else { 10_000.0 })
        } else {
            adaptive_columns(is_remote, used)
        };

        let pane = self.pane(side);
        let can_back = pane.history.can_back();
        let can_forward = pane.history.can_forward();
        let at_top = if is_remote {
            pane.path.is_empty() || pane.path == "/"
        } else {
            pane.path.is_empty()
        };
        let show_hidden = pane.filter.show_hidden;
        let loading = pane.loading;
        let measure = pane.measured_width.clone();
        let entity_id = cx.entity_id();

        let id_suffix = if is_remote { "remote" } else { "local" };
        let toolbar = {
            let (e_back, e_fwd, e_up, e_refresh, e_hidden, e_close) = (
                entity.clone(),
                entity.clone(),
                entity.clone(),
                entity.clone(),
                entity.clone(),
                entity.clone(),
            );
            h_flex()
                .h(px(36.0))
                .flex_none()
                .px_2()
                .gap_1()
                .items_center()
                .border_b_1()
                .border_color(tokens.border)
                .child(
                    tool_button(
                        SharedString::from(format!("sftp-back-{id_suffix}")),
                        IconName::ArrowLeft,
                        t(SftpViewMsg::Back),
                        !can_back,
                    )
                    .on_click(move |_, _, cx| {
                        e_back.update(cx, |this, cx| this.step_history(side, -1, cx));
                    }),
                )
                // 窄栏里不放「前进」（Alt+→ 照样能用），把地方让给路径栏。
                .when(!filter_compact(dock, used), |bar| {
                    bar.child(
                        tool_button(
                            SharedString::from(format!("sftp-forward-{id_suffix}")),
                            IconName::ArrowRight,
                            t(SftpViewMsg::Forward),
                            !can_forward,
                        )
                        .on_click(move |_, _, cx| {
                            e_fwd.update(cx, |this, cx| this.step_history(side, 1, cx));
                        }),
                    )
                })
                .child(
                    tool_button(
                        SharedString::from(format!("sftp-up-{id_suffix}")),
                        IconName::ArrowUp,
                        t(SftpViewMsg::Up),
                        at_top,
                    )
                    .on_click(move |_, _, cx| {
                        e_up.update(cx, |this, cx| this.go_parent(side, cx));
                    }),
                )
                .child(self.render_path_bar(side, tokens, used, cx))
                .child(
                    tool_button(
                        SharedString::from(format!("sftp-refresh-{id_suffix}")),
                        IconName::RefreshCw,
                        t(SftpViewMsg::Refresh),
                        false,
                    )
                    .loading(loading)
                    .on_click(move |_, _, cx| {
                        e_refresh.update(cx, |this, cx| this.refresh(side, cx));
                    }),
                )
                .child(
                    tool_button(
                        SharedString::from(format!("sftp-hidden-{id_suffix}")),
                        if show_hidden {
                            IconName::Eye
                        } else {
                            IconName::EyeOff
                        },
                        if show_hidden {
                            t(SftpViewMsg::HideHidden)
                        } else {
                            t(SftpViewMsg::ShowHidden)
                        },
                        false,
                    )
                    .on_click(move |_, _, cx| {
                        e_hidden.update(cx, |this, cx| this.toggle_hidden(side, cx));
                    }),
                )
                .map(|bar| {
                    let collapsed =
                        filter_collapsed(dock, used, pane.filter_open, &pane.filter.text);
                    if collapsed {
                        let e_filter = entity.clone();
                        let input = pane.filter_input.clone();
                        bar.child(
                            tool_button(
                                SharedString::from(format!("sftp-filter-open-{id_suffix}")),
                                IconName::Search,
                                t(SftpViewMsg::Filter),
                                false,
                            )
                            .on_click(move |_, window, cx| {
                                e_filter.update(cx, |this, cx| {
                                    this.pane_mut(side).filter_open = true;
                                    cx.notify();
                                });
                                let focus = input.read(cx).focus_handle(cx);
                                window.focus(&focus, cx);
                            }),
                        )
                    } else {
                        bar.child(
                            div()
                                .w(px(filter_slot(dock, used, false)))
                                .flex_none()
                                .child(
                                    Input::new(&pane.filter_input).prefix(
                                        Icon::new(IconName::Search)
                                            .size_3()
                                            .text_color(tokens.text_muted),
                                    ),
                                ),
                        )
                    }
                })
                .when(dock && !is_remote, |bar| {
                    bar.child(
                        tool_button(
                            "sftp-close-local-pane",
                            IconName::Close,
                            t(SftpViewMsg::CloseLocalPane),
                            false,
                        )
                        .on_click(move |_, _, cx| {
                            e_close.update(cx, |this, cx| this.set_local_hidden(true, cx));
                        }),
                    )
                })
        };

        // 列头：和数据行共用同一份列宽。
        let header = {
            let order = pane.order;
            h_flex()
                .h(px(32.0))
                .flex_none()
                .px_2()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(tokens.border)
                .typo(Role::FieldLabel, tokens)
                .text_color(tokens.text_muted)
                .child(div().w(px(16.0)).flex_none())
                .children(columns.iter().copied().map(|column| {
                    let head_entity = entity.clone();
                    let sortable = column.sort_key();
                    let active = sortable.is_some_and(|key| order.key == key);
                    let arrow = active.then_some(if order.descending {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    });
                    let cell = h_flex()
                        .id(SharedString::from(format!(
                            "sftp-col-{id_suffix}-{}",
                            column.stable_id()
                        )))
                        .gap_0p5()
                        .items_center()
                        .rounded_sm()
                        .when(column.right_aligned(), |d| d.justify_end())
                        .when(active, |d| d.text_color(tokens.text))
                        .child(div().truncate().child(column.label()))
                        .children(arrow.map(|icon| Icon::new(icon).size_3()));
                    let cell = match column.width() {
                        Some(width) => cell.w(px(width)).flex_none(),
                        None => cell.flex_1().min_w_0(),
                    };
                    match sortable {
                        Some(key) => {
                            let hover = tokens.hover;
                            cell.cursor_pointer()
                                .hover(move |style| style.bg(hover))
                                .on_click(move |_, _, cx| {
                                    head_entity
                                        .update(cx, |this, cx| this.toggle_sort(side, key, cx));
                                })
                                .into_any_element()
                        }
                        None => cell.into_any_element(),
                    }
                }))
        };

        // 列表（虚拟滚动）：20000 个条目也只画可见的那一段。
        let count = pane.rows.len();
        let empty_text = pane.rows.is_empty().then(|| {
            if pane.loading && pane.raw.is_empty() {
                t(SftpViewMsg::ReadingDir)
            } else if !pane.filter.text.trim().is_empty() {
                t(SftpViewMsg::NoMatches)
            } else if is_remote && self.phase != Phase::Ready {
                t(SftpViewMsg::NotConnected)
            } else {
                t(SftpViewMsg::EmptyDir)
            }
        });
        let list_columns = columns.clone();
        let row_tokens = *tokens;
        let rows_el = uniform_list(
            SharedString::from(format!("sftp-rows-{id_suffix}")),
            count,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                this.render_rows(
                    side,
                    range,
                    &list_columns,
                    &row_tokens,
                    now_year,
                    offset,
                    cx,
                )
            }),
        )
        .track_scroll(&self.pane(side).scroll)
        .size_full();

        let key_entity = entity.clone();
        let menu_entity = entity.clone();
        let blank_entity = entity.clone();
        let focus_entity = entity.clone();
        let list = div()
            .id(SharedString::from(format!("sftp-list-{id_suffix}")))
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&self.pane(side).focus)
            .on_key_down(move |event, window, cx| {
                let handled =
                    key_entity.update(cx, |this, cx| this.key_down(side, event, window, cx));
                if handled {
                    cx.stop_propagation();
                }
            })
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                focus_entity.update(cx, |this, cx| this.focus_pane(side, window, cx));
            })
            .on_mouse_down(MouseButton::Right, move |_, window, cx| {
                blank_entity.update(cx, |this, cx| this.blank_right_down(side, window, cx));
            })
            .child(match empty_text {
                Some(text) => div()
                    .p_4()
                    .typo(Role::Body, tokens)
                    .text_color(tokens.text_muted)
                    .child(text)
                    .into_any_element(),
                None => rows_el.into_any_element(),
            })
            .context_menu(move |menu, window, cx| {
                super::view_dialogs::file_menu(menu, side, &menu_entity, window, cx)
            });

        let selected_count = pane.selection.paths.len();
        let footer = h_flex()
            .h(px(24.0))
            .flex_none()
            .px_2()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(tokens.border)
            .typo(Role::Caption, tokens)
            .child(format!("{count}{}", t(SftpMsg::ItemCountSuffix)))
            .when(selected_count > 0, |bar| {
                bar.child(format!(
                    "{}{selected_count}{}",
                    t(SftpViewMsg::SelectedPrefix),
                    t(SftpMsg::ItemCountSuffix)
                ))
            });

        let drop_entity = entity.clone();
        v_flex()
            .id(SharedString::from(format!("sftp-pane-{id_suffix}")))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .rounded_md()
            .border_1()
            .border_color(if focused {
                tokens.accent
            } else {
                tokens.border
            })
            // 量这一栏的宽度。
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let width = f32::from(bounds.size.width);
                        let before = measure.get();
                        measure.set(width);
                        if (before - width).abs() > 1.0 {
                            cx.notify(entity_id);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(toolbar)
            .child(header)
            .child(list)
            .child(footer)
            // 另一栏拖来的条目：传到这一栏的当前目录（落在目录行上的由行自己接）。
            .drag_over::<SftpDrag>({
                let accent = tokens.accent;
                let view_id = self.view_id;
                move |style, drag, _, _| {
                    if drag.view_id == view_id && drag.side != side {
                        style.border_color(accent)
                    } else {
                        style
                    }
                }
            })
            .on_drop::<SftpDrag>(cx.listener(move |this, drag: &SftpDrag, _window, cx| {
                this.drop_pane_drag(side, None, drag, cx);
            }))
            // 从资源管理器把文件 / 目录拖进来就上传到右边当前目录；拖到本地栏什么也不做。
            .when(is_remote, |pane| {
                pane.on_drop(move |paths: &ExternalPaths, _window, cx| {
                    drop_entity.update(cx, |this, cx| this.handle_drop(paths, None, cx));
                })
            })
            .into_any_element()
    }

    /// 路径栏：面包屑，点空白处（或 Ctrl+L）变成输入框。
    fn render_path_bar(
        &self,
        side: Side,
        tokens: &Tokens,
        pane_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let pane = self.pane(side);
        let is_remote = side == Side::Remote;
        let id_suffix = if is_remote { "remote" } else { "local" };

        if let Some(edit) = &pane.path_edit {
            let esc_entity = entity.clone();
            return div()
                .flex_1()
                .min_w_0()
                .on_key_down(move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        esc_entity
                            .update(cx, |this, cx| this.finish_path_edit(side, None, window, cx));
                        cx.stop_propagation();
                    }
                })
                .child(Input::new(&edit.input))
                .into_any_element();
        }

        let crumbs = if is_remote {
            remote_crumbs(&pane.path)
        } else {
            local_crumbs(&pane.path)
        };
        // 面包屑放得下的宽度 ≈ 栏宽减去其它按钮（后退 前进 上级 刷新 隐藏 过滤）占的位置。
        // 量到了栏宽但剩得很少（窄栏）也要折叠成「… › 最后一级」，不能当成"还没量到"整串显示：
        // 那样溢出部分被裁掉，只剩第一级（C:、srv），反而是最没用的那段。
        let available = if pane_width > 0.0 {
            // 按钮（后退 前进 上级 刷新 隐藏，停靠面板多一个关闭）约 200，再减过滤那一格的实际宽度。
            let collapsed =
                filter_collapsed(self.dock, pane_width, pane.filter_open, &pane.filter.text);
            let buttons = if self.dock {
                216.0
            } else if filter_compact(self.dock, pane_width) {
                // 窄栏不放「前进」。
                168.0
            } else {
                200.0
            };
            (pane_width - buttons - filter_slot(self.dock, pane_width, collapsed)).max(1.0)
        } else {
            0.0
        };
        let (hidden, shown) = collapse_crumbs(&crumbs, available);
        let start_entity = entity.clone();
        let hover = tokens.hover;
        let mut bar = h_flex()
            .id(SharedString::from(format!("sftp-crumbs-{id_suffix}")))
            .flex_1()
            .min_w_0()
            .h(px(30.0))
            .px_0p5()
            .gap_0p5()
            .items_center()
            .overflow_hidden()
            .rounded_sm()
            .border_1()
            .border_color(tokens.background)
            .hover(move |style| style.border_color(hover))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                start_entity.update(cx, |this, cx| this.begin_path_edit(side, window, cx));
            });
        let crumb_tokens = *tokens;
        let crumb_button =
            |id: SharedString, label: String, target: String, entity: Entity<SftpDemo>| {
                div()
                    .id(id)
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .truncate()
                    .typo(Role::Body, &crumb_tokens)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(label)
                    // 点某一级目录：去那里，不进入路径输入。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.browse(side, &target, cx));
                    })
            };
        if crumbs.is_empty() {
            let root = if is_remote {
                "/"
            } else {
                t(SftpViewMsg::ThisComputer)
            };
            let target = if is_remote { "/" } else { "" };
            bar = bar.child(crumb_button(
                SharedString::from(format!("crumb-root-{id_suffix}")),
                root.to_owned(),
                target.to_owned(),
                entity.clone(),
            ));
        }
        if !hidden.is_empty() {
            let ellipsis_entity = entity.clone();
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("crumb-ellipsis-{id_suffix}")))
                    .px_1p5()
                    .text_color(tokens.text_muted)
                    .child("…")
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        ellipsis_entity
                            .update(cx, |this, cx| this.begin_path_edit(side, window, cx));
                    }),
            );
        }
        for (index, crumb) in shown.iter().enumerate() {
            if index > 0 || !hidden.is_empty() {
                bar = bar.child(
                    Icon::new(IconName::ChevronRight)
                        .size_2p5()
                        .text_color(tokens.text_muted),
                );
            }
            let button = crumb_button(
                SharedString::from(format!("crumb-{id_suffix}-{}", crumb.path)),
                crumb.name.clone(),
                crumb.path.clone(),
                entity.clone(),
            );
            // 前面几级不压缩，放不下的由前面的折叠成“…”；最后一级（当前目录）名字太长时自己截断。
            bar = if index + 1 == shown.len() {
                bar.child(button.min_w_0().flex_shrink_1())
            } else {
                bar.child(button.flex_none())
            };
        }
        bar.into_any_element()
    }

    /// 列表里可见的那一段行。
    #[allow(clippy::too_many_arguments)]
    fn render_rows(
        &mut self,
        side: Side,
        range: Range<usize>,
        columns: &[Column],
        tokens: &Tokens,
        now_year: i64,
        offset: i64,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let pane = self.pane(side);
        let focused_pane = self.focused_side == side;
        let id_name = match side {
            Side::Remote => "sftp-row-remote",
            Side::Local => "sftp-row-local",
        };
        let id_name_cell = match side {
            Side::Remote => "sftp-name-remote",
            Side::Local => "sftp-name-local",
        };
        let hover = tokens.hover;
        let rows: Vec<(usize, Entry, bool, bool)> = range
            .filter_map(|index| {
                let entry = pane.rows.get(index)?.clone();
                let selected = pane.selection.contains(&entry);
                let focused =
                    focused_pane && pane.selection.focused.as_deref() == Some(&entry.path);
                Some((index, entry, selected, focused))
            })
            .collect();
        rows.into_iter()
            .map(|(index, entry, selected, focused)| {
                let operable = entry.name_is_utf8;
                // 链接（V2-U1 第 9 条）：图标按目标类型画，左下角叠快捷方式箭头；目标路径只放进悬停提示。
                let link = entry.link_info();
                let icon = match (&link, entry.kind) {
                    (Some(info), _) => link_icon(info, tokens).into_any_element(),
                    (None, EntryKind::Dir) => {
                        crate::icons::folder_icon(false, 16.0).into_any_element()
                    }
                    (None, EntryKind::Drive) => Icon::new(IconName::HardDrive)
                        .size_4()
                        .text_color(rgb(FOLDER_COLOR))
                        .into_any_element(),
                    _ => Icon::new(IconName::File)
                        .size_4()
                        .text_color(tokens.text_muted)
                        .into_any_element(),
                };
                let link_tip: Option<SharedString> =
                    link.as_ref().map(|info| info.tooltip().into());
                let name_cell = h_flex()
                    .id((id_name_cell, index))
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().truncate().child(entry.name.clone()))
                    .when_some(link_tip, |cell, tip| {
                        cell.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    })
                    .children((!operable).then(|| {
                        div()
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .child(t(SftpViewMsg::NotUtf8Badge))
                    }));
                let cells = columns
                    .iter()
                    .copied()
                    .filter(|c| *c != Column::Name)
                    .map(|column| {
                        div()
                            .w(px(column.width().unwrap_or(64.0)))
                            .flex_none()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .when(column.right_aligned(), |d| d.text_right())
                            .child(cell_text(column, &entry, now_year, offset))
                            .into_any_element()
                    });
                let down_entity = cx.entity();
                let right_entity = cx.entity();
                let up_entity = cx.entity();
                // 拖动：条目取松手时的选择（按下时没选中的会先被换成它）；落在目录行上就传进那个目录。
                let drag_payload = self.drag_payload(cx, side, &entry.path);
                let view_id = self.view_id;
                let drop_dir = matches!(entry.kind, EntryKind::Dir | EntryKind::Drive)
                    .then(|| entry.path.clone());
                let drop_external_dir = (side == Side::Remote && entry.kind == EntryKind::Dir)
                    .then(|| entry.path.clone());
                let drag_over_bg = tokens.accent_soft;
                h_flex()
                    .id((id_name, index))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex_none()
                    .px_2()
                    .gap_2()
                    .items_center()
                    .typo(Role::Body, tokens)
                    .cursor_default()
                    .when(selected, |d| d.bg(tokens.accent_soft))
                    .when(!selected, |d| d.hover(move |style| style.bg(hover)))
                    .when(focused, |d| d.border_1().border_color(tokens.accent))
                    .when(!operable, |d| d.text_color(tokens.text_muted))
                    .child(div().w(px(16.0)).flex_none().child(icon))
                    .child(name_cell)
                    .children(cells)
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, cx| {
                            down_entity.update(cx, |this, cx| {
                                this.row_mouse_down(
                                    side,
                                    index,
                                    event.modifiers,
                                    event.click_count,
                                    window,
                                    cx,
                                )
                            });
                        },
                    )
                    .on_mouse_down(MouseButton::Right, move |_: &MouseDownEvent, window, cx| {
                        right_entity
                            .update(cx, |this, cx| this.row_right_down(side, index, window, cx));
                    })
                    .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                        // 拖动结束的那次松手不是点击。
                        if !cx.has_active_drag() {
                            up_entity.update(cx, |this, cx| this.row_mouse_up(side, index, cx));
                        }
                    })
                    .when(operable, |row| {
                        row.on_drag(drag_payload, move |drag, _offset, _window, cx| {
                            let label = drag
                                .view
                                .upgrade()
                                .map(|view| {
                                    view.update(cx, |this, _| {
                                        this.pane_mut(drag.side).pending_collapse = None;
                                        this.drag_label(drag.side, &drag.anchor)
                                    })
                                })
                                .unwrap_or_default();
                            cx.new(|_| SftpDragChip::new(label))
                        })
                    })
                    .when_some(drop_dir, |row, path| {
                        row.drag_over::<SftpDrag>(move |style, drag, _, _| {
                            if drag.view_id == view_id && drag.side != side {
                                style.bg(drag_over_bg)
                            } else {
                                style
                            }
                        })
                        .on_drop::<SftpDrag>(cx.listener(
                            move |this, drag: &SftpDrag, _window, cx| {
                                if drag.view_id == this.view_id && drag.side != side {
                                    cx.stop_propagation();
                                    this.drop_pane_drag(side, Some(path.clone()), drag, cx);
                                }
                            },
                        ))
                    })
                    .when_some(drop_external_dir, |row, path| {
                        row.on_drop::<ExternalPaths>(cx.listener(
                            move |this, paths: &ExternalPaths, _window, cx| {
                                cx.stop_propagation();
                                this.drop_external(paths, Some(path.clone()), cx);
                            },
                        ))
                    })
                    .into_any_element()
            })
            .collect()
    }

    // ---- 条上的提示 ----

    /// 服务器上正在压缩 / 检查 / 解压：一条带取消的提示。
    fn render_busy_bar(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let busy = self.busy.as_ref()?;
        let entity = cx.entity();
        let elapsed = busy.started.elapsed().as_secs();
        let text = match busy.kind {
            super::archive_ui::BusyKind::Paste => {
                super::clip::copying_text(busy.local, elapsed, busy.files)
            }
            kind => super::archive_ui::busy_text(kind, elapsed, busy.bytes),
        };
        // 本地复制只是把任务排进队列，没有"服务器上"的东西可取消（要停就在队列里取消）。
        let cancellable = !busy.local;
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .typo(Role::Body, tokens)
                        .child(text),
                )
                .when(cancellable, |bar| {
                    bar.child(
                        Button::new("sftp-busy-cancel")
                            .label(if busy.cancelling {
                                t(SftpViewMsg::Cancelling)
                            } else {
                                t(SftpViewMsg::Cancel)
                            })
                            .compact()
                            .disabled(busy.cancelling)
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |this, cx| this.cancel_busy(cx));
                            }),
                    )
                })
                .into_any_element(),
        )
    }

    /// `cp` 跑了但失败：显示原因，点按钮才改经本机中转（不自动换办法）。
    fn render_paste_retry_bar(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let retry = self.paste_retry.as_ref()?;
        let relay_entity = cx.entity();
        let close_entity = cx.entity();
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .typo(Role::Body, tokens)
                        .text_color(tokens.danger)
                        .child(retry.text.clone()),
                )
                .child(
                    Button::new("sftp-paste-relay")
                        .label(t(SftpViewMsg::RelayRetry))
                        .compact()
                        .on_click(move |_, _, cx| {
                            relay_entity.update(cx, |this, cx| this.retry_paste_via_relay(cx));
                        }),
                )
                .child(
                    Button::new("sftp-paste-retry-close")
                        .label(t(SftpViewMsg::Close))
                        .compact()
                        .on_click(move |_, _, cx| {
                            close_entity.update(cx, |this, cx| this.dismiss_paste_retry(cx));
                        }),
                )
                .into_any_element(),
        )
    }

    /// 递归删除进行中：进度和“取消删除”。
    fn render_delete_bar(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let job = self.deleting.as_ref()?;
        let entity = cx.entity();
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .border_t_1()
                .border_color(tokens.border)
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .typo(Role::Body, tokens)
                        .child(format!(
                            "{}{} {}{}{}{}",
                            if job.cancelling {
                                t(SftpMsg::CancelDeleting)
                            } else {
                                t(SftpMsg::Deleting)
                            },
                            job.name,
                            t(SftpMsg::DeleteProgressMiddle),
                            job.deleted,
                            t(SftpMsg::DeleteProgressSuffix),
                            if job.current.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    "{}{}{}",
                                    t(SftpMsg::ParenOpen),
                                    file_name(&job.current),
                                    t(SftpMsg::ParenClose)
                                )
                            }
                        )),
                )
                .child(
                    Button::new("sftp-delete-cancel")
                        .label(t(SftpViewMsg::CancelDelete))
                        .compact()
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.cancel_delete(cx));
                        }),
                )
                .into_any_element(),
        )
    }

    // ---- 同名文件面板 ----

    /// 引擎在等回答：放在传输队列上面，不挡文件列表。
    fn render_conflict_panel(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.conflicts.front()?;
        let entity = cx.entity();
        let target_name = file_name(&prompt.info.target);
        let source = prompt.info.source.clone();
        let target = prompt.info.target.clone();
        let waiting = self.conflicts.len().saturating_sub(1);
        let can_resume_here = prompt.info.can_resume;
        let (source_line, target_line, hint_line) = conflict_compare(&prompt.info);
        let action = |id: &'static str, label: &'static str, action: ConflictAction, all: bool| {
            let entity = entity.clone();
            Button::new(id)
                .label(label)
                .compact()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        if all {
                            this.answer_all_conflicts(action, window, cx);
                        } else {
                            this.answer_conflict(action, window, cx);
                        }
                    });
                })
        };
        Some(
            v_flex()
                .mx_2()
                .mb_2()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .child(
                    h_flex()
                        .h(px(28.0))
                        .px_2()
                        .gap_2()
                        .items_center()
                        .child(div().typo(Role::ItemName, tokens).child(format!(
                            "{}{target_name}",
                            t(SftpViewMsg::ConflictTitlePrefix)
                        )))
                        .when(waiting > 0, |line| {
                            line.child(div().typo(Role::Caption, tokens).child(format!(
                                "{}{waiting}{}",
                                t(SftpViewMsg::ConflictWaitingPrefix),
                                t(SftpViewMsg::ConflictWaitingSuffix)
                            )))
                        })
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .typo(Role::Caption, tokens)
                                .child(format!("{}{source}", t(SftpViewMsg::SourcePrefix))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .typo(Role::Caption, tokens)
                                .child(format!("{}{target}", t(SftpViewMsg::TargetPrefix))),
                        ),
                )
                .child(
                    h_flex()
                        .px_2()
                        .pb_1()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .typo(Role::Caption, tokens)
                                .text_color(tokens.text)
                                .child(source_line),
                        )
                        .child(
                            div()
                                .typo(Role::Caption, tokens)
                                .text_color(tokens.text)
                                .child(target_line),
                        )
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .typo(Role::Caption, tokens)
                                .child(hint_line),
                        ),
                )
                .child(
                    h_flex()
                        .px_2()
                        .pb_2()
                        .gap_2()
                        .items_center()
                        .flex_wrap()
                        .child(
                            div()
                                .w(px(180.0))
                                .child(Input::new(&self.conflict_name_input)),
                        )
                        .child(action(
                            "sftp-conflict-overwrite",
                            t(SftpViewMsg::Overwrite),
                            ConflictAction::Overwrite,
                            false,
                        ))
                        .child(action(
                            "sftp-conflict-skip",
                            t(SftpViewMsg::Skip),
                            ConflictAction::Skip,
                            false,
                        ))
                        .when(can_resume_here, |row| {
                            row.child(action(
                                "sftp-conflict-resume",
                                t(SftpViewMsg::Resume),
                                ConflictAction::Resume,
                                false,
                            ))
                        })
                        .child(action(
                            "sftp-conflict-rename",
                            t(SftpViewMsg::Rename),
                            ConflictAction::Rename,
                            false,
                        ))
                        .child(action(
                            "sftp-conflict-overwrite-all",
                            t(SftpViewMsg::OverwriteAll),
                            ConflictAction::Overwrite,
                            true,
                        ))
                        .child(action(
                            "sftp-conflict-skip-all",
                            t(SftpViewMsg::SkipAll),
                            ConflictAction::Skip,
                            true,
                        )),
                )
                .into_any_element(),
        )
    }

    // ---- 传输队列 ----

    fn render_queue(&self, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let dock = self.dock;
        let facts: Vec<queue::RowFacts<'_>> = self.transfers.iter().map(|r| r.facts()).collect();
        let summary = queue::queue_summary(&facts);
        let total = self.transfers.len();
        let toggle_entity = entity.clone();
        let clear_entity = entity.clone();
        let hover = tokens.hover;
        let header = h_flex()
            .id("sftp-queue-toggle")
            .h(px(30.0))
            .mx_2()
            .px_3()
            .gap_2()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .typo(Role::FieldLabel, tokens)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .child(Icon::new(Ico::FolderTree).size_3p5())
            .child(t(SftpViewMsg::QueueTitle))
            .children(summary.as_ref().map(|(count, _, text)| {
                div()
                    .px_1p5()
                    .rounded_full()
                    .bg(tokens.accent)
                    .typo(Role::Caption, tokens)
                    .text_color(tokens.on_accent)
                    .child(if dock {
                        text.clone()
                    } else {
                        format!("{count}{}", t(SftpViewMsg::ActiveSuffix))
                    })
            }))
            .child(div().flex_1())
            .child(div().typo(Role::Caption, tokens).child(if self.queue_open {
                t(SftpViewMsg::Collapse).to_owned()
            } else {
                format!(
                    "{}{total}{}",
                    t(SftpViewMsg::QueueTotalPrefix),
                    t(SftpViewMsg::QueueTotalSuffix)
                )
            }))
            .on_click(move |_, _, cx| {
                toggle_entity.update(cx, |this, cx| {
                    this.queue_open = !this.queue_open;
                    cx.notify();
                });
            });

        let body = self.queue_open.then(|| {
            let rows: Vec<AnyElement> = self
                .transfers
                .iter()
                .enumerate()
                .map(|(index, row)| self.render_transfer_row(index, row, tokens, cx))
                .collect();
            v_flex()
                .mx_2()
                .mt_1()
                .max_h(px(224.0))
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .child(if rows.is_empty() {
                    div()
                        .px_3()
                        .py_3()
                        .typo(Role::Body, tokens)
                        .text_color(tokens.text_muted)
                        .child(t(SftpViewMsg::QueueEmpty))
                        .into_any_element()
                } else {
                    v_flex()
                        .min_h_0()
                        .child(
                            v_flex()
                                .id("sftp-transfers")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .children(rows),
                        )
                        .child(
                            h_flex()
                                .flex_none()
                                .justify_end()
                                .px_2()
                                .py_1()
                                .border_t_1()
                                .border_color(tokens.border)
                                .child(
                                    Button::new("sftp-clear-finished")
                                        .ghost()
                                        .compact()
                                        .icon(Icon::new(Ico::ListRestart))
                                        .label(t(SftpViewMsg::ClearFinished))
                                        .on_click(move |_, _, cx| {
                                            clear_entity
                                                .update(cx, |this, cx| this.clear_finished(cx));
                                        }),
                                ),
                        )
                        .into_any_element()
                })
        });
        v_flex()
            .flex_none()
            .py_1()
            .child(header)
            .children(body)
            .into_any_element()
    }

    /// 队列里的一行：方向、名称、（宽版）来源 → 目标、进度、（宽版）速度和剩余时间、状态、操作按钮、
    /// 进度条；失败显示原因，说明和完成后的对账另起几行。
    fn render_transfer_row(
        &self,
        index: usize,
        row: &super::TransferRow,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let compact = self.dock;
        let id = row.id;
        let state = row.state.clone();
        let live = !state.is_terminal();
        let is_dir = row.total_files > 1;
        let name_source = if is_dir {
            row.source.clone()
        } else {
            row.current.clone().unwrap_or_else(|| row.source.clone())
        };
        let name = {
            let cut = name_source.rfind(['/', '\\']).map_or(0, |i| i + 1);
            name_source[cut..].to_owned()
        };
        let percent = queue::row_percent(&state, row.done_bytes, row.total_bytes);
        let icon = match row.direction {
            Direction::Upload => Icon::new(IconName::ArrowUp)
                .size_4()
                .text_color(rgb(FOLDER_COLOR)),
            Direction::Download => Icon::new(IconName::ArrowDown)
                .size_3p5()
                .text_color(rgb(LINK_COLOR)),
            _ => Icon::new(IconName::Copy)
                .size_3p5()
                .text_color(tokens.caution),
        };
        let button = |suffix: &'static str, icon: Icon, tip: &'static str| {
            Button::new((suffix, index))
                .ghost()
                .compact()
                .icon(icon)
                .tooltip(tip)
        };
        let mut buttons = h_flex().w(px(112.0)).flex_none().gap_1().justify_end();
        if super::menu::transfer_can_pause(&state) {
            let e = entity.clone();
            buttons = buttons.child(
                button(
                    "xfer-pause",
                    Icon::new(IconName::Pause),
                    t(SftpViewMsg::Pause),
                )
                .on_click(move |_, _, cx| e.update(cx, |this, cx| this.pause_transfer(id, cx))),
            );
        }
        if state == TransferState::Paused {
            let e = entity.clone();
            buttons = buttons.child(
                button(
                    "xfer-resume",
                    Icon::new(IconName::Play),
                    t(SftpViewMsg::ResumeTransfer),
                )
                .on_click(move |_, _, cx| e.update(cx, |this, cx| this.resume_transfer(id, cx))),
            );
        }
        if super::menu::transfer_can_retry(&state) {
            let e = entity.clone();
            buttons = buttons.child(
                button(
                    "xfer-retry",
                    Icon::new(Ico::RotateCcw),
                    t(SftpViewMsg::Retry),
                )
                .on_click(move |_, _, cx| e.update(cx, |this, cx| this.retry_transfer(id, cx))),
            );
        }
        if live {
            let e = entity.clone();
            buttons = buttons.child(
                button(
                    "xfer-cancel",
                    Icon::new(IconName::Close),
                    t(SftpViewMsg::CancelTransfer),
                )
                .on_click(move |_, _, cx| e.update(cx, |this, cx| this.cancel_transfer(id, cx))),
            );
        }
        let state_color = match &state {
            TransferState::Failed(_) => tokens.danger,
            TransferState::Done => tokens.dot_connected,
            _ => tokens.text_muted,
        };
        let state_text = match &state {
            // 失败原因另起一行完整显示，状态列只写“失败”。
            TransferState::Failed(_) => t(SftpViewMsg::Failed).to_owned(),
            other => state_label(other),
        };
        let facts = row.facts();
        let reconcile = queue::reconcile_summary(&facts, row.elapsed());
        let mismatch = queue::reconcile_is_mismatch(&facts);
        let menu_entity = entity.clone();
        let menu_state = state.clone();
        let sizes = format!(
            "{} / {}",
            super::props::size_long(row.done_bytes),
            super::props::size_long(row.total_bytes)
        );
        v_flex()
            .id(("sftp-xfer", index))
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(tokens.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .typo(Role::Body, tokens)
                    .child(icon)
                    .child(
                        h_flex()
                            .gap_1()
                            .items_baseline()
                            .overflow_hidden()
                            .when(compact, |d| d.min_w_0().flex_1())
                            .when(!compact, |d| d.w(px(176.0)).flex_none())
                            .child(div().truncate().min_w_0().child(name))
                            .children(is_dir.then(|| {
                                div()
                                    .flex_none()
                                    .typo(Role::Caption, tokens)
                                    .child(format!("（{}/{}）", row.done_files, row.total_files))
                            })),
                    )
                    .when(!compact, |line| {
                        line.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .typo(Role::Caption, tokens)
                                .child(format!("{} → {}", row.source, row.target)),
                        )
                    })
                    .child(
                        div()
                            .when(compact, |d| d.ml_auto())
                            .when(!compact, |d| d.w(px(128.0)))
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .text_color(tokens.text)
                            .text_right()
                            .child(sizes),
                    )
                    .when(!compact, |line| {
                        line.child(
                            div()
                                .w(px(80.0))
                                .flex_none()
                                .typo(Role::Caption, tokens)
                                .text_right()
                                .child(if live {
                                    queue::speed_text(row.speed_bps)
                                } else {
                                    String::new()
                                }),
                        )
                        .child(
                            div()
                                .w(px(80.0))
                                .flex_none()
                                .typo(Role::Caption, tokens)
                                .text_right()
                                .child(if state == TransferState::Running {
                                    queue::eta_text(row.eta_secs)
                                } else {
                                    String::new()
                                }),
                        )
                    })
                    .child(
                        div()
                            .w(px(if compact { 84.0 } else { 108.0 }))
                            .flex_none()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .text_right()
                            .text_color(state_color)
                            .child(state_text),
                    )
                    .child(buttons),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Progress::new(("xfer-progress", index))
                                .value(percent)
                                .color(if matches!(state, TransferState::Failed(_)) {
                                    tokens.danger
                                } else {
                                    tokens.accent
                                }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(40.0))
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .text_right()
                            .child(format!("{percent:.0}%")),
                    ),
            )
            .children(match &state {
                TransferState::Failed(reason) => Some(
                    div()
                        .typo(Role::Caption, tokens)
                        .text_color(tokens.danger)
                        .child(reason.clone()),
                ),
                _ => None,
            })
            .children(reconcile.map(|text| {
                div()
                    .typo(Role::Caption, tokens)
                    .text_color(if mismatch {
                        tokens.danger
                    } else {
                        tokens.text_muted
                    })
                    .child(text)
            }))
            .children((!row.notes.is_empty()).then(|| {
                v_flex().children(
                    row.notes
                        .iter()
                        .map(|note| div().typo(Role::Caption, tokens).child(format!("· {note}"))),
                )
            }))
            .context_menu(move |menu, window, cx| {
                super::view_dialogs::transfer_menu(menu, &menu_state, id, &menu_entity, window, cx)
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod list_size_tests {
    use crate::typography::Role;

    #[test]
    fn the_filter_box_gives_way_in_narrow_panes() {
        assert_eq!(super::filter_width(true, 600.0), 84.0);
        assert_eq!(super::filter_width(false, 0.0), 120.0);
        assert_eq!(super::filter_width(false, 540.0), 120.0);
        assert_eq!(super::filter_width(false, 450.0), 96.0);
        assert_eq!(super::filter_width(false, 316.0), 72.0);
        // 窄栏（独立文件窗口最窄时两栏各约 316）：过滤框收成按钮，点开或有文字时才是输入框。
        assert!(super::filter_compact(false, 316.0));
        assert!(!super::filter_compact(false, 450.0));
        assert!(!super::filter_compact(true, 316.0), "停靠面板不收");
        assert!(!super::filter_compact(false, 0.0), "还没量到按宽版");
        assert!(super::filter_collapsed(false, 316.0, false, ""));
        assert!(!super::filter_collapsed(false, 316.0, true, ""));
        assert!(!super::filter_collapsed(false, 316.0, false, "log"));
        assert_eq!(super::filter_slot(false, 316.0, true), 30.0);
        assert_eq!(super::filter_slot(false, 316.0, false), 120.0);
    }

    #[test]
    fn file_rows_are_roomy_enough_for_14px_names() {
        // V2-U14：文件名走 Body（14px 主文字色），行高 32–34px，次要列走 Caption 但不低于 12px。
        assert!((32.0..=34.0).contains(&super::ROW_HEIGHT));
        assert_eq!(Role::Body.spec().size, 14.0);
        assert!(Role::Caption.spec().size >= 12.0);
        assert!(super::ROW_HEIGHT >= Role::Body.spec().size * Role::Body.spec().leading);
    }
}
