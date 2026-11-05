//! 编辑器标签的界面：工具条、提示条（断线 / 冲突 / 属主 / 草稿 / 跳行 / 另存为）、编辑器本体、状态栏。
//!
//! 状态栏的几段文字照 0.2.14 的 `statusSegments`：行列（有选区时带选中字数）、编码、换行、语言、换行模式。

use ::gpui::{ClickEvent, SharedString, Window, px};
use gpui_kit::component::input::{Editor, Input};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::commands::{
    CloseGoto, CopyLineDown, CopyLineUp, DeleteLine, FindNext, FindPrevious, GotoLine,
    InsertLineBelow, JumpToBracket, MoveLineDown, MoveLineUp, Save, SelectLine,
    SelectNextOccurrence, ToggleComment,
};
use super::contract::{encoding_label, eol_label};
use super::drafts::{DraftNotice, draft_age_label, now_unix};
use super::{Phase, RemoteEditor};
use crate::app_theme::Tokens;
use crate::main_window::widgets::{ButtonLook, text_button};

/// 编辑器视图的文案。
#[derive(Clone, Copy, Debug)]
enum ViewMsg {
    PhaseConnecting,
    PhaseLoading,
    PhaseFailed,
    WrapOn,
    Wrap,
    NoWrap,
    Undo,
    Redo,
    Find,
    Replace,
    Goto,
    SaveAs,
    Saving,
    Save,
    Reconnecting,
    DisconnectedKeep,
    Reconnect,
    ConflictPending,
    HandleConflict,
    RemoteDeleted,
    RemoteChanged,
    SaveAsNew,
    DiscardMine,
    OverwriteRemote,
    Later,
    WriteInPlace,
    SaveAnyway,
    Cancel,
    CloseTip,
    Discard,
    Recover,
    CopyToClipboard,
    SaveAsLocal,
    UnsavedKept,
    SaveAndClose,
    DontSave,
    GotoTitle,
    GotoHint,
    SavingAs,
    SaveAsHint,
    Unsaved,
    RemoteChangedSizePrefix,
    RemoteChangedSizeClose,
    SaveNotWrittenSuffix,
    OwnerPrefix,
    OwnerMid,
    OwnerTail,
    DraftFoundPrefix,
    DraftFoundSuffix,
    DraftOldPrefix,
    DraftOldSuffix,
    DraftUnknownPrefix,
    DraftUnknownSuffix,
    LineColPrefix,
    LineColMid,
    SelectedPrefix,
    SelectedClose,
    ByteSuffix,
    CharSuffix,
}

