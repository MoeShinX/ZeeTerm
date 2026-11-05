//! 主题与设置：设置页、调试窗口共用的读写入口（从 `terminal_workspace.rs` 拆出来）。
//! 设置是应用级的（`settings_store::AppSettings`），这里的 `self.settings` 只是读缓存；
//! 改设置一律走 `update_settings`（按字段改、不整份回写）。

use super::*;

impl TerminalWorkspace {
    /// 界面语言偏好：写进设置（和别的字段同一条落盘路径）并立刻生效。
    pub(crate) fn set_language(&mut self, pref: crate::i18n::LangPref, cx: &mut Context<Self>) {
        self.update_settings(cx, move |s| s.language = pref.as_str().to_owned());
        crate::i18n::set(
            pref.resolve(zssh_platform_windows::locale::system_is_chinese()),
            cx,
        );
        cx.notify();
    }

    // ---- 主题与设置（外壳的设置页和调试窗口共用） ----
    //
    // 设置是应用级的，只有一份（`settings_store::AppSettings`）：这里的 `self.settings` 只是读缓存，
    // 改设置一律走 `update_settings`（按字段改、不整份回写），别的窗口改了会通过订阅更新这份缓存。
    // 主题的应用、跟随系统的轮询、落盘都不在窗口里做。

