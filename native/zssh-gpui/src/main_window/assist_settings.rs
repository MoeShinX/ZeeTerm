//! 「命令建议」的开关和清除入口：设置 → 终端里的「命令建议」小节，以及状态栏弹窗里同样两个开关的快捷入口。
//!
//! - `terminal_assist.enabled`（自动建议）：关掉立即收起浮层和选择模式，不再生成候选，也不做输入跟踪；
//! - `terminal_assist.history_enabled`（采集命令历史）：关掉停止采集并清空内存里已有的历史；
//! - 清除当前目标 / 全部内存历史；
//! - 「选择建议」快捷键：默认不绑定；用户自己绑定，界面说明"此上下文将拦截该组合"。
//!
//! 这些都是本机设置，不参与同步（见 `sync::settings`）。历史只在内存里，所以"清除"没有磁盘上的东西要处理。

use ::gpui::{AnyElement, KeyDownEvent, Window};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::widgets::{ButtonLook, note, text_button};
use crate::app_prefs::{self, TERMINAL_ASSIST_ENABLED_KEY, TERMINAL_ASSIST_HISTORY_KEY};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::settings_nav::SettingsSection;
use crate::settings_view::{card, row, row_with};
use crate::terminal_workspace::binding_text;

/// 这一组界面上的文案。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AssistSettingsMsg {
    CardTitle,
    CardDescription,
    AutoTitle,
    AutoDescription,
    HistoryTitle,
    HistoryDescription,
    KeyTitle,
    KeyDescription,
    KeyUnbound,
    KeySet,
    KeyClear,
    KeyListening,
    KeyCancel,
    KeyNeedsModifier,
    KeyBound,
    KeyCleared,
    ClearTitle,
    ClearDescription,
    ClearCurrent,
    ClearAll,
    ClearedPrefix,
    ClearedSuffix,
    OpenSettings,
}

impl Msg for AssistSettingsMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::CardTitle => "命令建议",
            Self::CardDescription => {
                "在终端输入命令时，根据这台主机上执行过的命令和你的快捷命令给出候选。只有当前会话提供了输入行标记（或本次启用了 Bash / Zsh 集成）时才会出现；原生 Tab、方向键和 Enter 不受影响，也不会自动执行。"
            }
            Self::AutoTitle => "自动建议",
            Self::AutoDescription => "关掉后立即收起候选，也不再跟踪终端输入。",
            Self::HistoryTitle => "记录命令历史",
            Self::HistoryDescription => {
                "历史只放在内存里：按主机分开、不写盘、不同步、不导入远端 shell 历史；以空格开头的命令和疑似含密码 / 令牌的命令不记录。关掉会同时清空已有的历史。"
            }
            Self::KeyTitle => "「选择建议」快捷键",
            Self::KeyDescription => {
                "默认不绑定。绑定后，候选显示时按下它进入选择模式（用 ↑↓ 选，Tab / Enter 填入）。此上下文将拦截该组合，不再发给远端程序。"
            }
            Self::KeyUnbound => "未绑定",
            Self::KeySet => "设置…",
            Self::KeyClear => "清除",
            Self::KeyListening => "请按下组合键（需要带 Ctrl、Alt 或 Cmd），Esc 取消…",
            Self::KeyCancel => "取消",
            Self::KeyNeedsModifier => {
                "这个组合会和普通输入冲突：需要带 Ctrl、Alt 或 Cmd，且不能是 Esc、Enter、Tab。"
            }
            Self::KeyBound => "已绑定。此上下文将拦截该组合。",
            Self::KeyCleared => "已清除绑定。",
            Self::ClearTitle => "清除历史",
            Self::ClearDescription => "只清内存里的命令历史，不影响撰写栏的历史和快捷命令。",
            Self::ClearCurrent => "清除当前目标",
            Self::ClearAll => "清除全部",
            Self::ClearedPrefix => "已清除 ",
            Self::ClearedSuffix => " 条历史。",
            Self::OpenSettings => "更多设置…",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::CardTitle => "Command Suggestions",
            Self::CardDescription => {
                "While you type a command in the terminal, offers suggestions from the commands run on this host and your quick commands. They appear only when the session provides input-line marks (or the Bash / Zsh integration was turned on for this session). Native Tab, arrow keys and Enter are unaffected, and nothing is ever run automatically."
            }
            Self::AutoTitle => "Auto suggestions",
            Self::AutoDescription => {
                "When off, suggestions close at once and terminal input is no longer tracked."
            }
            Self::HistoryTitle => "Record command history",
            Self::HistoryDescription => {
                "History stays in memory only: kept per host, never written to disk or synced, and never imported from the remote shell history. Commands that start with a space and commands that look like they hold a password or token are not recorded. Turning this off also clears the history."
            }
            Self::KeyTitle => "\"Select suggestion\" shortcut",
            Self::KeyDescription => {
                "Not bound by default. Once bound, press it while suggestions are showing to enter select mode (pick with up/down, fill in with Tab / Enter). This context will intercept the combination; it is no longer sent to the remote program."
            }
            Self::KeyUnbound => "Not bound",
            Self::KeySet => "Set...",
            Self::KeyClear => "Clear",
            Self::KeyListening => {
                "Press the combination (it needs Ctrl, Alt or Cmd), Esc to cancel..."
            }
            Self::KeyCancel => "Cancel",
            Self::KeyNeedsModifier => {
                "That combination would clash with normal typing: it needs Ctrl, Alt or Cmd, and cannot be Esc, Enter or Tab."
            }
            Self::KeyBound => "Bound. This context will intercept the combination.",
            Self::KeyCleared => "Binding cleared.",
            Self::ClearTitle => "Clear history",
            Self::ClearDescription => {
                "Clears only the in-memory command history; the compose bar history and quick commands are untouched."
            }
            Self::ClearCurrent => "Clear this target",
            Self::ClearAll => "Clear all",
            Self::ClearedPrefix => "Cleared ",
            Self::ClearedSuffix => " history entries.",
            Self::OpenSettings => "More settings...",
        }
    }
}

