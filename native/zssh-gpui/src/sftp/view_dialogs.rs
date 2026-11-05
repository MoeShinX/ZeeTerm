//! 文件视图的右键菜单（`PopupMenu`）和对话框的画法。
//!
//! 菜单里有什么、哪些置灰由 `menu::build_menu` 算好；这里只把它们画出来：每一项左边是名字，
//! 右边是快捷键，置灰的项右边写原因（悬停提示在菜单里不好用，直接写出来）。
//! 对话框盖在整个文件视图上（半透明遮罩 + 居中卡片），Esc 取消、Enter 确认；
//! 删除 / 覆盖这类不能撤销的不响应 Enter，必须点按钮。

use std::rc::Rc;

use ::gpui::{MouseButton, SharedString, Window, hsla, px};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::sftp::archive::ConflictPolicy;

use super::archive_ui::conflict_more_text;
use super::dialogs::{
    DeleteDialog, Dialog, DownloadDialog, ExtractAsk, NameCheck, NameDialog, NameKind,
    OverwriteAsk, PermDialog, PropDialog, delete_description, delete_names, delete_title,
    name_taken, validate_local_dir, validate_name,
};
use super::menu::{MenuNode, Side, build_menu, build_transfer_menu, submenu_icon};
use super::model::{Entry, EntryKind};
use super::props::{
    CalcPhase, PermBit, aggregate, bytes_text, calc_text, can_calculate, info_rows,
};
use super::{SftpDemo, SftpMsg, props};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::main_window::widgets::{ButtonLook, error_note, note, text_button};
use crate::menu_kit;

/// 本文件（对话框画面）自己的文案。
#[derive(Clone, Copy, Debug)]
enum SftpDialogViewMsg {
    NewFolder,
    NewFile,
    Rename,
    NameTakenRemoteWillAsk,
    NameTakenWillFail,
    Cancel,
    Ok,
    Delete,
    Owner,
    Group,
    Others,
    Read,
    Write,
    Execute,
    PermissionsTitle,
    Octal,
    OctalError,
    RecursiveLabel,
    Apply,
    PropertySuffix,
    PropertyManySuffix,
    Selected,
    SelectedSummaryMiddle,
    FolderCountMiddle,
    FileCountSuffix,
    FileSize,
    Contains,
    FolderTotal,
    EditPermissions,
    CancelCalc,
    CalcSize,
    Recalc,
    Close,
    DownloadTitle,
    DownloadSummaryPrefix,
    DownloadSummarySuffix,
    ChooseFolder,
    CheckingDir,
    Download,
    ExtractConflictTitle,
    ExtractConflictPrefix,
    ExtractConflictMiddle,
    ExtractConflictAfter,
    ExtractConflictCountSuffix,
    SkipExisting,
    Overwrite,
    OverwriteTitle,
    OverwriteBodyPrefix,
    OverwriteBodyMiddle,
}