impl crate::i18n::Msg for ViewMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::PhaseConnecting => "连接中",
            Self::PhaseLoading => "读取中",
            Self::PhaseFailed => "读取失败",
            Self::WrapOn => "自动换行：开",
            Self::Wrap => "自动换行",
            Self::NoWrap => "不换行",
            Self::Undo => "撤销",
            Self::Redo => "重做",
            Self::Find => "查找",
            Self::Replace => "替换",
            Self::Goto => "跳行",
            Self::SaveAs => "另存为",
            Self::Saving => "保存中…",
            Self::Save => "保存",
            Self::Reconnecting => "正在重新连接…",
            Self::DisconnectedKeep => {
                "这台主机的连接已经断开，内容还留在编辑器里；重新连接后再保存"
            }
            Self::Reconnect => "重新连接",
            Self::ConflictPending => {
                "有一个保存冲突还没处理：你的修改还在编辑器里，再按保存会重新核对远端"
            }
            Self::HandleConflict => "处理冲突",
            Self::RemoteDeleted => "远端文件已经被删掉了",
            Self::RemoteChanged => "远端文件在你打开之后被改过",
            Self::SaveAsNew => "另存为（同目录新文件）",
            Self::DiscardMine => "放弃我的修改",
            Self::OverwriteRemote => "覆盖远端",
            Self::Later => "稍后处理",
            Self::WriteInPlace => "直接写入原文件",
            Self::SaveAnyway => "仍然保存（属主改变）",
            Self::Cancel => "取消",
            Self::CloseTip => "关闭提示",
            Self::Discard => "丢弃",
            Self::Recover => "恢复",
            Self::CopyToClipboard => "复制到剪贴板",
            Self::SaveAsLocal => "另存为本地文件",
            Self::UnsavedKept => "有未保存的修改（已存为草稿，下次启动可以恢复）",
            Self::SaveAndClose => crate::i18n::t(ViewMsg::SaveAndClose),
            Self::DontSave => "不保存",
            Self::GotoTitle => "跳到行",
            Self::GotoHint => "行号，或 行:列；回车跳转，Esc 关闭",
            Self::SavingAs => "另存中…",
            Self::SaveAsHint => "相对路径按当前文件所在目录解析；存的是副本，仍然编辑原文件",
            Self::Unsaved => "未保存",
            Self::RemoteChangedSizePrefix => "远端文件在你打开之后被改过（现在 ",
            Self::RemoteChangedSizeClose => "）",
            Self::SaveNotWrittenSuffix => {
                "，所以这次保存没有写。你的修改还在编辑器里，选一种处理方式："
            }
            Self::OwnerPrefix => "保存后文件属主会从 ",
            Self::OwnerMid => " 变成 ",
            Self::OwnerTail => {
                "（服务器不让改回去），远端文件还没有被替换。「仍然保存」用临时文件替换，中途断线不会弄残原文件，但属主会变；「直接写入原文件」属主和权限都不变，可写到一半断线会只剩半截文件"
            }
            Self::DraftFoundPrefix => "发现未保存的草稿（",
            Self::DraftFoundSuffix => "）：恢复，还是丢弃？",
            Self::DraftOldPrefix => "草稿基于旧版本（",
            Self::DraftOldSuffix => "），不能自动恢复；可以复制或另存出来再丢弃",
            Self::DraftUnknownPrefix => "发现旧格式草稿（",
            Self::DraftUnknownSuffix => "，认不出属于哪台主机，已原样保留）；可以复制或另存出来",
            Self::LineColPrefix => "行 ",
            Self::LineColMid => "，列 ",
            Self::SelectedPrefix => "（选中 ",
            Self::SelectedClose => "）",
            Self::ByteSuffix => " 字节",
            Self::CharSuffix => " 个字符",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::PhaseConnecting => "Connecting",
            Self::PhaseLoading => "Loading",
            Self::PhaseFailed => "Load failed",
            Self::WrapOn => "Word wrap: on",
            Self::Wrap => "Word wrap",
            Self::NoWrap => "No wrap",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Find => "Find",
            Self::Replace => "Replace",
            Self::Goto => "Go to line",
            Self::SaveAs => "Save as",
            Self::Saving => "Saving…",
            Self::Save => "Save",
            Self::Reconnecting => "Reconnecting…",
            Self::DisconnectedKeep => {
                "The connection to this host is gone; your content is still here — reconnect before saving"
            }
            Self::Reconnect => "Reconnect",
            Self::ConflictPending => {
                "A save conflict is still unresolved: your edits are kept and pressing Save again re-checks the remote file"
            }
            Self::HandleConflict => "Resolve conflict",
            Self::RemoteDeleted => "The remote file was deleted",
            Self::RemoteChanged => "The remote file changed after you opened it",
            Self::SaveAsNew => "Save as (new file in the same folder)",
            Self::DiscardMine => "Discard my changes",
            Self::OverwriteRemote => "Overwrite remote",
            Self::Later => "Handle later",
            Self::WriteInPlace => "Write directly into the original file",
            Self::SaveAnyway => "Save anyway (owner changes)",
            Self::Cancel => "Cancel",
            Self::CloseTip => "Dismiss",
            Self::Discard => "Discard",
            Self::Recover => "Recover",
            Self::CopyToClipboard => "Copy to clipboard",
            Self::SaveAsLocal => "Save to a local file",
            Self::UnsavedKept => {
                "Unsaved changes (kept as a draft; recoverable on the next launch)"
            }
            Self::SaveAndClose => "Save and Close",
            Self::DontSave => "Don't Save",
            Self::GotoTitle => "Go to Line",
            Self::GotoHint => "Line, or line:column; press Enter to jump, Esc to close",
            Self::SavingAs => "Saving…",
            Self::SaveAsHint => {
                "Relative paths resolve against the current file's folder; a copy is written and you keep editing the original"
            }
            Self::Unsaved => "Unsaved",
            Self::RemoteChangedSizePrefix => "The remote file changed after you opened it (now ",
            Self::RemoteChangedSizeClose => ")",
            Self::SaveNotWrittenSuffix => {
                "; so nothing was written this time. Your edits are kept — choose how to proceed:"
            }
            Self::OwnerPrefix => "Saving will change the owner from ",
            Self::OwnerMid => " to ",
            Self::OwnerTail => {
                " (the server will not let it change back), and the remote file has not been replaced yet. \"Save anyway\" replaces it through a temporary file: a dropped connection mid-write will not corrupt the original, but the owner changes. \"Write directly into the original\" keeps the owner and permissions unchanged, but a dropped connection mid-write can leave a partial file."
            }
            Self::DraftFoundPrefix => "Found an unsaved draft (",
            Self::DraftFoundSuffix => "). Recover it or discard it?",
            Self::DraftOldPrefix => "The draft is based on an older version (",
            Self::DraftOldSuffix => {
                "); it cannot be recovered automatically — copy or save it out, then discard"
            }
            Self::DraftUnknownPrefix => "Found an old-format draft (",
            Self::DraftUnknownSuffix => {
                "; it does not belong to a known host and was kept as-is) — copy or save it out"
            }
            Self::LineColPrefix => "Line ",
            Self::LineColMid => ", column ",
            Self::SelectedPrefix => " (",
            Self::SelectedClose => ")",
            Self::ByteSuffix => "bytes",
            Self::CharSuffix => "characters",
        }
    }
}

