//! 启动时的恢复提示：上次程序没有正常结束（崩溃、被结束进程），有文件的修改没来得及传到服务器。
//! 列出来，每一项可以 打开编辑器接着改 / 重新上传 / 另存到本地 / 放弃（0.2.14 `EditRecoveryDialog` +
//! `EditRecoveryHost` 的语义）。数据来自 `remote_editor::recovery`（编辑草稿 + 外部编辑留下的临时目录）。
//!
//! - 「重新上传」先核对服务器上的文件在这期间有没有被改过（或删了）：改过就不传，列出原因，
//!   这一项多出一个「覆盖服务器上的文件」让用户看过之后再决定；
//! - 「放弃」要点两次（第二次按钮变成「确认放弃」，换别的项就作废）；
//! - 「稍后处理」只是关掉提示，什么都不动，下次启动还会列出来。

use std::collections::{HashMap, HashSet};

use ::gpui::{SharedString, Window, px};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::bridge::{on_blocking, on_ssh_runtime};
use super::modal::Modal;
use super::widgets::{ButtonLook, error_note, note, text_button};
use crate::app_theme::Tokens;
use crate::remote_editor::recovery::{self, Action, Outcome, RecoveryItem};
use crate::remote_editor::{ChannelSource, OpenSpec};

/// 恢复提示的界面状态。
pub struct RecoveryUi {
    pub items: Vec<RecoveryItem>,
    /// 正在做动作的那一项（同一时间只做一个）。
    pub busy: Option<String>,
    /// 每一项最近一次动作的结果文字（`true` = 出错 / 冲突）。
    pub notes: HashMap<String, (bool, String)>,
    /// 重新上传报过冲突的项：多出「覆盖服务器上的文件」。
    pub conflicts: HashSet<String>,
    /// 点过一次「放弃」、等第二次确认的项。
    pub confirm_discard: Option<String>,
}