impl Msg for SftpDialogViewMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::NewFolder => "新建文件夹",
            Self::NewFile => "新建文件",
            Self::Rename => "重命名",
            Self::NameTakenRemoteWillAsk => "已经有同名的条目，确定后会问是否覆盖",
            Self::NameTakenWillFail => "已经有同名的条目，确定会失败",
            Self::Cancel => "取消",
            Self::Ok => "确定",
            Self::Delete => "删除",
            Self::Owner => "所有者",
            Self::Group => "组",
            Self::Others => "其他",
            Self::Read => "读",
            Self::Write => "写",
            Self::Execute => "执行",
            Self::PermissionsTitle => "修改权限",
            Self::Octal => "八进制",
            Self::OctalError => "权限要写成 3-4 位八进制（例如 644 或 0644）",
            Self::RecursiveLabel => "递归应用到文件夹里面的所有内容（符号链接不改）",
            Self::Apply => "应用",
            Self::PropertySuffix => " 属性",
            Self::PropertyManySuffix => " 项的属性",
            Self::Selected => "选中",
            Self::SelectedSummaryMiddle => " 项（",
            Self::FolderCountMiddle => " 个文件夹，",
            Self::FileCountSuffix => " 个文件）",
            Self::FileSize => "文件大小",
            Self::Contains => "包含",
            Self::FolderTotal => "文件夹合计",
            Self::EditPermissions => "修改权限…",
            Self::CancelCalc => "取消计算",
            Self::CalcSize => "计算大小",
            Self::Recalc => "重新计算",
            Self::Close => "关闭",
            Self::DownloadTitle => "下载到…",
            Self::DownloadSummaryPrefix => "把选中的 ",
            Self::DownloadSummarySuffix => " 项下载到这台电脑的哪个目录",
            Self::ChooseFolder => "选择文件夹…",
            Self::CheckingDir => "正在检查目录…",
            Self::Download => "下载",
            Self::ExtractConflictTitle => "目标里已有同名文件",
            Self::ExtractConflictPrefix => "解压「",
            Self::ExtractConflictMiddle => "」到「",
            Self::ExtractConflictAfter => "」时，有 ",
            Self::ExtractConflictCountSuffix => " 个文件在里面已经存在。",
            Self::SkipExisting => "跳过已有文件",
            Self::Overwrite => "覆盖",
            Self::OverwriteTitle => "覆盖服务器上的同名文件？",
            Self::OverwriteBodyPrefix => "服务器上已经有一个叫 ",
            Self::OverwriteBodyMiddle => " 的文件，覆盖后原来的内容就没有了。",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::NewFolder => "New Folder",
            Self::NewFile => "New File",
            Self::Rename => "Rename",
            Self::NameTakenRemoteWillAsk => {
                "An entry with that name already exists; you will be asked whether to overwrite"
            }
            Self::NameTakenWillFail => "An entry with that name already exists; this will fail",
            Self::Cancel => "Cancel",
            Self::Ok => "OK",
            Self::Delete => "Delete",
            Self::Owner => "Owner",
            Self::Group => "Group",
            Self::Others => "Others",
            Self::Read => "Read",
            Self::Write => "Write",
            Self::Execute => "Execute",
            Self::PermissionsTitle => "Change Permissions",
            Self::Octal => "Octal",
            Self::OctalError => "Permissions must be written as 3-4 octal digits (for example 644 or 0644)",
            Self::RecursiveLabel => {
                "Apply recursively to everything inside the folder (symbolic links are left unchanged)"
            }
            Self::Apply => "Apply",
            Self::PropertySuffix => " properties",
            Self::PropertyManySuffix => " items' properties",
            Self::Selected => "Selected",
            Self::SelectedSummaryMiddle => " items (",
            Self::FolderCountMiddle => " folders, ",
            Self::FileCountSuffix => " files)",
            Self::FileSize => "File size",
            Self::Contains => "Contains",
            Self::FolderTotal => "Folder total",
            Self::EditPermissions => "Change Permissions…",
            Self::CancelCalc => "Cancel Calculation",
            Self::CalcSize => "Calculate Size",
            Self::Recalc => "Recalculate",
            Self::Close => "Close",
            Self::DownloadTitle => "Download To…",
            Self::DownloadSummaryPrefix => "Download the selected ",
            Self::DownloadSummarySuffix => " items to which folder on this computer?",
            Self::ChooseFolder => "Choose Folder…",
            Self::CheckingDir => "Checking directory…",
            Self::Download => "Download",
            Self::ExtractConflictTitle => "The target already contains files with the same names",
            Self::ExtractConflictPrefix => "Extracting \"",
            Self::ExtractConflictMiddle => "\" to \"",
            Self::ExtractConflictAfter => "\": ",
            Self::ExtractConflictCountSuffix => " files already exist there.",
            Self::SkipExisting => "Skip Existing Files",
            Self::Overwrite => "Overwrite",
            Self::OverwriteTitle => "Overwrite the file with the same name on the server?",
            Self::OverwriteBodyPrefix => "The server already has a file named ",
            Self::OverwriteBodyMiddle => "; overwriting it will discard its current contents.",
        }
    }
}