/// 把「在编辑器上做一件事」包成按钮要的点击回调。
fn act(
    entity: &Entity<RemoteEditor>,
    f: impl Fn(&mut RemoteEditor, &mut Window, &mut Context<RemoteEditor>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let entity = entity.clone();
    move |_, window, cx| {
        entity.update(cx, |this, cx| f(this, window, cx));
    }
}

/// 一条提示条：整行底色（`warn` 为真是红色底，否则是强调色淡底）、文字、右边一组按钮。
fn bar(
    id: &'static str,
    warn: bool,
    message: impl Into<SharedString>,
    buttons: Vec<AnyElement>,
    tokens: &Tokens,
) -> Stateful<Div> {
    let mut tint = if warn { tokens.danger } else { tokens.accent };
    tint.a = 0.12;
    h_flex()
        .id(id)
        .w_full()
        .px_3()
        .py_1p5()
        .gap_2()
        .items_center()
        .flex_none()
        .border_b_1()
        .border_color(tokens.border)
        .bg(tint)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(tokens.text)
                .child(message.into()),
        )
        .children(buttons)
}

impl Render for RemoteEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        if self.want_focus && self.phase == Phase::Ready {
            self.want_focus = false;
            let focus = self.editor.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        let entity = cx.entity();
        let ready = self.phase == Phase::Ready;
        let saving = self.active_save.is_some();
        let unsaved = self.has_unsaved(cx);