    /// 按字段改应用级设置；自己的读缓存马上跟上，字段变化带来的窗口内副作用一并处理。
    /// 返回设置是否真的变了。
    pub(super) fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut UiSettings),
    ) -> bool {
        let store = self.settings_store.clone();
        let changed = store.update(cx, |model, mcx| model.update_with(mcx, edit));
        self.sync_settings(None, cx);
        changed
    }

    /// 应用级设置变了（自己改的，或别的窗口改的）：更新读缓存，处理这个窗口里受影响的东西。
    /// `window` 只有订阅回调里有；拿得到才去同步设置页上的输入框和滑块。
    pub(super) fn sync_settings(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let (latest, revision) = {
            let model = self.settings_store.read(cx);
            (model.settings().clone(), model.revision())
        };
        if revision == self.settings_rev {
            return;
        }
        let before = std::mem::replace(&mut self.settings, latest);
        self.settings_rev = revision;
        self.apply_settings_change(&before, window, cx);
    }

    pub(super) fn apply_settings_change(
        &mut self,
        before: &UiSettings,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let effects =
            crate::settings_store::window_effects(before, &self.settings, &self.highlight.rules);
        if effects.font_family {
            self.font_cache = None;
            if let Some(window) = window.as_deref_mut() {
                let stack = self.settings.font_family.clone();
                // 输入框里已经是这个值（本来就是它触发的修改）就不再设，免得来回触发。
                if self.font_input.read(cx).value().as_ref() != stack.as_str() {
                    self.font_input
                        .update(cx, |state, cx| state.set_value(stack, window, cx));
                }
            }
        }
        if let Some(lines) = effects.scrollback {
            for tab in self.tabs.items_mut() {
                for pane in &mut tab.panes {
                    pane.set_history(lines);
                }
            }
            if let Some(window) = window {
                self.scrollback_slider
                    .update(cx, |state, cx| state.set_value(lines as f32, window, cx));
            }
        }
        // 高亮规则也是应用级的：别的窗口改了，这里换成同一份并重新扫描。自己改的规则
        // 已经先写进 `self.highlight.rules` 了，两边一致就什么都不用做。
        if let Some(incoming) = effects.adopt_rules {
            let top = incoming.iter().map(|rule| rule.id).max().unwrap_or(0) + 1;
            self.highlight.next_id = self.highlight.next_id.max(top);
            self.highlight.rules = incoming;
            self.highlight.bump();
        }
        if effects.release_ime {
            crate::ime::apply(&self.ime_key, false);
        }
        if effects.reset_blink {
            // 刚改完光标要看得见：闪烁相位回到「亮」。
            self.cursor_blink = BlinkClock::new();
        }
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub(crate) fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        let dark = !crate::app_theme::is_dark(cx);
        self.set_theme(dark, cx);
    }

    pub(crate) fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| {
            s.follow_system = false;
            s.dark = dark;
        });
        cx.notify();
    }

    pub(crate) fn set_theme_auto(&mut self, cx: &mut Context<Self>) {
        let system_light = zssh_platform_windows::theme::apps_use_light_theme();
        self.update_settings(cx, |s| {
            s.follow_system = true;
            s.dark = crate::settings::resolve_dark(true, s.dark, system_light);
        });
        cx.notify();
    }

    /// 换皮肤：`dark` 选的是深色那一栏还是浅色那一栏；明暗对不上的 id 不收。
    /// 当前明暗正在用这一栏时界面立刻跟着变，另一栏记下来，切明暗时生效（同 0.2.14）。
    pub(crate) fn set_skin(&mut self, dark: bool, id: &str, cx: &mut Context<Self>) {
        if crate::app_theme::skin(id).is_none_or(|skin| skin.dark != dark) {
            return;
        }
        let id = id.to_owned();
        self.update_settings(cx, move |s| {
            let slot = if dark {
                &mut s.skin_dark
            } else {
                &mut s.skin_light
            };
            *slot = id;
        });
        cx.notify();
    }

    /// 终端配色：`skin`（跟随皮肤）或某个皮肤的 id；别的值不收。
    pub(crate) fn set_terminal_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        if crate::app_theme::normalize_terminal_theme(id) != id
            || self.settings.terminal_theme == id
        {
            return;
        }
        let id = id.to_owned();
        self.update_settings(cx, move |s| s.terminal_theme = id);
        cx.notify();
    }

    /// 设置里的字体栈（输入框每次变化都会走这里）；空的 / 太长的退回默认。
    pub(crate) fn set_font_family(&mut self, text: &str, cx: &mut Context<Self>) {
        let stack = crate::settings::sanitize_font_family(text);
        if stack == self.settings.font_family {
            return;
        }
        self.update_settings(cx, move |s| s.font_family = stack);
        cx.notify();
    }

    /// 下拉里选了一个常见字体：写成「它 + 后备」，并把输入框里的字同步过去。
    pub(crate) fn pick_font(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.font_custom = false;
        let stack = crate::fonts::stack_for_choice(name);
        self.font_input
            .update(cx, |state, cx| state.set_value(stack.clone(), window, cx));
        self.set_font_family(&stack, cx);
        cx.notify();
    }

    /// 下拉里选了「自定义…」：显示输入框，字体栈先不动。
    pub(crate) fn pick_custom_font(&mut self, cx: &mut Context<Self>) {
        self.font_custom = true;
        cx.notify();
    }

    pub(crate) fn font_custom_shown(&self) -> bool {
        self.font_custom || !crate::fonts::is_common(&self.settings.font_family)
    }

    pub(crate) fn font_input(&self) -> &Entity<InputState> {
        &self.font_input
    }

    pub(crate) fn scrollback_slider(&self) -> &Entity<SliderState> {
        &self.scrollback_slider
    }

    /// 终端实际用的字体名：设置里的字体栈里第一个本机装了的，一个都没有就用主题自带的等宽字体。
    pub(super) fn mono_family(&mut self, cx: &App) -> SharedString {
        if let Some((stack, family)) = &self.font_cache
            && *stack == self.settings.font_family
        {
            return family.clone();
        }
        let installed = crate::fonts::installed_fonts(cx);
        let theme_mono = cx.theme().mono_font_family.to_string();
        let family: SharedString =
            crate::fonts::resolve_family(&self.settings.font_family, installed, &theme_mono).into();
        self.font_cache = Some((self.settings.font_family.clone(), family.clone()));
        family
    }

    /// 回滚行数（1000～100000）：所有标签里已经开着的窗格立刻按新上限（调小会丢掉最旧的行，
    /// 调大只是以后能攒更多，同 xterm 的 `scrollback` 选项），之后新开的窗格也用它。
    /// 已开窗格的调整在 `apply_settings_change` 里做，所有窗口都一样。
    pub(crate) fn set_scrollback(&mut self, lines: usize, cx: &mut Context<Self>) {
        let lines = lines.clamp(MIN_SCROLLBACK, MAX_SCROLLBACK);
        if lines == self.settings.scrollback {
            return;
        }
        self.update_settings(cx, move |s| s.scrollback = lines);
        cx.notify();
    }

    pub(crate) fn adjust_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.update_settings(cx, move |s| {
            s.font_size = (s.font_size + delta).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        });
        cx.notify();
    }

    pub(crate) fn adjust_line_height(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.update_settings(cx, move |s| {
            let next = ((s.line_height + delta) * 100.0).round() / 100.0;
            s.line_height = next.clamp(MIN_LINE_HEIGHT, MAX_LINE_HEIGHT);
        });
        cx.notify();
    }

    pub(crate) fn set_cursor_shape(&mut self, shape: CursorShape, cx: &mut Context<Self>) {
        self.update_settings(cx, move |s| s.cursor = shape);
        cx.notify();
    }

    pub(crate) fn toggle_cursor_blink(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.cursor_blink = !s.cursor_blink);
        cx.notify();
    }

    /// 「在终端右上角显示延迟」：关着时窗格也不再登记成看得见，后台测量随之暂停。
    pub(crate) fn toggle_show_latency(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.show_latency = !s.show_latency);
        cx.notify();
    }

    /// 「终端里自动切到英文输入」：关掉时焦点进终端不再动输入法；已经在英文区的话马上还回去
    /// （在 `apply_settings_change` 里做）。
    pub(crate) fn toggle_ime_english(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.ime_english = !s.ime_english);
        cx.notify();
    }

    pub(crate) fn toggle_copy_on_select(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.copy_on_select = !s.copy_on_select);
        cx.notify();
    }

    pub(crate) fn toggle_right_click_paste(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.right_click_paste = !s.right_click_paste);
        cx.notify();
    }

    pub(crate) fn toggle_confirm_multiline(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.confirm_multiline = !s.confirm_multiline);
        cx.notify();
    }

    pub(crate) fn toggle_highlight_enabled(&mut self, cx: &mut Context<Self>) {
        let enable = !self.settings.highlight_enabled;
        self.update_settings(cx, move |s| s.highlight_enabled = enable);
        if !enable {
            self.highlight.scheduler.forget();
        } else if self.highlight.rules.is_empty() {
            // 打开时一条规则都没有（用户全删了）：补上常用规则，同 0.2.14，免得开了开关什么也不标。
            self.add_sample_highlight_rules(cx);
        }
        cx.notify();
    }

    pub(crate) fn cycle_bracketed_paste(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.bracketed_paste = s.bracketed_paste.next());
        cx.notify();
    }

    pub(crate) fn toggle_start_hidden(&mut self, cx: &mut Context<Self>) {
        self.update_settings(cx, |s| s.start_hidden = !s.start_hidden);
        cx.notify();
    }

    pub(crate) fn reset_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 高亮规则是用户自己的数据，恢复默认设置不动它。
        let rules = self.highlight.rules.clone();
        self.update_settings(cx, move |s| {
            *s = UiSettings::default();
            s.highlight_rules = Some(rules);
        });
        self.font_cache = None;
        self.font_custom = false;
        let stack = self.settings.font_family.clone();
        let lines = self.settings.scrollback;
        self.font_input
            .update(cx, |state, cx| state.set_value(stack, window, cx));
        self.scrollback_slider
            .update(cx, |state, cx| state.set_value(lines as f32, window, cx));
        cx.notify();
    }
}
