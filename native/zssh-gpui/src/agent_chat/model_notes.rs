//! 发给模型的"旁注"：界面上发生了、但模型不知道的事，在下一条消息前面补一段说明。
//!
//! 对话一旦开始，模型看到的世界就只有它自己的上下文。下面这些事发生时它不会自己知道，
//! 照旧状态继续操作会出错（而且"全部放行"下不会再确认）：
//! - 从历史"继续"：这个对话换了一把能力密钥，旧的作废了，但模型上下文里还留着旧密钥；终端也可能
//!   已经重新打开（工作目录、环境变量、切换过的用户都没了）；
//! - "改用当前"：执行目标换了一台主机，之前看到的输出和目录状态属于旧主机；
//! - 绑定主机的终端换了一个 shell 会话（重新连接 / 换了标签）：同上。
//!
//! 旁注只拼进**发给智能体的文本**；界面上的用户气泡和存进历史的文本永远只有用户原文。旁注在消息
//! 确实发出去之后才清掉，发送失败留着下次再带。旁注是模型侧的文字（和 `config::system_prompt`
//! 一样用英文），不进界面的 i18n。

use std::collections::HashMap;

use crate::main_window::anchor::{Anchor, Key};

/// 旁注块的第一行：告诉模型这不是用户写的。
pub(super) const HEADER: &str = "[ZeeTerm note — not written by the user]";

/// 一条待发旁注。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Note {
    /// 历史"继续"之后的第一条消息：新密钥 + 终端可能已重开。
    Resumed { capability: String },
    /// "改用当前"：执行目标换了。`from` 是最近一次发出消息时的目标（没锚定过是 `None`）。
    Rebound { from: Option<Anchor>, to: Anchor },
    /// 某台绑定主机的终端现在是另一个 shell 会话。
    TerminalReplaced { host_id: i64, name: String },
}

/// 主机名 / 目标名是用户自己起的，放进一行说明之前压成单行、去掉反引号，免得拆散格式。
fn plain(name: &str) -> String {
    name.replace(['\r', '\n', '`'], " ")
}

impl Note {
    /// 渲染成若干条要点（不含前面的 `- `）。
    fn lines(&self) -> Vec<String> {
        match self {
            Note::Resumed { capability } => vec![
                format!(
                    "This conversation's capability key is now `{capability}`. Any key shown \
                     earlier in this conversation is revoked; pass only this one as `capability` \
                     in every zssh tool call."
                ),
                "The terminals may have been reopened since the earlier messages: do not assume \
                 the previous working directory, environment variables, switched user or \
                 foreground program still exist."
                    .to_owned(),
            ],
            Note::Rebound { from, to } => {
                let to = plain(&to.name);
                match from {
                    Some(from) => {
                        let from = plain(&from.name);
                        vec![format!(
                            "The execution target changed from `{from}` to `{to}`. Output, files \
                             and directory state you saw earlier belong to `{from}`; look again \
                             on `{to}` before acting on it. Calls without `target_id` now run on \
                             `{to}`."
                        )]
                    }
                    None => vec![format!(
                        "The execution target is now `{to}`. Calls without `target_id` run on \
                         `{to}`; check its state before acting."
                    )],
                }
            }
            Note::TerminalReplaced { name, .. } => vec![format!(
                "The terminal for `{}` is now a different shell session (it was reconnected or \
                 another terminal was picked): the earlier working directory, environment \
                 variables, switched user and foreground program are gone; check the current \
                 state before continuing.",
                plain(name)
            )],
        }
    }
}