        // ---- 工具条 ----
        let phase_text = match self.phase {
            Phase::Connecting => crate::i18n::t(ViewMsg::PhaseConnecting),
            Phase::Loading => crate::i18n::t(ViewMsg::PhaseLoading),
            Phase::Ready => "",
            Phase::Failed => crate::i18n::t(ViewMsg::PhaseFailed),
        };
        let toolbar = h_flex()
            .id("remote-editor-toolbar")
            .w_full()
            .h(px(34.0))
            .px_3()
            .gap_1p5()
            .items_center()
            .flex_none()
            .border_b_1()
            .border_color(tokens.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(tokens.text_muted)
                    .child(SharedString::from(format!(
                        "{}：{}",
                        self.host_label, self.path
                    ))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .child(phase_text),
            )
            .when(ready, |d| {
                d.child(text_button(
                    "remote-editor-wrap",
                    if self.wrap {
                        crate::i18n::t(ViewMsg::WrapOn)
                    } else {
                        crate::i18n::t(ViewMsg::Wrap)
                    },
                    if self.wrap {
                        ButtonLook::Outline
                    } else {
                        ButtonLook::Ghost
                    },
                    act(&entity, |this, window, cx| this.toggle_wrap(window, cx)),
                ))
                .child(text_button(
                    "remote-editor-undo",
                    crate::i18n::t(ViewMsg::Undo),
                    ButtonLook::Ghost,
                    |_, window, cx| {
                        window.dispatch_action(Box::new(gpui_kit::component::input::Undo), cx);
                    },
                ))
                .child(text_button(
                    "remote-editor-redo",
                    crate::i18n::t(ViewMsg::Redo),
                    ButtonLook::Ghost,
                    |_, window, cx| {
                        window.dispatch_action(Box::new(gpui_kit::component::input::Redo), cx);
                    },
                ))
                .child(text_button(
                    "remote-editor-search",
                    crate::i18n::t(ViewMsg::Find),
                    if self.search_button_active(false, cx) {
                        ButtonLook::Outline
                    } else {
                        ButtonLook::Ghost
                    },
                    act(&entity, |this, window, cx| {
                        this.toggle_search(false, window, cx)
                    }),
                ))
                .child(text_button(
                    "remote-editor-replace",
                    crate::i18n::t(ViewMsg::Replace),
                    if self.search_button_active(true, cx) {
                        ButtonLook::Outline
                    } else {
                        ButtonLook::Ghost
                    },
                    act(&entity, |this, window, cx| {
                        this.toggle_search(true, window, cx)
                    }),
                ))
                .child(text_button(
                    "remote-editor-goto",
                    crate::i18n::t(ViewMsg::Goto),
                    ButtonLook::Ghost,
                    act(&entity, |this, window, cx| this.toggle_goto(window, cx)),
                ))
                .child(text_button(
                    "remote-editor-save-as-toggle",
                    crate::i18n::t(ViewMsg::SaveAs),
                    if self.save_as_open {
                        ButtonLook::Outline
                    } else {
                        ButtonLook::Ghost
                    },
                    act(&entity, |this, _window, cx| {
                        this.save_as_open = !this.save_as_open;
                        cx.notify();
                    }),
                ))
                .child(text_button(
                    "remote-editor-save",
                    if saving {
                        crate::i18n::t(ViewMsg::Saving)
                    } else {
                        crate::i18n::t(ViewMsg::Save)
                    },
                    ButtonLook::Primary,
                    act(&entity, |this, _window, cx| this.save(cx)),
                ))
            });

        // ---- 提示条 ----
        let mut bars: Vec<AnyElement> = Vec::new();

        if self.disconnected {
            bars.push(
                bar(
                    "remote-editor-disconnected",
                    true,
                    if self.reconnecting {
                        crate::i18n::t(ViewMsg::Reconnecting)
                    } else {
                        crate::i18n::t(ViewMsg::DisconnectedKeep)
                    },
                    vec![
                        text_button(
                            "remote-editor-reconnect",
                            crate::i18n::t(ViewMsg::Reconnect),
                            ButtonLook::Primary,
                            act(&entity, |this, _window, cx| this.reconnect(cx)),
                        )
                        .into_any_element(),
                    ],
                    &tokens,
                )
                .into_any_element(),
            );
        }

        if let Some(conflict) = &self.conflict {
            if conflict.later {
                bars.push(
                    bar(
                        "remote-editor-conflict-later",
                        true,
                        crate::i18n::t(ViewMsg::ConflictPending),
                        vec![
                            text_button(
                                "remote-editor-conflict-reopen",
                                crate::i18n::t(ViewMsg::HandleConflict),
                                ButtonLook::Outline,
                                act(&entity, |this, _window, cx| this.reopen_conflict(cx)),
                            )
                            .into_any_element(),
                        ],
                        &tokens,
                    )
                    .into_any_element(),
                );
            } else {
                let what = if conflict.remote.gone {
                    crate::i18n::t(ViewMsg::RemoteDeleted).to_owned()
                } else {
                    match conflict.remote.size {
                        Some(size) => format!(
                            "{}{}{}",
                            crate::i18n::t(ViewMsg::RemoteChangedSizePrefix),
                            crate::i18n::count(
                                size,
                                crate::i18n::t(ViewMsg::ByteSuffix),
                                "byte",
                                "bytes"
                            ),
                            crate::i18n::t(ViewMsg::RemoteChangedSizeClose)
                        ),
                        None => crate::i18n::t(ViewMsg::RemoteChanged).to_owned(),
                    }
                };
                let mut tint = tokens.danger;
                tint.a = 0.12;
                bars.push(
                    v_flex()
                        .id("remote-editor-conflict")
                        .w_full()
                        .px_3()
                        .py_1p5()
                        .gap_1p5()
                        .flex_none()
                        .border_b_1()
                        .border_color(tokens.border)
                        .bg(tint)
                        .child(
                            div()
                                .text_sm()
                                .text_color(tokens.text)
                                .child(SharedString::from(format!(
                                    "{what}{}",
                                    crate::i18n::t(ViewMsg::SaveNotWrittenSuffix)
                                ))),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().w(px(260.0)).child(Input::new(&self.conflict_name)))
                                .child(text_button(
                                    "remote-editor-conflict-save-as",
                                    crate::i18n::t(ViewMsg::SaveAsNew),
                                    ButtonLook::Outline,
                                    act(&entity, |this, _window, cx| this.save_conflict_as(cx)),
                                ))
                                .child(text_button(
                                    "remote-editor-conflict-discard",
                                    crate::i18n::t(ViewMsg::DiscardMine),
                                    ButtonLook::Outline,
                                    act(&entity, |this, _window, cx| this.discard_my_changes(cx)),
                                ))
                                .child(text_button(
                                    "remote-editor-conflict-overwrite",
                                    crate::i18n::t(ViewMsg::OverwriteRemote),
                                    ButtonLook::Danger,
                                    act(&entity, |this, _window, cx| this.overwrite_remote(cx)),
                                ))
                                .child(div().flex_1())
                                .child(text_button(
                                    "remote-editor-conflict-later-btn",
                                    crate::i18n::t(ViewMsg::Later),
                                    ButtonLook::Ghost,
                                    act(&entity, |this, _window, cx| this.defer_conflict(cx)),
                                )),
                        )
                        .into_any_element(),
                );
            }
        }