// ---------------------------------------------------------------------------
// 右键菜单
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn append_nodes(
    mut menu: PopupMenu,
    nodes: &[MenuNode],
    side: Side,
    entries: &Rc<Vec<Entry>>,
    blank: bool,
    entity: &Entity<SftpDemo>,
    tokens: Tokens,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    for node in nodes {
        match node {
            MenuNode::Separator => menu = menu.separator(),
            MenuNode::Item {
                action,
                label,
                shortcut,
                reason,
            } => {
                let action = *action;
                let entries = Rc::clone(entries);
                let entity = entity.clone();
                menu = menu.item(
                    menu_kit::row(&tokens, label.clone(), action.icon(side))
                        .disabled(reason.is_some())
                        .right(reason.clone().or_else(|| shortcut.map(str::to_owned)))
                        .on_click(move |_, window, cx| {
                            let entries = entries.to_vec();
                            entity.update(cx, |this, cx| {
                                this.run_menu_action(side, action, entries, blank, window, cx)
                            });
                        }),
                );
            }
            MenuNode::Submenu {
                label,
                reason,
                children,
            } => {
                if reason.is_some() {
                    // 置灰的子菜单不展开：写成一个置灰的项，原因在右边。
                    menu = menu.item(
                        menu_kit::row(&tokens, *label, submenu_icon(label))
                            .disabled(true)
                            .right(reason.clone())
                            .inert(),
                    );
                } else {
                    let children = children.clone();
                    let entries = Rc::clone(entries);
                    let entity = entity.clone();
                    menu = menu_kit::submenu(
                        menu,
                        *label,
                        submenu_icon(label),
                        window,
                        cx,
                        move |sub, window, cx| {
                            append_nodes(
                                sub.min_w(px(160.0)),
                                &children,
                                side,
                                &entries,
                                blank,
                                &entity,
                                tokens,
                                window,
                                cx,
                            )
                        },
                    );
                }
            }
        }
    }
    menu
}

/// 文件列表的右键菜单：右键按下的那一刻，按这一栏的选择现算。
pub(super) fn file_menu(
    menu: PopupMenu,
    side: Side,
    entity: &Entity<SftpDemo>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let context = entity.read(cx).menu_context(side);
    let tokens = Tokens::current(cx);
    let nodes = build_menu(&context);
    let entries = Rc::new(context.entries);
    append_nodes(
        menu.min_w(px(220.0)),
        &nodes,
        side,
        &entries,
        context.blank,
        entity,
        tokens,
        window,
        cx,
    )
}

/// 传输队列里一行的右键菜单（已完成的行没有可做的操作，菜单是空的，不弹）。
pub(super) fn transfer_menu(
    menu: PopupMenu,
    state: &zssh_core::sftp::transfer::TransferState,
    id: zssh_core::sftp::transfer::TransferId,
    entity: &Entity<SftpDemo>,
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let tokens = Tokens::current(cx);
    let mut menu = menu.min_w(px(140.0));
    for node in build_transfer_menu(state) {
        if let MenuNode::Item { action, label, .. } = node {
            let entity = entity.clone();
            menu = menu.item(
                menu_kit::row(&tokens, label, action.icon(Side::Local)).on_click(
                    move |_, _, cx| {
                        entity.update(cx, |this, cx| this.run_transfer_menu(action, id, cx));
                    },
                ),
            );
        }
    }
    menu
}

// ---------------------------------------------------------------------------
// 对话框
// ---------------------------------------------------------------------------