/// 设置里存的按键写法 -> 给人看的样子（`ctrl-alt-s` -> `Ctrl+Alt+S`）。
pub(super) fn binding_label(stored: &str) -> String {
    if stored.is_empty() {
        return t(AssistSettingsMsg::KeyUnbound).to_owned();
    }
    stored
        .split('-')
        .map(|part| match part {
            "ctrl" => "Ctrl".to_owned(),
            "alt" => "Alt".to_owned(),
            "shift" => "Shift".to_owned(),
            "cmd" => "Cmd".to_owned(),
            "fn" => "Fn".to_owned(),
            other if other.chars().count() == 1 => other.to_uppercase(),
            other => {
                let mut chars = other.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl MainWindow {
    fn write_assist_flag(
        &mut self,
        key: &'static str,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: fn(&mut app_prefs::AppPrefs, bool),
    ) {
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_flag(db, key, value));
        match result {
            Ok(()) => {
                apply(&mut self.prefs, value);
                // 终端每帧读缓存的设置，刷新缓存；再立刻通知工作区，不等下一个节拍。
                app_prefs::refresh_cached(self.prefs.clone());
                self.workspace
                    .update(cx, |workspace, _| workspace.apply_assist_prefs());
                self.assist_settings_notice = None;
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    pub(super) fn set_assist_enabled(
        &mut self,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_assist_flag(
            TERMINAL_ASSIST_ENABLED_KEY,
            value,
            window,
            cx,
            |prefs, v| {
                prefs.terminal_assist_enabled = v;
            },
        );
    }

    pub(super) fn set_assist_history(
        &mut self,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_assist_flag(
            TERMINAL_ASSIST_HISTORY_KEY,
            value,
            window,
            cx,
            |prefs, v| {
                prefs.terminal_assist_history = v;
            },
        );
    }

    /// 清除当前目标 / 全部内存历史，结果写在小节里。
    pub(super) fn assist_clear_history(&mut self, all: bool, cx: &mut Context<Self>) {
        let count = self.workspace.update(cx, |workspace, _| {
            if all {
                workspace.clear_history_all()
            } else {
                workspace.clear_history_current()
            }
        });
        self.assist_settings_notice = Some(format!(
            "{}{}{}",
            t(AssistSettingsMsg::ClearedPrefix),
            count,
            t(AssistSettingsMsg::ClearedSuffix)
        ));
        cx.notify();
    }

    fn save_assist_key(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_text(db, app_prefs::TERMINAL_ASSIST_SELECT_KEY, text));
        match result {
            Ok(()) => {
                self.prefs.terminal_assist_select_key = text.to_owned();
                app_prefs::refresh_cached(self.prefs.clone());
                self.assist_settings_notice = Some(
                    t(if text.is_empty() {
                        AssistSettingsMsg::KeyCleared
                    } else {
                        AssistSettingsMsg::KeyBound
                    })
                    .to_owned(),
                );
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    pub(super) fn assist_clear_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.assist_key_capture = false;
        self.save_assist_key("", window, cx);
    }

    pub(super) fn assist_start_key_capture(&mut self, cx: &mut Context<Self>) {
        self.assist_key_capture = true;
        self.assist_settings_notice = None;
        cx.notify();
    }

    pub(super) fn assist_cancel_key_capture(&mut self, cx: &mut Context<Self>) {
        self.assist_key_capture = false;
        cx.notify();
    }

    /// 录入"选择建议"键时的一次按键（窗口级捕获阶段，先于任何别的快捷键）。Esc 取消；
    /// 能绑的组合保存；不能绑的（没有 Ctrl / Alt / Cmd、或是 Esc / Enter / Tab）留在录入状态并说明原因。
    pub(super) fn assist_capture_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            self.assist_cancel_key_capture(cx);
            return;
        }
        // 光是按下修饰键本身不算，等真正的组合。
        if matches!(
            keystroke.key.as_str(),
            "control" | "alt" | "shift" | "platform" | "function"
        ) {
            return;
        }
        match binding_text(keystroke) {
            Some(text) => {
                self.assist_key_capture = false;
                self.save_assist_key(&text, window, cx);
            }
            None => {
                self.assist_settings_notice =
                    Some(t(AssistSettingsMsg::KeyNeedsModifier).to_owned());
                cx.notify();
            }
        }
    }

    /// 设置 → 终端里的「命令建议」小节。
    pub(super) fn assist_settings_card(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let (auto_entity, history_entity) = (entity.clone(), entity.clone());
        let (set_entity, clear_entity, cancel_entity) =
            (entity.clone(), entity.clone(), entity.clone());
        let (current_entity, all_entity) = (entity.clone(), entity.clone());
        let capturing = self.assist_key_capture;
        let bound = self.prefs.terminal_assist_select_key.clone();
        let has_binding = !bound.is_empty();
        let key_shown = if capturing {
            t(AssistSettingsMsg::KeyListening).to_owned()
        } else {
            binding_label(&bound)
        };
        let mut rows: Vec<AnyElement> = vec![
            row(
                t(AssistSettingsMsg::AutoTitle),
                Some(t(AssistSettingsMsg::AutoDescription)),
                Switch::new("assist-auto")
                    .checked(self.prefs.terminal_assist_enabled)
                    .accessibility_label(t(AssistSettingsMsg::AutoTitle))
                    .on_click(move |checked: &bool, window, cx| {
                        let checked = *checked;
                        auto_entity.update(cx, |this, cx| {
                            this.set_assist_enabled(checked, window, cx);
                        });
                    }),
                tokens,
            ),
            row(
                t(AssistSettingsMsg::HistoryTitle),
                Some(t(AssistSettingsMsg::HistoryDescription)),
                Switch::new("assist-history")
                    .checked(self.prefs.terminal_assist_history)
                    .accessibility_label(t(AssistSettingsMsg::HistoryTitle))
                    .on_click(move |checked: &bool, window, cx| {
                        let checked = *checked;
                        history_entity.update(cx, |this, cx| {
                            this.set_assist_history(checked, window, cx);
                        });
                    }),
                tokens,
            ),
            row_with(
                t(AssistSettingsMsg::KeyTitle),
                Some(key_shown.into()),
                h_flex()
                    .gap_1()
                    .when(has_binding && !capturing, |d| {
                        d.child(
                            Button::new("assist-key-clear")
                                .ghost()
                                .compact()
                                .label(t(AssistSettingsMsg::KeyClear))
                                .on_click(move |_, window, cx| {
                                    clear_entity.update(cx, |this, cx| {
                                        this.assist_clear_key(window, cx);
                                    });
                                }),
                        )
                    })
                    .child(if capturing {
                        Button::new("assist-key-cancel")
                            .outline()
                            .compact()
                            .label(t(AssistSettingsMsg::KeyCancel))
                            .on_click(move |_, _, cx| {
                                cancel_entity
                                    .update(cx, |this, cx| this.assist_cancel_key_capture(cx));
                            })
                    } else {
                        Button::new("assist-key-set")
                            .outline()
                            .compact()
                            .label(t(AssistSettingsMsg::KeySet))
                            .on_click(move |_, _, cx| {
                                set_entity.update(cx, |this, cx| this.assist_start_key_capture(cx));
                            })
                    }),
                tokens,
            ),
            note(t(AssistSettingsMsg::KeyDescription), tokens).into_any_element(),
            row(
                t(AssistSettingsMsg::ClearTitle),
                Some(t(AssistSettingsMsg::ClearDescription)),
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("assist-clear-current")
                            .outline()
                            .compact()
                            .label(t(AssistSettingsMsg::ClearCurrent))
                            .on_click(move |_, _, cx| {
                                current_entity
                                    .update(cx, |this, cx| this.assist_clear_history(false, cx));
                            }),
                    )
                    .child(
                        Button::new("assist-clear-all")
                            .outline()
                            .compact()
                            .label(t(AssistSettingsMsg::ClearAll))
                            .on_click(move |_, _, cx| {
                                all_entity
                                    .update(cx, |this, cx| this.assist_clear_history(true, cx));
                            }),
                    ),
                tokens,
            ),
        ];
        if let Some(text) = &self.assist_settings_notice {
            rows.push(note(text.clone(), tokens).into_any_element());
        }
        card(
            "settings-assist",
            t(AssistSettingsMsg::CardTitle),
            Some(t(AssistSettingsMsg::CardDescription)),
            tokens,
            rows,
        )
    }

    /// 状态栏弹窗里同样两个开关的入口，外加跳到设置的按钮。
    pub(super) fn assist_dialog_toggles(
        &self,
        tokens: &Tokens,
        entity: &Entity<Self>,
    ) -> AnyElement {
        let (auto_entity, history_entity, settings_entity) =
            (entity.clone(), entity.clone(), entity.clone());
        v_flex()
            .gap_1p5()
            .child(row(
                t(AssistSettingsMsg::AutoTitle),
                None,
                Switch::new("assist-dialog-auto")
                    .checked(self.prefs.terminal_assist_enabled)
                    .accessibility_label(t(AssistSettingsMsg::AutoTitle))
                    .on_click(move |checked: &bool, window, cx| {
                        let checked = *checked;
                        auto_entity.update(cx, |this, cx| {
                            this.set_assist_enabled(checked, window, cx);
                        });
                    }),
                tokens,
            ))
            .child(row(
                t(AssistSettingsMsg::HistoryTitle),
                None,
                Switch::new("assist-dialog-history")
                    .checked(self.prefs.terminal_assist_history)
                    .accessibility_label(t(AssistSettingsMsg::HistoryTitle))
                    .on_click(move |checked: &bool, window, cx| {
                        let checked = *checked;
                        history_entity.update(cx, |this, cx| {
                            this.set_assist_history(checked, window, cx);
                        });
                    }),
                tokens,
            ))
            .child(h_flex().justify_end().child(text_button(
                "assist-dialog-settings",
                t(AssistSettingsMsg::OpenSettings),
                ButtonLook::Ghost,
                move |_, window, cx| {
                    settings_entity.update(cx, |this, cx| {
                        this.close_assist_dialog(window, cx);
                        this.open_settings_section(SettingsSection::Terminal, window, cx);
                    });
                },
            )))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{AssistSettingsMsg, binding_label};
    use crate::i18n::Msg as _;

    #[test]
    fn a_stored_binding_reads_like_a_normal_shortcut_and_unbound_says_so() {
        assert_eq!(binding_label("ctrl-alt-s"), "Ctrl+Alt+S");
        assert_eq!(binding_label("ctrl-shift-f5"), "Ctrl+Shift+F5");
        assert_eq!(binding_label("alt-up"), "Alt+Up");
        assert_eq!(binding_label(""), "未绑定");
    }

    #[test]
    fn every_message_has_both_languages_and_the_interception_notice_is_stated() {
        use AssistSettingsMsg::*;
        for msg in [
            CardTitle,
            CardDescription,
            AutoTitle,
            AutoDescription,
            HistoryTitle,
            HistoryDescription,
            KeyTitle,
            KeyDescription,
            KeyUnbound,
            KeySet,
            KeyClear,
            KeyListening,
            KeyCancel,
            KeyNeedsModifier,
            KeyBound,
            KeyCleared,
            ClearTitle,
            ClearDescription,
            ClearCurrent,
            ClearAll,
            ClearedPrefix,
            ClearedSuffix,
            OpenSettings,
        ] {
            assert!(!msg.zh().is_empty(), "{msg:?}");
            assert!(!msg.en().is_empty(), "{msg:?}");
        }
        assert!(KeyDescription.zh().contains("此上下文将拦截该组合"));
        assert!(KeyBound.zh().contains("此上下文将拦截该组合"));
        assert!(KeyDescription.en().contains("will intercept"));
    }
}