        // 属主保不住：什么都没替换，问用户怎么办。
        if let Some((from, to)) = &self.owner_prompt {
            bars.push(
                bar(
                    "remote-editor-owner",
                    true,
                    format!(
                        "{}{from}{}{to}{}",
                        crate::i18n::t(ViewMsg::OwnerPrefix),
                        crate::i18n::t(ViewMsg::OwnerMid),
                        crate::i18n::t(ViewMsg::OwnerTail)
                    ),
                    vec![
                        text_button(
                            "remote-editor-owner-in-place",
                            crate::i18n::t(ViewMsg::WriteInPlace),
                            ButtonLook::Outline,
                            act(&entity, |this, _window, cx| this.save_in_place(cx)),
                        )
                        .into_any_element(),
                        text_button(
                            "remote-editor-owner-allow",
                            crate::i18n::t(ViewMsg::SaveAnyway),
                            ButtonLook::Danger,
                            act(&entity, |this, _window, cx| {
                                this.save_allowing_owner_change(cx)
                            }),
                        )
                        .into_any_element(),
                        text_button(
                            "remote-editor-owner-cancel",
                            crate::i18n::t(ViewMsg::Cancel),
                            ButtonLook::Ghost,
                            act(&entity, |this, _window, cx| {
                                this.owner_prompt = None;
                                cx.notify();
                            }),
                        )
                        .into_any_element(),
                    ],
                    &tokens,
                )
                .into_any_element(),
            );
        }

