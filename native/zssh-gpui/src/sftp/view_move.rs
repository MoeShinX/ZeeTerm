//! 「移动到…」三个对话框的画法（V2-U23）：选目标文件夹（路径输入框 + 目录浏览列表）、
//! 同名询问、结果汇总。状态和动作在 `dialogs.rs` / `move_flow.rs`，规则在 `move_to.rs`。

use ::gpui::{MouseButton, MouseDownEvent, SharedString, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::SftpDemo;
use super::dialogs::{MoveConflict, MoveDialog, MoveReport};
use super::menu::Side;
use super::model::{Crumb, local_at_top, local_crumbs, remote_crumbs};
use super::move_to::{Choice, check_destination, dialog_title as move_title};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::main_window::widgets::{ButtonLook, error_note, note, text_button};
use crate::typography::{Role, Typo as _};

/// 本文件（移动对话框画面）自己的文案。
#[derive(Clone, Copy, Debug)]
enum SftpMoveViewMsg {
    ThisComputer,
    UpLevel,
    NewFolder,
    Reading,
    NoSubfolders,
    Create,
    Cancel,
    TargetFolder,
    Checking,
    Move,
    CancelMove,
    Skip,
    Replace,
    ConflictTitle,
    ExistsPrefix,
    ExistsMiddle,
    ApplyRestPrefix,
    ApplyRestSuffix,
    SummaryTitle,
    MorePrefix,
    MoreSuffix,
    GotIt,
}

impl Msg for SftpMoveViewMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::ThisComputer => "此电脑",
            Self::UpLevel => "上一级",
            Self::NewFolder => "新建文件夹",
            Self::Reading => "正在读取…",
            Self::NoSubfolders => "这里面没有子文件夹",
            Self::Create => "创建",
            Self::Cancel => "取消",
            Self::TargetFolder => "目标文件夹",
            Self::Checking => "检查中…",
            Self::Move => "移动",
            Self::CancelMove => "取消移动",
            Self::Skip => "跳过",
            Self::Replace => "替换",
            Self::ConflictTitle => "目标里已有同名项",
            Self::ExistsPrefix => "「",
            Self::ExistsMiddle => "」在目标文件夹里已经存在。",
            Self::ApplyRestPrefix => "对其余同名项都这样做（后面还有 ",
            Self::ApplyRestSuffix => " 项）",
            Self::SummaryTitle => "移动结果",
            Self::MorePrefix => "…等另外 ",
            Self::MoreSuffix => " 项",
            Self::GotIt => "知道了",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::ThisComputer => "This PC",
            Self::UpLevel => "Up One Level",
            Self::NewFolder => "New Folder",
            Self::Reading => "Reading…",
            Self::NoSubfolders => "No subfolders here",
            Self::Create => "Create",
            Self::Cancel => "Cancel",
            Self::TargetFolder => "Target folder",
            Self::Checking => "Checking…",
            Self::Move => "Move",
            Self::CancelMove => "Cancel Move",
            Self::Skip => "Skip",
            Self::Replace => "Replace",
            Self::ConflictTitle => "The target already contains an item with the same name",
            Self::ExistsPrefix => "\"",
            Self::ExistsMiddle => "\" already exists in the target folder.",
            Self::ApplyRestPrefix => "Do the same for the remaining conflicts (",
            Self::ApplyRestSuffix => " more)",
            Self::SummaryTitle => "Move Results",
            Self::MorePrefix => "…and ",
            Self::MoreSuffix => " more",
            Self::GotIt => "Got It",
        }
    }
}

/// 面包屑最多显示几级（再长的前面折成 …）。
const MAX_CRUMBS: usize = 3;
const FOLDER_LIST_HEIGHT: f32 = 208.0;
const FOLDER_ROW_HEIGHT: f32 = 30.0;