fn dialog_title(text: impl Into<SharedString>) -> Div {
    div()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

fn buttons(children: Vec<AnyElement>) -> Div {
    h_flex().gap_2().justify_end().children(children)
}

/// 一列等宽小字的名字（删除 / 解压冲突里列出来）。
fn name_list(names: Vec<String>, more: Option<String>, tokens: &Tokens) -> Div {
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
        .font_family("Consolas")
        .text_xs()
        .children(names.into_iter().map(|name| div().truncate().child(name)))
        .children(more.map(|text| div().text_color(tokens.text_muted).child(text)))
}

impl SftpDemo {
    /// 当前对话框（没有就是 `None`）：遮罩 + 居中卡片。
    pub(super) fn render_dialog(
        &mut self,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let entity = cx.entity();
        let (width, content): (f32, AnyElement) = match dialog {
            Dialog::Name(d) => (380.0, self.name_body(d, tokens, cx)),
            Dialog::Delete(d) => (420.0, self.delete_body(d, tokens, cx)),
            Dialog::Permissions(d) => (420.0, self.permissions_body(d, tokens, cx)),
            Dialog::Properties(d) => (500.0, self.properties_body(d, tokens, cx)),
            Dialog::DownloadTo(d) => (460.0, self.download_body(d, tokens, cx)),
            Dialog::ExtractConflict(a) => (480.0, self.extract_body(a, tokens, cx)),
            Dialog::OverwriteRename(a) => (420.0, self.overwrite_body(a, tokens, cx)),
            Dialog::MoveTo(d) => (520.0, self.move_body(d, tokens, cx)),
            Dialog::MoveConflict(c) => (440.0, self.move_conflict_body(c, tokens, cx)),
            Dialog::MoveSummary(r) => (480.0, self.move_summary_body(r, tokens, cx)),
        };
        let click_entity = entity.clone();
        let key_entity = entity.clone();
        Some(
            div()
                .id("sftp-dialog-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0.0, 0.0, 0.0, 0.45))
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    click_entity.update(cx, |this, cx| this.close_dialog(window, cx));
                })
                .on_key_down(move |event, window, cx| {
                    let mods = event.keystroke.modifiers;
                    if mods.modified() {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "escape" => {
                            key_entity.update(cx, |this, cx| this.close_dialog(window, cx));
                            cx.stop_propagation();
                        }
                        "enter" => {
                            key_entity.update(cx, |this, cx| this.dialog_enter(window, cx));
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                })
                .child(
                    v_flex()
                        .id("sftp-dialog-card")
                        .track_focus(&self.dialog_focus)
                        .w(px(width))
                        .max_w_full()
                        .max_h(px(560.0))
                        .m_2()
                        .p_4()
                        .gap_3()
                        .rounded_lg()
                        .bg(tokens.background)
                        .border_1()
                        .border_color(tokens.border)
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(content),
                )
                .into_any_element(),
        )
    }

    // ---- 新建 / 重命名 ----

    fn name_body(&self, d: &NameDialog, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let cancel = entity.clone();
        let ok = entity.clone();
        let name = d.input.read(cx).value().to_string();
        let check = validate_name(&name, d.side);
        let taken = name_taken(&name, &d.existing)
            && !(d.kind == NameKind::Rename
                && d.from.as_ref().is_some_and(|f| f.name == name.trim()));
        let (title, desc) = match d.kind {
            NameKind::Mkdir => (t(SftpDialogViewMsg::NewFolder), d.dir.clone()),
            NameKind::Create => (t(SftpDialogViewMsg::NewFile), d.dir.clone()),
            NameKind::Rename => (
                t(SftpDialogViewMsg::Rename),
                d.from.as_ref().map(|f| f.path.clone()).unwrap_or_default(),
            ),
        };
        let valid = check == NameCheck::Ok;
        v_flex()
            .gap_3()
            .child(dialog_title(title))
            .child(note(desc, tokens))
            .child(Input::new(&d.input))
            .children(match &check {
                NameCheck::Bad(reason) if !name.trim().is_empty() || reason != t(SftpMsg::DialogNameEmpty) => {
                    Some(error_note(reason.clone(), tokens.danger))
                }
                _ => None,
            })
            .children((valid && taken).then(|| {
                error_note(
                    if d.side == Side::Remote && d.kind == NameKind::Rename {
                        t(SftpDialogViewMsg::NameTakenRemoteWillAsk)
                    } else {
                        t(SftpDialogViewMsg::NameTakenWillFail)
                    },
                    tokens.caution,
                )
            }))
            .child(buttons(vec![
                text_button(
                    "sftp-dialog-cancel",
                    t(SftpDialogViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-dialog-ok",
                    t(SftpDialogViewMsg::Ok),
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        ok.update(cx, |this, cx| this.dialog_enter(window, cx));
                    },
                )
                .disabled(!valid)
                .into_any_element(),
            ]))
            .into_any_element()
    }

    // ---- 删除 ----

    fn delete_body(&self, d: &DeleteDialog, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let cancel = entity.clone();
        let ok = entity.clone();
        let (names, more) = delete_names(&d.entries);
        v_flex()
            .gap_3()
            .child(dialog_title(delete_title(d.side, d.entries.len())))
            .child(note(delete_description(&d.entries), tokens))
            .child(name_list(names, more, tokens))
            .child(buttons(vec![
                text_button(
                    "sftp-delete-no",
                    t(SftpDialogViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-delete-yes",
                    t(SftpDialogViewMsg::Delete),
                    ButtonLook::Danger,
                    move |_, window, cx| {
                        ok.update(cx, |this, cx| this.confirm_delete(window, cx));
                    },
                )
                .into_any_element(),
            ]))
            .into_any_element()
    }

    // ---- 修改权限 ----

    fn permissions_body(
        &self,
        d: &PermDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let cancel = entity.clone();
        let ok = entity.clone();
        let text = d.octal.read(cx).value().to_string();
        let octal_ok = super::engine::parse_octal_mode(&text).is_ok();
        let roles: [(&str, [PermBit; 3]); 3] = [
            (
                t(SftpDialogViewMsg::Owner),
                [
                    PermBit::OwnerRead,
                    PermBit::OwnerWrite,
                    PermBit::OwnerExecute,
                ],
            ),
            (
                t(SftpDialogViewMsg::Group),
                [
                    PermBit::GroupRead,
                    PermBit::GroupWrite,
                    PermBit::GroupExecute,
                ],
            ),
            (
                t(SftpDialogViewMsg::Others),
                [
                    PermBit::OthersRead,
                    PermBit::OthersWrite,
                    PermBit::OthersExecute,
                ],
            ),
        ];
        let matrix = d.matrix;
        let grid_head = h_flex()
            .px_1()
            .gap_2()
            .text_xs()
            .text_color(tokens.text_muted)
            .child(div().w(px(64.0)).flex_none())
            .children(
                [
                    t(SftpDialogViewMsg::Read),
                    t(SftpDialogViewMsg::Write),
                    t(SftpDialogViewMsg::Execute),
                ]
                    .into_iter()
                    .map(|label| div().flex_1().text_center().child(label).into_any_element()),
            );
        let rows = roles.into_iter().map(|(label, bits)| {
            h_flex()
                .px_3()
                .py_2()
                .gap_2()
                .items_center()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .child(div().w(px(64.0)).flex_none().text_sm().child(label))
                .children(bits.into_iter().map(|bit| {
                    let toggle = entity.clone();
                    div()
                        .flex_1()
                        .flex()
                        .justify_center()
                        .child(
                            Checkbox::new(SharedString::from(format!("sftp-perm-{bit:?}")))
                                .checked(matrix.get(bit))
                                .on_click(move |_, window, cx| {
                                    toggle.update(cx, |this, cx| {
                                        this.toggle_perm_bit(bit, window, cx)
                                    });
                                }),
                        )
                        .into_any_element()
                }))
                .into_any_element()
        });
        let recursive_entity = entity.clone();
        let is_dir = d.entry.kind == EntryKind::Dir;
        v_flex()
            .gap_3()
            .child(dialog_title(t(SftpDialogViewMsg::PermissionsTitle)))
            .child(note(d.entry.name.clone(), tokens).truncate())
            .child(grid_head)
            .children(rows)
            .child(
                h_flex()
                    .pt_2()
                    .border_t_1()
                    .border_color(tokens.border)
                    .items_center()
                    .justify_between()
                    .child(note(t(SftpDialogViewMsg::Octal), tokens))
                    .child(div().w(px(96.0)).child(Input::new(&d.octal))),
            )
            .children(
                (!octal_ok).then(|| {
                    error_note(t(SftpDialogViewMsg::OctalError), tokens.danger)
                }),
            )
            .children(is_dir.then(|| {
                Checkbox::new("sftp-perm-recursive")
                    .checked(d.recursive)
                    .label(t(SftpDialogViewMsg::RecursiveLabel))
                    .on_click(move |checked, _, cx| {
                        let checked = *checked;
                        recursive_entity.update(cx, |this, cx| {
                            if let Some(Dialog::Permissions(dialog)) = &mut this.dialog {
                                dialog.recursive = checked;
                                cx.notify();
                            }
                        });
                    })
            }))
            .child(buttons(vec![
                text_button(
                    "sftp-perm-cancel",
                    t(SftpDialogViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-perm-apply",
                    t(SftpDialogViewMsg::Apply),
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        ok.update(cx, |this, cx| this.dialog_enter(window, cx));
                    },
                )
                .disabled(!octal_ok)
                .into_any_element(),
            ]))
            .into_any_element()
    }

    // ---- 属性 ----

    fn properties_body(
        &self,
        d: &PropDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let single = d.targets.len() == 1;
        let offset = crate::datetime::local_utc_offset();
        let folders: Vec<usize> = (0..d.targets.len())
            .filter(|&i| can_calculate(&d.targets[i]))
            .collect();
        let states: Vec<props::CalcState> = folders.iter().map(|&i| d.calcs[i].clone()).collect();
        let total = aggregate(&states);
        let running = total.phase == CalcPhase::Running;
        let file_bytes: u64 = d
            .targets
            .iter()
            .filter(|t| t.kind == EntryKind::File)
            .map(|t| t.size.unwrap_or(0))
            .sum();
        let title = if single {
            format!(
                "{}{}",
                d.targets[0].name,
                t(SftpDialogViewMsg::PropertySuffix)
            )
        } else {
            format!(
                "{}{}",
                d.targets.len(),
                t(SftpDialogViewMsg::PropertyManySuffix)
            )
        };
        let line = |label: &str, value: String, id: &'static str| {
            h_flex()
                .gap_3()
                .items_start()
                .child(
                    div()
                        .w(px(88.0))
                        .flex_none()
                        .text_sm()
                        .text_color(tokens.text_muted)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .id(id)
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .child(value)
                        .into_any_element(),
                )
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        if single {
            for (label, value) in info_rows(&d.targets[0], d.side, offset) {
                rows.push(line(label, value, "sftp-prop-row").into_any_element());
            }
        } else {
            let dirs = d
                .targets
                .iter()
                .filter(|t| t.kind == EntryKind::Dir)
                .count();
            let files = d
                .targets
                .iter()
                .filter(|t| t.kind == EntryKind::File)
                .count();
            rows.push(
                line(
                    t(SftpDialogViewMsg::Selected),
                    format!(
                        "{}{}{dirs}{}{files}{}",
                        d.targets.len(),
                        t(SftpDialogViewMsg::SelectedSummaryMiddle),
                        t(SftpDialogViewMsg::FolderCountMiddle),
                        t(SftpDialogViewMsg::FileCountSuffix)
                    ),
                    "sftp-prop-row",
                )
                .into_any_element(),
            );
            if file_bytes > 0 {
                rows.push(
                    line(
                        t(SftpDialogViewMsg::FileSize),
                        bytes_text(file_bytes),
                        "sftp-prop-row",
                    ).into_any_element(),
                );
            }
        }
        if !folders.is_empty() {
            rows.push(
                line(
                    if single {
                        t(SftpDialogViewMsg::Contains)
                    } else {
                        t(SftpDialogViewMsg::FolderTotal)
                    },
                    calc_text(&total),
                    "sftp-prop-calc",
                )
                .into_any_element(),
            );
        }
        let (calc_entity, close_entity, perm_entity) =
            (entity.clone(), entity.clone(), entity.clone());
        let mut footer: Vec<AnyElement> = Vec::new();
        if d.side == Side::Remote && single && d.targets[0].kind != EntryKind::Drive {
            footer.push(
                text_button(
                    "sftp-prop-perm",
                    t(SftpDialogViewMsg::EditPermissions),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        perm_entity
                            .update(cx, |this, cx| this.properties_to_permissions(window, cx));
                    },
                )
                .into_any_element(),
            );
        }
        if !folders.is_empty() {
            let label = if running {
                t(SftpDialogViewMsg::CancelCalc)
            } else if total.phase == CalcPhase::Idle {
                t(SftpDialogViewMsg::CalcSize)
            } else {
                t(SftpDialogViewMsg::Recalc)
            };
            footer.push(
                text_button(
                    if running {
                        "sftp-size-cancel"
                    } else {
                        "sftp-size-start"
                    },
                    label,
                    ButtonLook::Outline,
                    move |_, _, cx| {
                        calc_entity.update(cx, |this, cx| {
                            if running {
                                this.cancel_calcs(cx);
                            } else {
                                this.start_calcs(cx);
                            }
                        });
                    },
                )
                .into_any_element(),
            );
        }
        footer.push(
            text_button(
                "sftp-prop-close",
                t(SftpDialogViewMsg::Close),
                ButtonLook::Primary,
                move |_, window, cx| {
                    close_entity.update(cx, |this, cx| this.close_dialog(window, cx));
                },
            )
            .into_any_element(),
        );
        v_flex()
            .gap_3()
            .child(dialog_title(title))
            .child(v_flex().gap_1p5().children(rows))
            .child(buttons(footer))
            .into_any_element()
    }

    // ---- 下载到… ----

    fn download_body(
        &self,
        d: &DownloadDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let (cancel, ok, pick) = (entity.clone(), entity.clone(), entity.clone());
        let text = d.input.read(cx).value().to_string();
        let check = validate_local_dir(&text);
        v_flex()
            .gap_3()
            .child(dialog_title(t(SftpDialogViewMsg::DownloadTitle)))
            .child(note(
                format!(
                    "{}{}{}",
                    t(SftpDialogViewMsg::DownloadSummaryPrefix),
                    d.entries.len(),
                    t(SftpDialogViewMsg::DownloadSummarySuffix)
                ),
                tokens,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(Input::new(&d.input)))
                    .child(text_button(
                        "sftp-download-pick",
                        t(SftpDialogViewMsg::ChooseFolder),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            pick.update(cx, |this, cx| this.pick_download_folder(window, cx));
                        },
                    )),
            )
            .children(match (&check, text.trim().is_empty()) {
                (NameCheck::Bad(reason), false) => Some(error_note(reason.clone(), tokens.danger)),
                _ => None,
            })
            .children(
                d.error
                    .as_ref()
                    .map(|err| error_note(err.clone(), tokens.danger)),
            )
            .children(d.checking.then(|| note(t(SftpDialogViewMsg::CheckingDir), tokens)))
            .child(buttons(vec![
                text_button(
                    "sftp-download-cancel",
                    t(SftpDialogViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-download-ok",
                    t(SftpDialogViewMsg::Download),
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        ok.update(cx, |this, cx| this.confirm_download_to(window, cx));
                    },
                )
                .disabled(check != NameCheck::Ok || d.checking)
                .into_any_element(),
            ]))
            .into_any_element()
    }

    // ---- 解压遇到同名文件 ----

    fn extract_body(&self, a: &ExtractAsk, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let (cancel, skip, overwrite) = (entity.clone(), entity.clone(), entity.clone());
        let more = conflict_more_text(a.plan.conflict_count, a.plan.conflict_sample.len())
            .map(|text| format!("…{text}"));
        v_flex()
            .gap_3()
            .child(dialog_title(t(SftpDialogViewMsg::ExtractConflictTitle)))
            .child(note(
                format!(
                    "{}{}{}{}{}{}{}",
                    t(SftpDialogViewMsg::ExtractConflictPrefix),
                    a.target.archive_name,
                    t(SftpDialogViewMsg::ExtractConflictMiddle),
                    a.target.dest_name,
                    t(SftpDialogViewMsg::ExtractConflictAfter),
                    a.plan.conflict_count,
                    t(SftpDialogViewMsg::ExtractConflictCountSuffix)
                ),
                tokens,
            ))
            .child(name_list(a.plan.conflict_sample.clone(), more, tokens))
            .child(buttons(vec![
                text_button(
                    "sftp-extract-cancel",
                    t(SftpDialogViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.cancel_extract_choice(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-extract-skip",
                    t(SftpDialogViewMsg::SkipExisting),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        skip.update(cx, |this, cx| {
                            this.choose_extract(ConflictPolicy::SkipExisting, window, cx)
                        });
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-extract-overwrite",
                    t(SftpDialogViewMsg::Overwrite),
                    ButtonLook::Primary,
                    move |_, window, cx| {
                        overwrite.update(cx, |this, cx| {
                            this.choose_extract(ConflictPolicy::Overwrite, window, cx)
                        });
                    },
                )
                .into_any_element(),
            ]))
            .into_any_element()
    }

    // ---- 远端改名撞上同名文件 ----

    fn overwrite_body(
        &self,
        a: &OverwriteAsk,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let (cancel, ok) = (entity.clone(), entity.clone());
        v_flex()
            .gap_3()
            .child(dialog_title(t(SftpDialogViewMsg::OverwriteTitle)))
            .child(note(
                format!(
                    "{}{}{}",
                    t(SftpDialogViewMsg::OverwriteBodyPrefix),
                    a.name,
                    t(SftpDialogViewMsg::OverwriteBodyMiddle)
                ),
                tokens,
            ))
            .child(buttons(vec![
                text_button(
                    "sftp-overwrite-no",
                    t(SftpDialogViewMsg::Cancel),
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        cancel.update(cx, |this, cx| this.close_dialog(window, cx));
                    },
                )
                .into_any_element(),
                text_button(
                    "sftp-overwrite-yes",
                    t(SftpDialogViewMsg::Overwrite),
                    ButtonLook::Danger,
                    move |_, window, cx| {
                        ok.update(cx, |this, cx| this.confirm_overwrite_rename(window, cx));
                    },
                )
                .into_any_element(),
            ]))
            .into_any_element()
    }
}
