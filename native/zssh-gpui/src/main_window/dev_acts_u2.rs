//! 调试驱动里 V2-U2（设置页左侧分类导航）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里这里只剩两个空函数。动作都走和界面控件同一批方法，`*_assert` 不对就让脚本失败；
//! `pref_assert` 读的是**落盘**的值（重新开库读），不是界面上缓存的。
//!
//! - `settings_section:<分类 id>`：打开设置页并切到这个分类（`appearance` / `terminal` / ...）；
//!   `settings_assert:section:<分类 id>`：现在显示的是这个分类；
//! - `pref_set:<键>:<值>`、`pref_assert:<键>:<值>`：连接 / SFTP / 窗口 / 会话日志这些数据库设置，
//!   键见 `settings_page::dev_set_pref`；
//! - `ui_set:<项>:<值>`、`ui_assert:<项>:<值>`：外观 / 终端这些 `ui-settings.json` 里的设置
//!   （`cursor` `cursor_blink` `ime_english` `highlight` `copy_on_select` `start_hidden`）；
//! - `hl_type:<第几条>:<文字>`（在规则输入框里打字并提交）、`hl_op:<操作>`（`add` `samples` `remove:<i>`
//!   `toggle:<i>` `regex:<i>:on|off` `case:<i>:on|off` `bg:<i>:on|off` `color:<i>:<十六进制>`）、
//!   `hl_assert:count:<n>` / `hl_assert:pattern:<i>:<文字>` / `hl_assert:flag:<i>:<enabled|regex|case|bg>:<on|off>` /
//!   `hl_assert:color:<i>:<十六进制>`。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use crate::settings::CursorShape;
    use crate::settings_nav::SettingsSection;

    const KNOWN: &[&str] = &[
        "settings_section",
        "settings_assert",
        "pref_set",
        "pref_assert",
        "ui_set",
        "ui_assert",
        "hl_type",
        "hl_op",
        "hl_assert",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(run_inner(this, name, a, b, window, cx))
    }

    fn on_off(value: &str) -> Result<bool, String> {
        match value {
            "on" | "true" => Ok(true),
            "off" | "false" => Ok(false),
            _ => Err("值要是 on / off".to_owned()),
        }
    }

    fn index(value: &str) -> Result<usize, String> {
        value
            .parse::<usize>()
            .map_err(|_| format!("{value} 不是规则下标"))
    }

    fn check(ok: bool, what: &str) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else {
            Err(format!("断言没有成立：{what}"))
        }
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        a: &str,
        b: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "settings_section" => {
                let section = SettingsSection::parse(a).ok_or_else(|| format!("没有分类 {a}"))?;
                this.show_view(super::super::nav::MainView::Settings, window, cx);
                this.set_settings_section(section, cx);
                Ok(Outcome::Done)
            }
            "settings_assert" => match a {
                "section" => check(
                    this.settings_section.id() == b,
                    &format!("现在是 {}，要 {b}", this.settings_section.id()),
                ),
                _ => Err("settings_assert:section:<id>".to_owned()),
            },
            "pref_set" => {
                this.dev_set_pref(a, b, window, cx)?;
                Ok(Outcome::Done)
            }
            "pref_assert" => {
                let actual = this.dev_pref_text(a)?;
                check(actual == b, &format!("{a} 落盘的是 {actual}，要 {b}"))
            }
            "ui_set" => {
                let ws = this.workspace.clone();
                match a {
                    "cursor" => {
                        let shape = match b {
                            "block" => CursorShape::Block,
                            "underline" => CursorShape::Underline,
                            "bar" => CursorShape::Bar,
                            _ => return Err("cursor 只能是 block / underline / bar".to_owned()),
                        };
                        ws.update(cx, |ws, cx| ws.set_cursor_shape(shape, cx));
                    }
                    "cursor_blink" => {
                        let want = on_off(b)?;
                        ws.update(cx, |ws, cx| {
                            if ws.settings().cursor_blink != want {
                                ws.toggle_cursor_blink(cx);
                            }
                        });
                    }
                    "ime_english" => {
                        let want = on_off(b)?;
                        ws.update(cx, |ws, cx| {
                            if ws.settings().ime_english != want {
                                ws.toggle_ime_english(cx);
                            }
                        });
                    }
                    "highlight" => {
                        let want = on_off(b)?;
                        ws.update(cx, |ws, cx| {
                            if ws.settings().highlight_enabled != want {
                                ws.toggle_highlight_enabled(cx);
                            }
                        });
                    }
                    "copy_on_select" => {
                        let want = on_off(b)?;
                        ws.update(cx, |ws, cx| {
                            if ws.settings().copy_on_select != want {
                                ws.toggle_copy_on_select(cx);
                            }
                        });
                    }
                    "start_hidden" => {
                        let want = on_off(b)?;
                        ws.update(cx, |ws, cx| {
                            if ws.settings().start_hidden != want {
                                ws.toggle_start_hidden(cx);
                            }
                        });
                    }
                    _ => return Err(format!("不认识的外观 / 终端设置 {a}")),
                }
                Ok(Outcome::Done)
            }
            "ui_assert" => {
                // 断言落盘的值：重新读 ui-settings.json。
                let saved = crate::settings::UiSettings::load();
                let actual = match a {
                    "cursor" => match saved.cursor {
                        CursorShape::Block => "block",
                        CursorShape::Underline => "underline",
                        CursorShape::Bar => "bar",
                    }
                    .to_owned(),
                    "cursor_blink" => on_off_text(saved.cursor_blink),
                    "ime_english" => on_off_text(saved.ime_english),
                    "highlight" => on_off_text(saved.highlight_enabled),
                    "copy_on_select" => on_off_text(saved.copy_on_select),
                    "start_hidden" => on_off_text(saved.start_hidden),
                    _ => return Err(format!("不认识的外观 / 终端设置 {a}")),
                };
                check(actual == b, &format!("{a} 落盘的是 {actual}，要 {b}"))
            }
            "hl_type" => {
                let Some(editor) = this.highlight_editor.clone() else {
                    return Ok(Outcome::Pending);
                };
                let at = index(a)?;
                editor.update(cx, |editor, cx| editor.dev_type_pattern(at, b, window, cx))?;
                Ok(Outcome::Done)
            }
            "hl_op" => {
                let ws = this.workspace.clone();
                let mut parts = b.split(':');
                let arg1 = parts.next().unwrap_or("");
                let arg2 = parts.next().unwrap_or("");
                let id_of = |cx: &mut Context<MainWindow>, at: usize| -> Result<u64, String> {
                    ws.read(cx)
                        .highlight_rules()
                        .get(at)
                        .map(|rule| rule.id)
                        .ok_or_else(|| format!("没有第 {at} 条规则"))
                };
                match a {
                    "add" => {
                        ws.update(cx, |ws, cx| ws.add_blank_highlight_rule(cx))
                            .ok_or("规则已满")?;
                    }
                    "samples" => {
                        ws.update(cx, |ws, cx| ws.add_sample_highlight_rules(cx));
                    }
                    "remove" => {
                        let id = id_of(cx, index(arg1)?)?;
                        ws.update(cx, |ws, cx| ws.remove_highlight_rule(id, cx));
                    }
                    "toggle" => {
                        let id = id_of(cx, index(arg1)?)?;
                        ws.update(cx, |ws, cx| ws.toggle_highlight_rule(id, cx));
                    }
                    "regex" => {
                        let (id, on) = (id_of(cx, index(arg1)?)?, on_off(arg2)?);
                        ws.update(cx, |ws, cx| ws.set_highlight_regex(id, on, cx));
                    }
                    "case" => {
                        let (id, on) = (id_of(cx, index(arg1)?)?, on_off(arg2)?);
                        ws.update(cx, |ws, cx| ws.set_highlight_case(id, on, cx));
                    }
                    "bg" => {
                        let (id, on) = (id_of(cx, index(arg1)?)?, on_off(arg2)?);
                        ws.update(cx, |ws, cx| ws.set_highlight_background(id, on, cx));
                    }
                    "color" => {
                        let id = id_of(cx, index(arg1)?)?;
                        let color = u32::from_str_radix(arg2.trim_start_matches('#'), 16)
                            .map_err(|_| "颜色要是十六进制")?;
                        ws.update(cx, |ws, cx| ws.set_highlight_color(id, color, cx));
                    }
                    _ => {
                        return Err(
                            "hl_op:add|samples|remove|toggle|regex|case|bg|color".to_owned()
                        );
                    }
                }
                Ok(Outcome::Done)
            }
            "hl_assert" => {
                // 断言落盘的规则：重新读 ui-settings.json。
                let saved = crate::settings::UiSettings::load();
                let rules = saved
                    .highlight_rules
                    .unwrap_or_else(crate::highlight::default_rules);
                let mut parts = b.split(':');
                let (arg1, arg2, arg3) = (
                    parts.next().unwrap_or(""),
                    parts.next().unwrap_or(""),
                    parts.next().unwrap_or(""),
                );
                match a {
                    "count" => check(
                        rules.len().to_string() == arg1,
                        &format!("规则 {} 条，要 {arg1}", rules.len()),
                    ),
                    "pattern" => {
                        let rule = rules.get(index(arg1)?).ok_or("没有这条规则")?;
                        // 模式里可能有冒号：第二段之后拼回去。
                        let want = if arg3.is_empty() {
                            arg2.to_owned()
                        } else {
                            format!("{arg2}:{arg3}")
                        };
                        check(rule.pattern == want, &format!("模式是 {}", rule.pattern))
                    }
                    "flag" => {
                        let rule = rules.get(index(arg1)?).ok_or("没有这条规则")?;
                        let actual = match arg2 {
                            "enabled" => rule.enabled,
                            "regex" => rule.regex,
                            "case" => rule.case_sensitive,
                            "bg" => rule.background.is_some(),
                            _ => return Err("flag 只能是 enabled / regex / case / bg".to_owned()),
                        };
                        check(on_off_text(actual) == arg3, &format!("{arg2} 是 {actual}"))
                    }
                    "color" => {
                        let rule = rules.get(index(arg1)?).ok_or("没有这条规则")?;
                        let want = u32::from_str_radix(arg2.trim_start_matches('#'), 16)
                            .map_err(|_| "颜色要是十六进制")?;
                        check(
                            rule.color() == want,
                            &format!("颜色是 {:06x}", rule.color()),
                        )
                    }
                    _ => Err("hl_assert:count|pattern|flag|color".to_owned()),
                }
            }
            _ => Err("还没有实现".to_owned()),
        }
    }

    fn on_off_text(value: bool) -> String {
        if value { "on" } else { "off" }.to_owned()
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