        // 草稿提示条。
        if let Some(notice) = &self.draft {
            let age = draft_age_label(notice.draft().saved_at_unix, now_unix());
            let (message, restorable) = match notice {
                DraftNotice::Restorable(_) => (
                    format!(
                        "{}{age}{}",
                        crate::i18n::t(ViewMsg::DraftFoundPrefix),
                        crate::i18n::t(ViewMsg::DraftFoundSuffix)
                    ),
                    true,
                ),
                DraftNotice::Stale(_) => (
                    format!(
                        "{}{age}{}",
                        crate::i18n::t(ViewMsg::DraftOldPrefix),
                        crate::i18n::t(ViewMsg::DraftOldSuffix)
                    ),
                    false,
                ),
                DraftNotice::Legacy(_) => (
                    format!(
                        "{}{age}{}",
                        crate::i18n::t(ViewMsg::DraftUnknownPrefix),
                        crate::i18n::t(ViewMsg::DraftUnknownSuffix)
                    ),
                    false,
                ),
            };
            let discard_label = if matches!(notice, DraftNotice::Legacy(_)) {
                crate::i18n::t(ViewMsg::CloseTip)
            } else {
                crate::i18n::t(ViewMsg::Discard)
            };
            let mut buttons: Vec<AnyElement> = Vec::new();
            if restorable {
                buttons.push(
                    text_button(
                        "remote-editor-draft-restore",
                        crate::i18n::t(ViewMsg::Recover),
                        ButtonLook::Primary,
                        act(&entity, |this, window, cx| this.restore_draft(window, cx)),
                    )
                    .into_any_element(),
                );
            } else {
                buttons.push(
                    text_button(
                        "remote-editor-draft-copy",
                        crate::i18n::t(ViewMsg::CopyToClipboard),
                        ButtonLook::Outline,
                        act(&entity, |this, _window, cx| this.copy_notice(cx)),
                    )
                    .into_any_element(),
                );
                buttons.push(
                    text_button(
                        "remote-editor-draft-export",
                        crate::i18n::t(ViewMsg::SaveAsLocal),
                        ButtonLook::Outline,
                        act(&entity, |this, _window, cx| this.export_notice(cx)),
                    )
                    .into_any_element(),
                );
            }
            buttons.push(
                text_button(
                    "remote-editor-draft-discard",
                    discard_label,
                    ButtonLook::Ghost,
                    act(&entity, |this, _window, cx| this.discard_draft(cx)),
                )
                .into_any_element(),
            );
            bars.push(
                bar("remote-editor-draft", false, message, buttons, &tokens).into_any_element(),
            );
        }

        // 独立窗口关闭时有未保存修改：内容已经落了草稿，这里只问怎么收尾。
        if self.close_prompt {
            bars.push(
                bar(
                    "remote-editor-close",
                    false,
                    crate::i18n::t(ViewMsg::UnsavedKept),
                    vec![
                        text_button(
                            "remote-editor-close-save",
                            crate::i18n::t(ViewMsg::SaveAndClose),
                            ButtonLook::Primary,
                            act(&entity, |this, _window, cx| this.save_then_close(cx)),
                        )
                        .into_any_element(),
                        text_button(
                            "remote-editor-close-discard",
                            crate::i18n::t(ViewMsg::DontSave),
                            ButtonLook::Danger,
                            act(&entity, |this, _window, cx| this.close_without_saving(cx)),
                        )
                        .into_any_element(),
                        text_button(
                            "remote-editor-close-cancel",
                            crate::i18n::t(ViewMsg::Cancel),
                            ButtonLook::Ghost,
                            act(&entity, |this, _window, cx| {
                                this.close_prompt = false;
                                cx.notify();
                            }),
                        )
                        .into_any_element(),
                    ],
                    &tokens,
                )
                .into_any_element(),
            );
        }