impl MainWindow {
    /// 启动后扫一次留下的修改（后台线程读盘），有就弹提示。已经有别的对话框开着就等下一个节拍。
    pub(super) fn scan_recovery_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recovery_scanned || self.modal.is_some() {
            return;
        }
        self.recovery_scanned = true;
        let data_dir = crate::settings::data_dir();
        let Ok(rx) = on_blocking(move || recovery::scan(&data_dir)) else {
            return;
        };
        self.await_result(rx, window, cx, |this, items, _window, cx| {
            this.show_recovery(items, cx);
        });
    }

    /// 把扫出来的项交给提示（驱动脚本也从这里进）。没有东西可恢复就什么也不弹。
    pub(crate) fn show_recovery(&mut self, mut items: Vec<RecoveryItem>, cx: &mut Context<Self>) {
        if items.is_empty() || matches!(self.modal, Some(Modal::Recovery(_))) {
            return;
        }
        // 草稿里只有主机 id，主机名按主机库补；库里已经没有这台主机了就保持空（界面只显示路径）。
        for item in &mut items {
            if item.host_label.is_none()
                && let Some(host) = item.host_id.and_then(|id| self.store.host(id))
            {
                item.host_label = Some(host.name.clone());
            }
        }
        self.modal = Some(Modal::Recovery(RecoveryUi {
            items,
            busy: None,
            notes: HashMap::new(),
            conflicts: HashSet::new(),
            confirm_discard: None,
        }));
        cx.notify();
    }

    /// 对一项做动作。放弃要先点一次确认；其余直接做。
    pub(crate) fn recovery_act(
        &mut self,
        id: &str,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Recovery(ui)) = &mut self.modal else {
            return;
        };
        if ui.busy.is_some() {
            return;
        }
        let Some(item) = ui.items.iter().find(|item| item.id == id).cloned() else {
            return;
        };
        if action == Action::Discard && ui.confirm_discard.as_deref() != Some(id) {
            ui.confirm_discard = Some(id.to_owned());
            cx.notify();
            return;
        }
        ui.confirm_discard = None;
        ui.busy = Some(id.to_owned());
        ui.notes.remove(id);
        cx.notify();
        let data_dir = crate::settings::data_dir();
        let Ok(rx) = on_ssh_runtime(async move { recovery::act(&item, action, &data_dir).await })
        else {
            self.recovery_result(
                id,
                action,
                Outcome::Failed("后台运行时起不来".to_owned()),
                window,
                cx,
            );
            return;
        };
        let id = id.to_owned();
        self.await_result(rx, window, cx, move |this, outcome, window, cx| {
            this.recovery_result(&id, action, outcome, window, cx);
        });
    }

    /// 一次动作的结果回来了。
    pub(crate) fn recovery_result(
        &mut self,
        id: &str,
        action: Action,
        outcome: Outcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Recovery(ui)) = &mut self.modal else {
            return;
        };
        ui.busy = None;
        match outcome {
            Outcome::Done(message) => {
                if action == Action::SaveLocal {
                    // 另存出来了，原来的还留着：等用户确认没问题再选放弃。
                    ui.notes.insert(
                        id.to_owned(),
                        (
                            false,
                            format!(
                                "已另存到 {}（{}）",
                                message.unwrap_or_default(),
                                crate::remote_editor::EXPORT_PLAINTEXT_NOTE
                            ),
                        ),
                    );
                } else {
                    ui.items.retain(|item| item.id != id);
                    ui.notes.remove(id);
                    ui.conflicts.remove(id);
                }
            }
            Outcome::Conflict(message) => {
                ui.conflicts.insert(id.to_owned());
                ui.notes.insert(id.to_owned(), (true, message));
            }
            Outcome::Failed(message) => {
                ui.notes.insert(id.to_owned(), (true, message));
            }
        }
        if ui.items.is_empty() {
            self.modal = None;
            self.notice("上次留下的修改都处理完了", window, cx);
        }
        cx.notify();
    }

    /// 「打开并恢复」：在编辑器标签里打开这个文件，草稿提示条会接着给 恢复 / 丢弃。
    pub(crate) fn recovery_open_in_editor(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Recovery(ui)) = &self.modal else {
            return;
        };
        let Some(item) = ui.items.iter().find(|item| item.id == id) else {
            return;
        };
        let (Some(host_id), Some(path)) = (item.host_id, item.path.clone()) else {
            return;
        };
        let host_label = item.host_label.clone().unwrap_or_default();
        self.modal = None;
        self.open_editor_tab(
            OpenSpec {
                host_id,
                host_label,
                path,
                source: ChannelSource::Dial,
            },
            window,
            cx,
        );
    }

    pub(super) fn recovery_body(
        &self,
        ui: &RecoveryUi,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let later = entity.clone();
        let rows = ui.items.iter().enumerate().map(|(index, item)| {
            let id = item.id.clone();
            let busy = ui.busy.is_some();
            let running = ui.busy.as_deref() == Some(item.id.as_str());
            let mut buttons: Vec<AnyElement> = Vec::new();
            if item.can_open_in_editor() {
                let (e, id) = (entity.clone(), id.clone());
                buttons.push(
                    text_button(
                        SharedString::from(format!("recovery-open-{index}")),
                        "打开并恢复",
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            e.update(cx, |this, cx| this.recovery_open_in_editor(&id, window, cx));
                        },
                    )
                    .disabled(busy)
                    .into_any_element(),
                );
            }
            let (e, rid) = (entity.clone(), id.clone());
            buttons.push(
                text_button(
                    SharedString::from(format!("recovery-reupload-{index}")),
                    "重新上传",
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        e.update(cx, |this, cx| {
                            this.recovery_act(&rid, Action::Reupload, window, cx);
                        });
                    },
                )
                .disabled(busy || !item.can_reupload())
                .into_any_element(),
            );
            if ui.conflicts.contains(&item.id) {
                let (e, rid) = (entity.clone(), id.clone());
                buttons.push(
                    text_button(
                        SharedString::from(format!("recovery-overwrite-{index}")),
                        "覆盖服务器上的文件",
                        ButtonLook::Danger,
                        move |_, window, cx| {
                            e.update(cx, |this, cx| {
                                this.recovery_act(&rid, Action::ReuploadOverwrite, window, cx);
                            });
                        },
                    )
                    .disabled(busy)
                    .into_any_element(),
                );
            }
            let (e, rid) = (entity.clone(), id.clone());
            buttons.push(
                text_button(
                    SharedString::from(format!("recovery-save-local-{index}")),
                    "另存到本地",
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        e.update(cx, |this, cx| {
                            this.recovery_act(&rid, Action::SaveLocal, window, cx);
                        });
                    },
                )
                .disabled(busy)
                .into_any_element(),
            );
            let confirming = ui.confirm_discard.as_deref() == Some(item.id.as_str());
            let (e, rid) = (entity.clone(), id.clone());
            buttons.push(
                text_button(
                    SharedString::from(format!("recovery-discard-{index}")),
                    if confirming { "确认放弃" } else { "放弃" },
                    if confirming {
                        ButtonLook::Danger
                    } else {
                        ButtonLook::Ghost
                    },
                    move |_, window, cx| {
                        e.update(cx, |this, cx| {
                            this.recovery_act(&rid, Action::Discard, window, cx);
                        });
                    },
                )
                .disabled(busy)
                .into_any_element(),
            );
            let draft_note = Some("这是编辑器里还没保存的内容（程序退出时来不及保存）。");
            let uncertain_note = item.uncertain.then_some(
                "没法确认这份文件有没有改过（读不了它，或者认不出对应的服务器文件），所以保险起见留下了。请先看一眼内容再决定。",
            );
            let meta_line = if item.age.is_empty() {
                format!("{} 字节", item.size)
            } else {
                format!("{} 字节 · {}", item.size, item.age)
            };
            v_flex()
                .id(("recovery-item", index))
                .w_full()
                .gap_1p5()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(recovery::recovery_line(item))),
                )
                .child(note(meta_line, tokens))
                .children(draft_note.map(|text| note(text, tokens)))
                .children(uncertain_note.map(|text| error_note(text, tokens.danger)))
                .children(
                    ui.notes
                        .get(&item.id)
                        .map(|(is_error, text)| {
                            if *is_error {
                                error_note(text.clone(), tokens.danger)
                            } else {
                                note(text.clone(), tokens)
                            }
                        }),
                )
                .children(running.then(|| note("处理中…", tokens)))
                .child(h_flex().gap_2().flex_wrap().children(buttons))
                .into_any_element()
        });
        v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(recovery::recovery_title(ui.items.len()))),
                    )
                    .child(note(
                        "程序上次没有正常结束，这些文件在本地改过，但没来得及传到服务器。它们还保存着，选一种处理方式。",
                        tokens,
                    )),
            )
            .child(
                v_flex()
                    .id("recovery-list")
                    .gap_2()
                    .max_h(px(380.0))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                h_flex().justify_end().child(text_button(
                    "recovery-later",
                    "稍后处理",
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        later.update(cx, |this, cx| this.close_modal(window, cx));
                    },
                )),
            )
            .into_any_element()
    }
}