/// 把旁注拼到用户原文前面，得到真正发给智能体的文本；没有旁注就原样返回。
pub(super) fn compose(notes: &[Note], text: &str) -> String {
    if notes.is_empty() {
        return text.to_owned();
    }
    let mut out = String::from(HEADER);
    out.push('\n');
    for line in notes.iter().flat_map(Note::lines) {
        out.push_str("- ");
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(text);
    out
}

/// 一次已经发出、结果还没回来的发送：带了哪些旁注，发出时各主机的终端是哪个。
#[derive(Debug)]
struct InFlight {
    turn: u64,
    notes: Vec<Note>,
    terminals: HashMap<i64, String>,
}

/// 这个对话的待发旁注队列。
#[derive(Debug, Default)]
pub(super) struct ModelNotes {
    pending: Vec<Note>,
    in_flight: Option<InFlight>,
    /// 最近一次**成功发出**消息时，每台主机目标当时用的终端编号（`TermEntry.id`，每次 SSH 会话登记都是
    /// 新编号：重新连接 / 换终端都会变）。
    seen_terminals: HashMap<i64, String>,
}

impl ModelNotes {
    /// 新对话 / 切换智能体 / 开始续接历史：旁注和终端记录都属于旧对话。
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> &[Note] {
        &self.pending
    }

    /// "改用当前"成功：记一条"目标换了"。连续改绑多次而中间没发消息，只留最后一条有效的：起点是
    /// 最近一次发出消息时的目标（第一次改绑前的那个），终点是现在的；改回了原来的主机就不加。
    pub(super) fn note_rebind(&mut self, previous: Option<Anchor>, to: Anchor) {
        let mut from = previous;
        let existing = self
            .pending
            .iter()
            .position(|note| matches!(note, Note::Rebound { .. }))
            .map(|index| self.pending.remove(index));
        if let Some(Note::Rebound { from: first, .. }) = existing {
            from = first;
        }
        if from.as_ref().is_some_and(|from| from.key == to.key) {
            return;
        }
        self.pending.push(Note::Rebound { from, to });
    }

    /// 历史"继续"后第一条消息没发出去（启动成功但发送失败）：把续接旁注放回去，下次再带。
    pub(super) fn requeue_resumed(&mut self, capability: String) {
        self.pending
            .retain(|note| !matches!(note, Note::Resumed { .. }));
        self.pending.insert(0, Note::Resumed { capability });
    }

    /// 要发一条消息了：核对各主机目标的终端有没有换，返回这条消息要带的旁注。
    ///
    /// `hosts` 是这个对话现在的主机目标（有主机库 id 的，不含快速连接终端），`terminal_of` 查某台主机
    /// 此刻选中的终端编号。上次发出消息时记过某台主机的终端、现在是另一个 → 补一条（同一台主机多次
    /// 变化只留一条；变回原来的就撤掉）。现在没有打开的终端不补（发送本来就会被拦住或自动连接）。
    /// "改用当前"刚改绑到的主机不再补这条：改绑旁注已经让模型重新查看现状。
    pub(super) fn begin_send(
        &mut self,
        turn: u64,
        hosts: &[(i64, String)],
        terminal_of: impl Fn(i64) -> Option<String>,
    ) -> Vec<Note> {
        let rebound_hosts: Vec<i64> = self
            .pending
            .iter()
            .filter_map(|note| match note {
                Note::Rebound {
                    to:
                        Anchor {
                            key: Key::Host(id), ..
                        },
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        // 不在目标里了的主机（用户把它 × 掉）：它的旁注作废。
        self.pending.retain(|note| match note {
            Note::TerminalReplaced { host_id, .. } => hosts.iter().any(|(id, _)| id == host_id),
            _ => true,
        });
        let mut now = HashMap::new();
        for (id, name) in hosts {
            let Some(current) = terminal_of(*id) else {
                continue;
            };
            let changed = !rebound_hosts.contains(id)
                && self
                    .seen_terminals
                    .get(id)
                    .is_some_and(|previous| *previous != current);
            self.pending.retain(
                |note| !matches!(note, Note::TerminalReplaced { host_id, .. } if host_id == id),
            );
            if changed {
                self.pending.push(Note::TerminalReplaced {
                    host_id: *id,
                    name: name.clone(),
                });
            }
            now.insert(*id, current);
        }
        let notes = self.pending.clone();
        self.in_flight = Some(InFlight {
            turn,
            notes: notes.clone(),
            terminals: now,
        });
        notes
    }

    /// 这条消息确实发出去了（`turn` 要和 [`Self::begin_send`] 的对得上，迟到的旧结果不算）：
    /// 带出去的旁注摘掉，终端记录更新为发出时的。
    pub(super) fn confirm_sent(&mut self, turn: u64) {
        let Some(flight) = self.in_flight.take_if(|flight| flight.turn == turn) else {
            return;
        };
        for sent in &flight.notes {
            if let Some(index) = self.pending.iter().position(|note| note == sent) {
                self.pending.remove(index);
            }
        }
        self.seen_terminals.extend(flight.terminals);
    }

    /// 这条消息没发出去 / 作废了：旁注和终端记录都不动，下次再带。
    pub(super) fn send_failed(&mut self, turn: u64) {
        self.in_flight.take_if(|flight| flight.turn == turn);
    }

    /// 在途的发送作废（启动失败、启动期间点了停止），不管是哪一回合。
    pub(super) fn abandon_send(&mut self) {
        self.in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: i64, name: &str) -> Anchor {
        Anchor {
            key: Key::Host(id),
            name: name.to_owned(),
        }
    }

    fn hosts(list: &[(i64, &str)]) -> Vec<(i64, String)> {
        list.iter()
            .map(|(id, name)| (*id, (*name).to_owned()))
            .collect()
    }

    /// 查表替身：`(主机, 终端)`。
    fn terminals(list: &[(i64, &str)]) -> impl Fn(i64) -> Option<String> {
        let table: HashMap<i64, String> = list
            .iter()
            .map(|(id, term)| (*id, (*term).to_owned()))
            .collect();
        move |id| table.get(&id).cloned()
    }

    #[test]
    fn no_notes_leaves_the_text_untouched() {
        assert_eq!(compose(&[], "你好"), "你好");
    }

    #[test]
    fn notes_are_a_prefix_and_the_user_text_follows_verbatim() {
        let notes = [
            Note::TerminalReplaced {
                host_id: 1,
                name: "web-1".to_owned(),
            },
            Note::Rebound {
                from: Some(host(1, "web-1")),
                to: host(2, "db-1"),
            },
        ];
        let text = compose(&notes, "查一下磁盘\n再看看内存");
        assert!(text.starts_with(&format!("{HEADER}\n- ")), "{text}");
        assert!(text.ends_with("\n\n查一下磁盘\n再看看内存"), "{text}");
        assert_eq!(text.matches("\n- ").count(), 2, "每条旁注一个要点：{text}");
        assert!(text.contains("`web-1`") && text.contains("`db-1`"));
    }

    #[test]
    fn the_resume_note_carries_the_new_key_and_the_terminal_warning() {
        let note = Note::Resumed {
            capability: "cap-xyz".to_owned(),
        };
        let text = compose(&[note], "继续");
        assert!(text.contains("`cap-xyz`"));
        assert!(text.contains("revoked"));
        assert!(text.contains("working directory"));
        assert!(text.ends_with("\n\n继续"));
    }

    #[test]
    fn a_host_name_cannot_break_the_note_layout() {
        let note = Note::Rebound {
            from: Some(host(1, "a`\nb")),
            to: host(2, "c\r\nd"),
        };
        let text = compose(&[note], "x");
        // 头一行 + 一个要点 + 空行 + 原文。
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(!text.contains("a`"));
    }

    // ---- D：改绑 ----

    #[test]
    fn a_rebind_notes_both_hosts() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "web-1")), host(2, "db-1"));
        assert_eq!(
            notes.pending(),
            [Note::Rebound {
                from: Some(host(1, "web-1")),
                to: host(2, "db-1")
            }]
        );
    }

    #[test]
    fn a_rebind_without_a_previous_anchor_only_names_the_new_target() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(None, host(2, "db-1"));
        let text = compose(notes.pending(), "x");
        assert!(text.contains("target is now `db-1`"), "{text}");
    }

    #[test]
    fn several_rebinds_without_a_message_keep_the_first_start_and_the_last_end() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.note_rebind(Some(host(2, "B")), host(3, "C"));
        assert_eq!(
            notes.pending(),
            [Note::Rebound {
                from: Some(host(1, "A")),
                to: host(3, "C")
            }]
        );
    }

    #[test]
    fn rebinding_back_to_the_original_host_adds_nothing() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.note_rebind(Some(host(2, "B")), host(1, "A"));
        assert!(notes.pending().is_empty(), "{:?}", notes.pending());
    }

    #[test]
    fn a_rebind_after_a_confirmed_send_starts_from_the_new_target() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.begin_send(1, &hosts(&[(2, "B")]), terminals(&[]));
        notes.confirm_sent(1);
        assert!(notes.pending().is_empty());
        notes.note_rebind(Some(host(2, "B")), host(1, "A"));
        assert_eq!(
            notes.pending(),
            [Note::Rebound {
                from: Some(host(2, "B")),
                to: host(1, "A")
            }],
            "上一次改绑的旁注已经发出，这次是新的一条"
        );
    }

    // ---- E：终端换了 ----

    #[test]
    fn a_new_terminal_for_a_known_host_is_noted_once() {
        let mut notes = ModelNotes::default();
        let list = hosts(&[(1, "web-1")]);
        // 第一条消息：没有记录，不补；发出去之后记下 term-1。
        assert!(
            notes
                .begin_send(1, &list, terminals(&[(1, "term-1")]))
                .is_empty()
        );
        notes.confirm_sent(1);
        // 终端没变：不补。
        assert!(
            notes
                .begin_send(2, &list, terminals(&[(1, "term-1")]))
                .is_empty()
        );
        notes.confirm_sent(2);
        // 重新连接后是 term-2：补一条。
        let sent = notes.begin_send(3, &list, terminals(&[(1, "term-2")]));
        assert_eq!(
            sent,
            [Note::TerminalReplaced {
                host_id: 1,
                name: "web-1".to_owned()
            }]
        );
        notes.confirm_sent(3);
        // 已经告诉过了，终端没再变：不重复。
        assert!(
            notes
                .begin_send(4, &list, terminals(&[(1, "term-2")]))
                .is_empty()
        );
    }

    #[test]
    fn a_host_changing_terminal_twice_before_a_send_gets_one_note() {
        let mut notes = ModelNotes::default();
        let list = hosts(&[(1, "web-1")]);
        notes.begin_send(1, &list, terminals(&[(1, "term-1")]));
        notes.confirm_sent(1);
        // 一次发送失败（旁注留着），期间终端又换了一次。
        let first = notes.begin_send(2, &list, terminals(&[(1, "term-2")]));
        assert_eq!(first.len(), 1);
        notes.send_failed(2);
        let second = notes.begin_send(3, &list, terminals(&[(1, "term-3")]));
        assert_eq!(second.len(), 1, "同一台主机只出一条：{second:?}");
        notes.confirm_sent(3);
        assert!(notes.pending().is_empty());
    }

    #[test]
    fn going_back_to_the_recorded_terminal_withdraws_the_note() {
        let mut notes = ModelNotes::default();
        let list = hosts(&[(1, "web-1")]);
        notes.begin_send(1, &list, terminals(&[(1, "term-1")]));
        notes.confirm_sent(1);
        assert_eq!(
            notes
                .begin_send(2, &list, terminals(&[(1, "term-2")]))
                .len(),
            1
        );
        notes.send_failed(2);
        // 又切回 term-1（它一直开着）：模型看到的 shell 没变，不必提示。
        assert!(
            notes
                .begin_send(3, &list, terminals(&[(1, "term-1")]))
                .is_empty()
        );
    }

    #[test]
    fn no_open_terminal_now_adds_nothing_and_keeps_the_record() {
        let mut notes = ModelNotes::default();
        let list = hosts(&[(1, "web-1")]);
        notes.begin_send(1, &list, terminals(&[(1, "term-1")]));
        notes.confirm_sent(1);
        assert!(notes.begin_send(2, &list, terminals(&[])).is_empty());
        notes.confirm_sent(2);
        // 记录没被"没有终端"覆盖：再开出来的是另一个终端，照样提示。
        assert_eq!(
            notes
                .begin_send(3, &list, terminals(&[(1, "term-9")]))
                .len(),
            1
        );
    }

    #[test]
    fn the_rebound_host_gets_no_extra_terminal_note() {
        let mut notes = ModelNotes::default();
        let list = hosts(&[(1, "web-1"), (2, "db-1")]);
        notes.begin_send(1, &list, terminals(&[(1, "term-1"), (2, "term-5")]));
        notes.confirm_sent(1);
        // 改绑到 db-1，同时 db-1 的终端也换过了：只留改绑那条。
        notes.note_rebind(Some(host(1, "web-1")), host(2, "db-1"));
        let sent = notes.begin_send(2, &list, terminals(&[(1, "term-1"), (2, "term-6")]));
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(matches!(sent[0], Note::Rebound { .. }));
        notes.confirm_sent(2);
        // 终端记录照常更新：之后 db-1 再换才会提示。
        assert!(
            notes
                .begin_send(3, &list, terminals(&[(1, "term-1"), (2, "term-6")]))
                .is_empty()
        );
    }

    #[test]
    fn a_removed_host_drops_its_pending_terminal_note() {
        let mut notes = ModelNotes::default();
        let both = hosts(&[(1, "web-1"), (2, "db-1")]);
        notes.begin_send(1, &both, terminals(&[(1, "t1"), (2, "t2")]));
        notes.confirm_sent(1);
        assert_eq!(
            notes
                .begin_send(2, &both, terminals(&[(1, "t9"), (2, "t2")]))
                .len(),
            1
        );
        notes.send_failed(2);
        // web-1 被 × 掉了：它的旁注不该再带给模型。
        let only_db = hosts(&[(2, "db-1")]);
        assert!(
            notes
                .begin_send(3, &only_db, terminals(&[(2, "t2")]))
                .is_empty()
        );
    }

    // ---- 发送结果 ----

    #[test]
    fn notes_stay_queued_until_the_send_is_confirmed() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        let first = notes.begin_send(1, &hosts(&[(2, "B")]), terminals(&[]));
        assert_eq!(first.len(), 1);
        // 发送失败：旁注还在，下一次再带。
        notes.send_failed(1);
        assert_eq!(notes.pending().len(), 1);
        let second = notes.begin_send(2, &hosts(&[(2, "B")]), terminals(&[]));
        assert_eq!(second, first);
        notes.confirm_sent(2);
        assert!(notes.pending().is_empty());
    }

    #[test]
    fn a_late_result_for_an_older_send_is_ignored() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.begin_send(1, &hosts(&[(2, "B")]), terminals(&[]));
        notes.begin_send(2, &hosts(&[(2, "B")]), terminals(&[]));
        notes.confirm_sent(1);
        assert_eq!(notes.pending().len(), 1, "第 1 次的结果不能确认第 2 次");
        notes.confirm_sent(2);
        assert!(notes.pending().is_empty());
    }

    #[test]
    fn a_note_added_while_a_send_is_in_flight_is_not_lost_when_it_is_confirmed() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.begin_send(1, &hosts(&[(2, "B")]), terminals(&[]));
        // 结果回来之前又改绑了一次（旁注内容变了）。
        notes.note_rebind(Some(host(2, "B")), host(3, "C"));
        notes.confirm_sent(1);
        assert_eq!(notes.pending().len(), 1, "{:?}", notes.pending());
    }

    #[test]
    fn requeuing_the_resume_note_goes_first_and_never_twice() {
        let mut notes = ModelNotes::default();
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.requeue_resumed("c1".to_owned());
        notes.requeue_resumed("c1".to_owned());
        assert_eq!(notes.pending().len(), 2);
        assert!(matches!(notes.pending()[0], Note::Resumed { .. }));
    }

    #[test]
    fn reset_forgets_everything_that_belonged_to_the_old_conversation() {
        let mut notes = ModelNotes::default();
        let list = hosts(&[(1, "web-1")]);
        notes.begin_send(1, &list, terminals(&[(1, "t1")]));
        notes.confirm_sent(1);
        notes.note_rebind(Some(host(1, "A")), host(2, "B"));
        notes.reset();
        assert!(notes.pending().is_empty());
        // 终端记录也清了：新对话里第一次看到 t2 不算变化。
        assert!(
            notes
                .begin_send(2, &list, terminals(&[(1, "t2")]))
                .is_empty()
        );
    }
}