        // 跳行条。
        if self.goto_open {
            bars.push(
                h_flex()
                    .id("remote-editor-goto-bar")
                    .key_context("ZsshGoto")
                    .on_action(cx.listener(|this, _: &CloseGoto, window, cx| {
                        this.close_goto(window, cx);
                    }))
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .flex_none()
                    .border_b_1()
                    .border_color(tokens.border)
                    .child(div().text_sm().child(crate::i18n::t(ViewMsg::GotoTitle)))
                    .child(div().w(px(160.0)).child(Input::new(&self.goto_input)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child(crate::i18n::t(ViewMsg::GotoHint)),
                    )
                    .into_any_element(),
            );
        }

        // 另存为（副本）行。
        if self.save_as_open {
            bars.push(
                h_flex()
                    .id("remote-editor-save-as-bar")
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .items_center()
                    .flex_none()
                    .border_b_1()
                    .border_color(tokens.border)
                    .child(div().text_sm().child(crate::i18n::t(ViewMsg::SaveAs)))
                    .child(div().w(px(360.0)).child(Input::new(&self.save_as_input)))
                    .child(text_button(
                        "remote-editor-save-as",
                        if self.saving_as {
                            crate::i18n::t(ViewMsg::SavingAs)
                        } else {
                            crate::i18n::t(ViewMsg::SaveAs)
                        },
                        ButtonLook::Outline,
                        act(&entity, |this, _window, cx| this.save_as(cx)),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .child(crate::i18n::t(ViewMsg::SaveAsHint)),
                    )
                    .into_any_element(),
            );
        }

        // ---- 编辑区 ----
        let body = if ready {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    Editor::new(&self.editor)
                        .h_full()
                        .bordered(false)
                        .text_size(px(self.font_size)),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(SharedString::from(self.status.clone()))
                .into_any_element()
        };

        // ---- 状态栏 ----
        let cursor = self.cursor;
        let position = format!(
            "{}{}{}{}",
            crate::i18n::t(ViewMsg::LineColPrefix),
            cursor.line,
            crate::i18n::t(ViewMsg::LineColMid),
            cursor.column
        );
        let position = if cursor.selected > 0 {
            format!(
                "{position}{}{}{}",
                crate::i18n::t(ViewMsg::SelectedPrefix),
                crate::i18n::count(
                    cursor.selected as u64,
                    crate::i18n::t(ViewMsg::CharSuffix),
                    "character",
                    "characters"
                ),
                crate::i18n::t(ViewMsg::SelectedClose)
            )
        } else {
            position
        };
        let segment = |text: String| {
            div()
                .flex_none()
                .text_xs()
                .text_color(tokens.text_muted)
                .child(SharedString::from(text))
        };
        let status_bar = h_flex()
            .id("remote-editor-status")
            .w_full()
            .h(px(24.0))
            .px_3()
            .gap_4()
            .items_center()
            .flex_none()
            .border_t_1()
            .border_color(tokens.border)
            .child(segment(position))
            .child(segment(encoding_label(&self.format).to_owned()))
            .child(segment(eol_label(&self.format).to_owned()))
            .child(segment(self.lang.display().to_owned()))
            .child(segment(
                if self.wrap {
                    crate::i18n::t(ViewMsg::Wrap)
                } else {
                    crate::i18n::t(ViewMsg::NoWrap)
                }
                .to_owned(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .child(SharedString::from(if ready {
                        self.status.clone()
                    } else {
                        String::new()
                    })),
            )
            .children(unsaved.then(|| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(tokens.accent)
                    .child(crate::i18n::t(ViewMsg::Unsaved))
            }));

        v_flex()
            .size_full()
            .key_context("ZsshEditor")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Save, _window, cx| this.save(cx)))
            .on_action(cx.listener(|this, _: &GotoLine, window, cx| this.toggle_goto(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleComment, window, cx| {
                this.toggle_comment(window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveLineUp, window, cx| {
                this.move_lines(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveLineDown, window, cx| {
                this.move_lines(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyLineUp, window, cx| {
                this.copy_lines(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyLineDown, window, cx| {
                this.copy_lines(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteLine, window, cx| {
                this.delete_lines(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectLine, _window, cx| this.select_line(cx)))
            .on_action(cx.listener(|this, _: &InsertLineBelow, window, cx| {
                this.insert_line_below(window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpToBracket, _window, cx| {
                this.jump_to_bracket(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectNextOccurrence, _window, cx| {
                this.select_next_occurrence(cx);
            }))
            .on_action(cx.listener(|this, _: &FindNext, window, cx| {
                this.find_step(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, window, cx| {
                this.find_step(false, window, cx);
            }))
            .bg(tokens.background)
            .child(toolbar)
            .children(bars)
            .child(body)
            .child(status_bar)
    }
}