fn title(text: impl Into<SharedString>) -> Div {
    div()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

fn button_row(children: Vec<AnyElement>) -> Div {
    h_flex().gap_2().justify_end().children(children)
}

/// 浏览目录的面包屑：远端从 `/` 起，本地在 Windows 上从“此电脑”起、在 Linux 上也从 `/` 起
/// （Linux 的本地面包屑自己就以 `/` 打头，不再加一级）。
fn browse_crumbs(side: Side, dir: &str) -> Vec<Crumb> {
    let mut crumbs = match side {
        Side::Local if !cfg!(windows) => Vec::new(),
        Side::Remote => vec![Crumb {
            name: "/".to_owned(),
            path: "/".to_owned(),
        }],
        Side::Local => vec![Crumb {
            name: t(SftpMoveViewMsg::ThisComputer).to_owned(),
            path: String::new(),
        }],
    };
    crumbs.extend(match side {
        Side::Remote => remote_crumbs(dir),
        Side::Local => local_crumbs(dir),
    });
    crumbs
}

impl SftpDemo {
    // ---- 选目标文件夹 ----

    pub(super) fn move_body(
        &self,
        d: &MoveDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let text = d.input.read(cx).value().to_string();
        let check = check_destination(d.side, &d.entries, &text);
        let at_top = match d.side {
            Side::Remote => d.browse.dir.is_empty() || d.browse.dir == "/",
            Side::Local => local_at_top(&d.browse.dir),
        };

        // 面包屑 + 上一级。
        let crumbs = browse_crumbs(d.side, &d.browse.dir);
        let cut = crumbs.len().saturating_sub(MAX_CRUMBS);
        let hover = tokens.hover;
        let mut crumb_bar = h_flex()
            .flex_1()
            .min_w_0()
            .h(px(28.0))
            .gap_0p5()
            .items_center()
            .overflow_hidden();
        if cut > 0 {
            crumb_bar = crumb_bar.child(div().px_1p5().text_color(tokens.text_muted).child("…"));
        }
        for (index, crumb) in crumbs.iter().enumerate().skip(cut) {
            if index > cut || cut > 0 {
                crumb_bar = crumb_bar.child(
                    Icon::new(IconName::ChevronRight)
                        .size_2p5()
                        .text_color(tokens.text_muted),
                );
            }
            let (go, target) = (entity.clone(), crumb.path.clone());
            crumb_bar = crumb_bar.child(
                div()
                    .id(("sftp-move-crumb", index))
                    .flex_none()
                    .max_w(px(120.0))
                    .truncate()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .typo(Role::Body, tokens)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .child(crumb.name.clone())
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        let target = target.clone();
                        go.update(cx, |this, cx| this.move_browse_to(target, window, cx));
                    }),
            );
        }
        let up = entity.clone();
        let new_folder = entity.clone();
        let nav_bar = h_flex()
            .gap_1()
            .items_center()
            .child(
                Button::new("sftp-move-up")
                    .ghost()
                    .compact()
                    .icon(IconName::ArrowUp)
                    .tooltip(t(SftpMoveViewMsg::UpLevel))
                    .disabled(at_top)
                    .on_click(move |_, window, cx| {
                        up.update(cx, |this, cx| this.move_up(window, cx));
                    }),
            )
            .child(crumb_bar)
            .child(
                text_button(
                    "sftp-move-new-folder",
                    t(SftpMoveViewMsg::NewFolder),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        new_folder.update(cx, |this, cx| this.toggle_new_folder(window, cx));
                    },
                )
                .disabled(d.browse.dir.is_empty()),
            );

        // 目录浏览列表。
        let mut list = v_flex()
            .id("sftp-move-folders")
            .track_scroll(&d.scroll)
            .h(px(FOLDER_LIST_HEIGHT))
            .w_full()
            .p_1()
            .gap_0p5()
            .overflow_y_scroll()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .bg(tokens.surface);
        if d.browse.loading && d.browse.folders.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(tokens.text_muted)
                    .child(t(SftpMoveViewMsg::Reading)),
            );
        } else if let Some(err) = &d.browse.error {
            list = list.child(div().p_2().child(error_note(err.clone(), tokens.danger)));
        } else if d.browse.folders.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(tokens.text_muted)
                    .child(t(SftpMoveViewMsg::NoSubfolders)),
            );
        }
        let accent = tokens.accent_soft;
        for (index, folder) in d.browse.folders.iter().enumerate() {
            let selected = d.browse.selected.as_deref() == Some(folder.path.as_str());
            let click = entity.clone();
            let path = folder.path.clone();
            list = list.child(
                h_flex()
                    .id(("sftp-move-folder", index))
                    .h(px(FOLDER_ROW_HEIGHT))
                    .w_full()
                    .flex_none()
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded_sm()
                    .typo(Role::Body, tokens)
                    .cursor_default()
                    .when(selected, |row| row.bg(accent))
                    .when(!selected, |row| row.hover(move |style| style.bg(hover)))
                    .child(crate::icons::folder_icon(false, 16.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(folder.name.clone()),
                    )
                    // 单击选中（它成为目标），双击进入。
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, cx| {
                            let path = path.clone();
                            let double = event.click_count >= 2;
                            click.update(cx, |this, cx| {
                                if double {
                                    this.move_browse_to(path, window, cx);
                                } else {
                                    this.move_select_folder(path, window, cx);
                                }
                            });
                        },
                    ),
            );
        }

        // 新建文件夹的名字行。
        let new_row = d.new_folder.as_ref().map(|input| {
            let (create, close) = (entity.clone(), entity.clone());
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(Input::new(input)))
                        .child(
                            text_button(
                                "sftp-move-folder-create",
                                t(SftpMoveViewMsg::Create),
                                ButtonLook::Primary,
                                move |_, window, cx| {
                                    create
                                        .update(cx, |this, cx| this.confirm_new_folder(window, cx));
                                },
                            )
                            .disabled(d.creating),
                        )
                        .child(text_button(
                            "sftp-move-folder-close",
                            t(SftpMoveViewMsg::Cancel),
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                close.update(cx, |this, cx| this.toggle_new_folder(window, cx));
                            },
                        )),
                )
                .children(
                    d.new_folder_error
                        .as_ref()
                        .map(|err| error_note(err.clone(), tokens.danger)),
                )
        });

        let (cancel, ok) = (entity.clone(), entity.clone());
        let confirm_disabled = !check.is_ok() || d.confirming || d.creating;
        v_flex()
            .gap_3()
            .child(title(move_title(&d.entries)))
            .child(note(t(SftpMoveViewMsg::TargetFolder), tokens))
            .child(nav_bar)
            .child(Input::new(&d.input))
            .child(list)
            .children(new_row)
            .children(check.note().map(|text| error_note(text, tokens.caution)))
            .children(
                d.error
                    .as_ref()
                    .map(|err| error_note(err.clone(), tokens.danger)),
            )
            .child(button_row(vec![
                text_button(
                    "sftp-move-cancel",
                    t(SftpMoveViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-move-ok",
                    if d.confirming {
                        t(SftpMoveViewMsg::Checking)
                    } else {
                        t(SftpMoveViewMsg::Move)
                    },
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        ok.update(cx, |this, cx| this.confirm_move_to(window, cx));
                    },
                )
                .disabled(confirm_disabled)
                .into_any_element(),
            ]))
            .into_any_element()
    }

    // ---- 同名询问 ----

    pub(super) fn move_conflict_body(
        &self,
        c: &MoveConflict,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let (abort, skip, replace, toggle) = (
            entity.clone(),
            entity.clone(),
            entity.clone(),
            entity.clone(),
        );
        let ask = &c.ask;
        let detail = match &ask.why_not {
            Some(why) => why.clone(),
            None => t(super::SftpMsg::MoveTargetAlreadyExistsDetail).to_owned(),
        };
        let mut buttons = vec![
            text_button(
                "sftp-move-abort",
                t(SftpMoveViewMsg::CancelMove),
                ButtonLook::Outline,
                move |_, window, cx| {
                    abort.update(cx, |this, cx| this.abort_move(window, cx));
                },
            )
            .into_any_element(),
            text_button(
                "sftp-move-skip",
                t(SftpMoveViewMsg::Skip),
                if ask.can_replace {
                    ButtonLook::Outline
                } else {
                    ButtonLook::Primary
                },
                move |_, window, cx| {
                    skip.update(cx, |this, cx| {
                        this.answer_move_conflict(Choice::Skip, window, cx)
                    });
                },
            )
            .into_any_element(),
        ];
        if ask.can_replace {
            buttons.push(
                text_button(
                    "sftp-move-replace",
                    t(SftpMoveViewMsg::Replace),
                    ButtonLook::Danger,
                    move |_, window, cx| {
                        replace.update(cx, |this, cx| {
                            this.answer_move_conflict(Choice::Replace, window, cx)
                        });
                    },
                )
                .into_any_element(),
            );
        }
        v_flex()
            .gap_3()
            .child(title(t(SftpMoveViewMsg::ConflictTitle)))
            .child(note(
                format!(
                    "{}{}{}{detail}",
                    t(SftpMoveViewMsg::ExistsPrefix),
                    ask.name,
                    t(SftpMoveViewMsg::ExistsMiddle)
                ),
                tokens,
            ))
            .children((ask.remaining > 0).then(|| {
                Checkbox::new("sftp-move-apply-rest")
                    .checked(c.apply_rest)
                    .label(format!(
                        "{}{}{}",
                        t(SftpMoveViewMsg::ApplyRestPrefix),
                        ask.remaining,
                        t(SftpMoveViewMsg::ApplyRestSuffix)
                    ))
                    .on_click(move |checked, _, cx| {
                        let checked = *checked;
                        toggle.update(cx, |this, cx| this.set_move_apply_rest(checked, cx));
                    })
            }))
            .child(button_row(buttons))
            .into_any_element()
    }

    // ---- 结果汇总 ----

    pub(super) fn move_summary_body(
        &self,
        r: &MoveReport,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let more = r
            .lines
            .len()
            .saturating_sub(super::dialogs::DELETE_NAMES_SHOWN);
        let shown: Vec<String> = r
            .lines
            .iter()
            .take(super::dialogs::DELETE_NAMES_SHOWN)
            .cloned()
            .collect();
        v_flex()
            .gap_3()
            .child(title(t(SftpMoveViewMsg::SummaryTitle)))
            .child(note(r.summary.clone(), tokens))
            .child(
                v_flex()
                    .max_h(px(192.0))
                    .overflow_hidden()
                    .gap_0p5()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .text_xs()
                    .children(shown.into_iter().map(|line| div().child(line)))
                    .children((more > 0).then(|| {
                        div().text_color(tokens.text_muted).child(format!(
                            "{}{more}{}",
                            t(SftpMoveViewMsg::MorePrefix),
                            t(SftpMoveViewMsg::MoreSuffix)
                        ))
                    })),
            )
            .child(button_row(vec![
                text_button(
                    "sftp-move-summary-close",
                    t(SftpMoveViewMsg::GotIt),
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
            ]))
            .into_any_element()
    }
}
