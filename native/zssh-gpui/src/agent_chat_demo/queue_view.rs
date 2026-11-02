//! 「待发送」队列在对话区底部的样子（V2-U1 第 2 条；状态规则在 `queue.rs`）。

use ::gpui::{AnyElement, IntoElement as _, SharedString, Window};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::*;
use gpui_kit::*;

use super::AgentChatDemo;

impl AgentChatDemo {
    /// 队列里有话就画一块；没有就不画。回合进行中说「等当前回合结束后发给 AI」，
    /// 停掉之后（队列被扣住）说「已停止，没有发出」并给「现在发送」。
    pub(super) fn queue_block(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.real || self.chat.queue.is_empty() {
            return None;
        }
        let entity = cx.entity();
        let running = self.chat.is_running();
        let held = self.chat.queue.is_held();
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let count = self.chat.queue.len();
        let editing = self.chat.queue.editing();
        let can_interrupt = running && self.session.is_some();

        let heading: SharedString = if self.chat.queue.is_winding_down() {
            format!("待发送 {count} 条 · 正在打断当前回合，马上发出").into()
        } else if running {
            format!("待发送 {count} 条 · 等当前回合结束后发给 AI").into()
        } else if editing.is_some() {
            format!("待发送 {count} 条 · 保存或取消编辑后发出").into()
        } else if held {
            format!("待发送 {count} 条 · 已停止，这些还没有发出").into()
        } else {
            format!("待发送 {count} 条").into()
        };

        let action = if can_interrupt {
            let entity = entity.clone();
            Some(
                Button::new("ai-queue-interrupt")
                    .compact()
                    .outline()
                    .label("打断并发送")
                    .tooltip("停止当前回合，马上把这些发给 AI")
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |this, cx| this.interrupt_and_send(cx));
                    })
                    .into_any_element(),
            )
        } else if !running && held {
            let entity = entity.clone();
            Some(
                Button::new("ai-queue-send-now")
                    .compact()
                    .primary()
                    .label("现在发送")
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |this, cx| this.queue_send_now(cx));
                    })
                    .into_any_element(),
            )
        } else {
            None
        };

        let rows: Vec<AnyElement> = self
            .chat
            .queue
            .items()
            .iter()
            .map(|item| {
                let id = item.id;
                if let Some((edit_id, state)) = self.queue_edit.as_ref()
                    && *edit_id == id
                {
                    let save = entity.clone();
                    let cancel = entity.clone();
                    return v_flex()
                        .id(("ai-queue-row", id))
                        .gap_1()
                        .child(Textarea::new(state).w_full())
                        .child(
                            h_flex()
                                .gap_1()
                                .justify_end()
                                .child(
                                    Button::new(("ai-queue-cancel", id))
                                        .compact()
                                        .ghost()
                                        .label("取消")
                                        .on_click(move |_, _window, cx| {
                                            cancel.update(cx, |this, cx| {
                                                this.queue_cancel_edit(cx);
                                            });
                                        }),
                                )
                                .child(
                                    Button::new(("ai-queue-save", id))
                                        .compact()
                                        .primary()
                                        .label("保存")
                                        .on_click(move |_, _window, cx| {
                                            save.update(cx, |this, cx| this.queue_save_edit(cx));
                                        }),
                                ),
                        )
                        .into_any_element();
                }
                let edit = entity.clone();
                let remove = entity.clone();
                // 文字占一整行、按钮在下面靠右：面板再窄文字也不会被按钮挤成一列。
                v_flex()
                    .id(("ai-queue-row", id))
                    .gap_0p5()
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_sm()
                            .child(SharedString::from(item.text.clone())),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .justify_end()
                            .child(
                                Button::new(("ai-queue-edit", id))
                                    .compact()
                                    .ghost()
                                    .label("编辑")
                                    .on_click(move |_, window, cx| {
                                        edit.update(cx, |this, cx| {
                                            this.queue_begin_edit(id, window, cx);
                                        });
                                    }),
                            )
                            .child(
                                Button::new(("ai-queue-remove", id))
                                    .compact()
                                    .ghost()
                                    .label("删除")
                                    .on_click(move |_, _window, cx| {
                                        remove.update(cx, |this, cx| this.queue_remove(id, cx));
                                    }),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            v_flex()
                .id("agent-chat-queue")
                .flex_none()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(border)
                .child(
                    // 提示在上、按钮在下靠右：面板窄的时候提示也不会被按钮挤成一列。
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .text_xs()
                                .text_color(muted)
                                .child(heading),
                        )
                        .children(action.map(|button| h_flex().justify_end().child(button))),
                )
                .children(rows)
                .into_any_element(),
        )
    }
}

/// 调试驱动用的入口（`ai_queue` 动作）：和界面按钮走同一批方法。
impl AgentChatDemo {
    pub fn dev_queue_texts(&self) -> Vec<String> {
        self.chat
            .queue
            .items()
            .iter()
            .map(|item| item.text.clone())
            .collect()
    }

    pub fn dev_queue_held(&self) -> bool {
        self.chat.queue.is_held()
    }

    pub fn dev_queue_editing(&self) -> bool {
        self.chat.queue.editing().is_some()
    }

    fn dev_queue_id(&self, nth: usize) -> Result<u64, String> {
        self.chat
            .queue
            .items()
            .get(nth)
            .map(|item| item.id)
            .ok_or_else(|| format!("队列里没有第 {nth} 条"))
    }

    pub fn dev_queue_begin_edit(
        &mut self,
        nth: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let id = self.dev_queue_id(nth)?;
        self.queue_begin_edit(id, window, cx);
        Ok(())
    }

    /// 编辑第 n 条：打开编辑框、写入新文字、点「保存」。
    pub fn dev_queue_edit(
        &mut self,
        nth: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.dev_queue_begin_edit(nth, window, cx)?;
        let Some((_, state)) = self.queue_edit.as_ref() else {
            return Err("编辑框没有打开".to_owned());
        };
        let text = text.to_owned();
        state.update(cx, |state, cx| state.set_value(text, window, cx));
        self.queue_save_edit(cx);
        Ok(())
    }

    pub fn dev_queue_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.queue_cancel_edit(cx);
    }

    pub fn dev_queue_remove(&mut self, nth: usize, cx: &mut Context<Self>) -> Result<(), String> {
        let id = self.dev_queue_id(nth)?;
        self.queue_remove(id, cx);
        Ok(())
    }

    pub fn dev_queue_interrupt(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        if !self.chat.queue_can_interrupt() || self.session.is_none() {
            return Err("现在不能打断并发送（没有在跑的回合或队列是空的）".to_owned());
        }
        self.interrupt_and_send(cx);
        Ok(())
    }

    pub fn dev_queue_send_now(&mut self, cx: &mut Context<Self>) {
        self.queue_send_now(cx);
    }
}
