//! 调试驱动里 V2-U1（用户手测第一轮反馈）要用的 `act:` 动作。只在 debug 构建里编进去；release 里
//! 这里只剩两个空函数。动作都走和界面按钮 / 右键菜单同一批方法，`assert_*` 不对就让脚本失败。
//!
//! - `tab_disconnect:<标签下标>`（标签右键「断开连接」）；
//! - `vault_focus:<第几张>|none|search`（主机页键盘焦点，V2-U1 第 5 条）、`vault_focus:set:<第几张>`；
//! - `tab_strip:<子命令>`（标签条溢出，V2-U1 第 4 条）：`hidden:<最少>:<最多>` 露不全的标签个数、
//!   `active_visible` 当前标签完整可见、`active:<下标>` 当前终端标签、`tab_width:<最小>:<最大>` 第一个标签的宽度、`offset:<最小>:<最大>`
//!   横向滚动位置（正数写成滚了多少）、`wheel:<竖向>` 滚轮（正数向下）、`menu:open|close`、`menu_len:<n>`、
//!   `menu_select:<第几个>`、`menu_close:<第几个>`、`menu_open:yes|no`；条件还没成立就等下一帧。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;

    const KNOWN: &[&str] = &[
        "tab_disconnect",
        "tab_strip",
        "vault_focus",
        "identity",
        "modal_has",
        "term_window",
        "tab_drag",
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
        let mut parts = spec.splitn(4, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        let c = parts.next().unwrap_or("");
        Some(run_inner(this, name, [a, b, c], window, cx))
    }

    fn tab_strip(
        this: &mut MainWindow,
        sub: &str,
        b: &str,
        c: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let infos = this.workspace.read(cx).tab_infos();
        let entries = this.tab_entries(&infos, cx);
        let in_range = |value: f32, lo: &str, hi: &str| -> Result<bool, String> {
            let (lo, hi): (f32, f32) = (number(lo, "下限")?, number(hi, "上限")?);
            Ok((lo..=hi).contains(&value))
        };
        match sub {
            "hidden" => {
                if !in_range(this.tab_strip.hidden as f32, b, c)? {
                    return Ok(Outcome::Pending);
                }
            }
            "active_visible" => {
                let active_term = this.workspace.read(cx).active_index();
                let Some((_, index)) = this.active_entry(&entries, &infos, active_term) else {
                    return Err("现在没有高亮的标签".to_owned());
                };
                let scroll = &this.tab_strip.scroll;
                let (Some(item), viewport) = (scroll.bounds_for_item(index), scroll.bounds())
                else {
                    return Ok(Outcome::Pending);
                };
                let offset = scroll.offset().x;
                let left = f32::from(item.left() + offset);
                let right = f32::from(item.right() + offset);
                if left < f32::from(viewport.left()) - 1.0
                    || right > f32::from(viewport.right()) + 1.0
                {
                    return Ok(Outcome::Pending);
                }
            }
            "report" => {
                let scroll = &this.tab_strip.scroll;
                let viewport = scroll.bounds();
                let last = entries.len().saturating_sub(1);
                let item = scroll.bounds_for_item(last);
                let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
                super::super::dev_script::log_line(
                    &log,
                    &format!(
                        "tab_strip viewport={:?} offset={:?} max={:?} children={} last={:?} hidden={}",
                        viewport,
                        scroll.offset(),
                        scroll.max_offset(),
                        scroll.children_count(),
                        item,
                        this.tab_strip.hidden
                    ),
                );
            }
            "active" => {
                let want: usize = number(b, "标签下标")?;
                let have = this.workspace.read(cx).active_index();
                if have != Some(want) {
                    return Ok(Outcome::Pending);
                }
            }
            "tab_width" => {
                let Some(item) = this.tab_strip.scroll.bounds_for_item(0) else {
                    return Ok(Outcome::Pending);
                };
                if !in_range(f32::from(item.size.width), b, c)? {
                    return Ok(Outcome::Pending);
                }
            }
            "offset" => {
                let scrolled = -f32::from(this.tab_strip.scroll.offset().x);
                if !in_range(scrolled, b, c)? {
                    return Ok(Outcome::Pending);
                }
            }
            "wheel" => {
                let dy: f32 = number(b, "滚动量")?;
                // 正数 = 滚轮向下 = GPUI 的滚动量为负。
                if !this.tab_strip.wheel(0.0, -dy) {
                    return Err("没有滚动（已经到头，或标签条放得下）".to_owned());
                }
            }
            "menu" => match b {
                "open" => this.tab_strip.menu_open = true,
                "close" => this.tab_strip.menu_open = false,
                _ => return Err("menu 只能是 open 或 close".to_owned()),
            },
            "menu_open" => {
                let want = match b {
                    "yes" => true,
                    "no" => false,
                    _ => return Err("参数只能是 yes 或 no".to_owned()),
                };
                if this.tab_strip.menu_open != want {
                    return Ok(Outcome::Pending);
                }
            }
            "menu_len" => {
                let want: usize = number(b, "个数")?;
                if entries.len() != want {
                    return Err(format!("标签条里有 {} 个标签，不是 {want}", entries.len()));
                }
            }
            "menu_select" | "menu_close" => {
                let nth: usize = number(b, "第几个")?;
                let Some(entry) = entries.get(nth) else {
                    return Err("没有这个标签".to_owned());
                };
                let key = entry.key;
                if sub == "menu_select" {
                    this.select_tab_entry(key, window, cx);
                } else {
                    this.close_tab_entry(key, window, cx);
                }
            }
            _ => return Err("tab_strip 的子命令不认识".to_owned()),
        }
        cx.notify();
        Ok(Outcome::Done)
    }

    fn term_window_act(
        this: &mut MainWindow,
        sub: &str,
        b: &str,
        c: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let handles = crate::term_window::handles(cx);
        let pending = |ok: bool| {
            if ok {
                Ok(Outcome::Done)
            } else {
                Ok(Outcome::Pending)
            }
        };
        match sub {
            // pop：把主窗口当前标签弹出成独立终端窗口（和拖出标签同一条路，会话不断）。
            "pop" => {
                let Some(tab_id) =
                    this.workspace.read(cx).active_index().and_then(|index| {
                        this.workspace.read(cx).tab_infos().get(index).map(|i| i.id)
                    })
                else {
                    return Err("主窗口没有终端标签可以弹出".to_owned());
                };
                let Some((index, tab)) = this
                    .workspace
                    .update(cx, |ws, cx| ws.take_tab_by_id(tab_id, cx))
                else {
                    return Err("取不出这个标签".to_owned());
                };
                match crate::term_window::open(cx, tab, None) {
                    Ok(_) => this.after_tabs_closed(window, cx),
                    Err(tab) => this
                        .workspace
                        .update(cx, |ws, cx| ws.restore_tab(index, tab, cx)),
                }
                Ok(Outcome::Done)
            }
            "count" => {
                let want: usize = number(b, "个数")?;
                pending(handles.len() == want)
            }
            "windows" => {
                let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
                for (_, rect) in crate::tab_drag::other_windows(cx, ::gpui::WindowId::from(0)) {
                    super::super::dev_script::log_line(&log, &format!("window rect={rect:?}"));
                }
                Ok(Outcome::Done)
            }
            "bounds" => {
                let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
                for handle in &handles {
                    let bounds = handle.update(cx, |_, window, _| window.bounds());
                    super::super::dev_script::log_line(
                        &log,
                        &format!("term_window bounds={bounds:?}"),
                    );
                }
                Ok(Outcome::Done)
            }
            _ => {
                // 子命令前面加 `last_` 就对最后一个独立窗口操作（默认是第一个）。
                let (sub, handle) = match sub.strip_prefix("last_") {
                    Some(rest) => (rest, handles.last().copied()),
                    None => (sub, handles.first().copied()),
                };
                let Some(handle) = handle else {
                    return Ok(Outcome::Pending);
                };
                match sub {
                    "tabs" => {
                        let want: usize = number(b, "个数")?;
                        let have = handle
                            .read_with(cx, |view, cx| view.tab_count(cx))
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        pending(have == want)
                    }
                    // resize:<宽>:<高>：把第一个独立终端窗口调成这个大小（看窄窗口的标题栏用）
                    "resize" => {
                        let width: f32 = number(b, "宽")?;
                        let height: f32 = number(c, "高")?;
                        handle
                            .update(cx, |_, window, _| {
                                window.resize(::gpui::size(::gpui::px(width), ::gpui::px(height)));
                            })
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        Ok(Outcome::Done)
                    }
                    // set:theme:dark|light|auto / set:font:<增量> / set:rule:<模式> / set:highlight:on|off：
                    //   在这个独立窗口的工作区里改设置（和它设置页的按钮同一批方法）。
                    "set" => {
                        let value = c;
                        let workspace = handle
                            .read_with(cx, |view, _| view.dev_workspace())
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        match b {
                            "theme" => workspace.update(cx, |ws, cx| match value {
                                "dark" => ws.set_theme(true, cx),
                                "light" => ws.set_theme(false, cx),
                                _ => ws.set_theme_auto(cx),
                            }),
                            "font" => {
                                let delta: f32 = number(value, "增量")?;
                                workspace.update(cx, |ws, cx| ws.adjust_font(delta, cx));
                            }
                            "rule" => {
                                let pattern = value.to_owned();
                                workspace
                                    .update(cx, |ws, cx| ws.add_highlight_rule(pattern, false, cx));
                            }
                            "highlight" => {
                                let want = value == "on";
                                workspace.update(cx, |ws, cx| {
                                    if ws.settings().highlight_enabled != want {
                                        ws.toggle_highlight_enabled(cx);
                                    }
                                });
                            }
                            _ => {
                                return Err("set 只能是 theme / font / rule / highlight".to_owned());
                            }
                        }
                        Ok(Outcome::Done)
                    }
                    // mem:<字段>:<值>：这个窗口手里的设置读缓存（不读盘）是不是这个值；还没到就等下一帧。
                    "mem" => {
                        let workspace = handle
                            .read_with(cx, |view, _| view.dev_workspace())
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        let have = workspace.read(cx).dev_settings_text(b, cx)?;
                        pending(have == c)
                    }
                    "type" => {
                        let text = b.replace(r"\r", "\r");
                        handle
                            .update(cx, |view, _window, cx| view.dev_type(&text, cx))
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        Ok(Outcome::Done)
                    }
                    "expect" => {
                        let text = if c.is_empty() {
                            b.to_owned()
                        } else {
                            format!("{b}:{c}")
                        };
                        let found = handle
                            .read_with(cx, |view, cx| view.dev_has_text(&text, cx))
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        pending(found)
                    }
                    "status" => {
                        let index: usize = number(b, "标签下标")?;
                        let want = match c {
                            "connected" => crate::terminal_workspace::TabStatus::Connected,
                            "disconnected" => crate::terminal_workspace::TabStatus::Disconnected,
                            _ => return Err("状态只能是 connected / disconnected".to_owned()),
                        };
                        let have = handle
                            .read_with(cx, |view, cx| view.dev_statuses(cx))
                            .map_err(|_| "窗口已经关闭".to_owned())?;
                        pending(have.get(index) == Some(&want))
                    }
                    _ => Err("term_window 的子命令不认识".to_owned()),
                }
            }
        }
    }

    fn number<T: std::str::FromStr>(raw: &str, what: &str) -> Result<T, String> {
        raw.trim().parse().map_err(|_| format!("{what}不是数字"))
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        [a, _b, _c]: [&str; 3],
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "tab_disconnect" => {
                let index: usize = number(a, "标签下标")?;
                let can = this
                    .workspace
                    .read(cx)
                    .tab_infos()
                    .get(index)
                    .is_some_and(|info| info.can_disconnect);
                if !can {
                    return Err("这个标签现在不能断开（没有连着的会话）".to_owned());
                }
                this.disconnect_tab(index, window, cx);
            }
            // vault_focus:<第几张> / none / search：键盘焦点在哪（还没到就等下一帧）；
            // vault_focus:set:<第几张> 把焦点放过去（和 Tab 同一个方法）。
            "vault_focus" => {
                if a == "report" {
                    let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
                    let search = this
                        .vault_query
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window);
                    let any = window.focused(cx).is_some();
                    super::super::dev_script::log_line(
                        &log,
                        &format!(
                            "vault_focus card={:?} search={search} any_focus={any} focused={:?} first_card={:?}",
                            this.focused_host_index(window),
                            window.focused(cx),
                            this.host_focus.values().next()
                        ),
                    );
                    return Ok(Outcome::Done);
                }
                if a == "set" {
                    let index: usize = number(_b, "第几张")?;
                    if !this.focus_host_index(index, window, cx) {
                        return Err("没有这张卡片".to_owned());
                    }
                    return Ok(Outcome::Done);
                }
                let current = this.focused_host_index(window);
                let search = this
                    .vault_query
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window);
                let ok = match a {
                    "none" => current.is_none() && !search,
                    "search" => search,
                    index => current == Some(number(index, "第几张")?),
                };
                if !ok {
                    return Ok(Outcome::Pending);
                }
            }
            // identity:has:<名字> / no:<名字> / use:<名字>（主机表单选用这条凭据）。
            "identity" => {
                let find = |this: &MainWindow, name: &str| {
                    this.store
                        .identities()
                        .iter()
                        .find(|item| item.name == name)
                        .map(|item| item.id)
                };
                match (a, find(this, _b)) {
                    ("has", Some(_)) | ("no", None) => {}
                    ("has", None) => return Err(format!("身份库里没有「{}」", _b)),
                    ("no", Some(_)) => return Err(format!("身份库里还有「{}」", _b)),
                    ("use", Some(id)) => this.host_set_identity(Some(id), window, cx),
                    ("use", None) => return Err(format!("身份库里没有「{}」", _b)),
                    _ => return Err("identity 的子命令不认识".to_owned()),
                }
            }
            // modal_has:<文字>：现在的确认框正文里有这段话。
            "modal_has" => {
                let text = [a, _b, _c]
                    .iter()
                    .filter(|part| !part.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(":");
                match &this.modal {
                    Some(super::super::modal::Modal::Confirm(confirm))
                        if confirm.message.contains(&text) => {}
                    _ => return Err("确认框里没有这段话".to_owned()),
                }
            }
            // term_window:count:<n> 独立终端窗口个数 / tabs:<n> 第一个窗口里的标签数 / type:<文字> 往它当前标签打字 /
            //   expect:<文字> 它当前标签屏幕上有这段话 / status:<标签下标>:<connected|disconnected> / bounds 写进日志。
            "term_window" => return term_window_act(this, a, _b, _c, window, cx),
            // tab_drag:idle 没有拖动在进行 / tab_drag:active 有 / tab_drag:spans[:sftp|files|term] 把标签现在画出来的
            //   左右边界写进日志（拖动换位的截图脚本据此算按下 / 移动的坐标）：不写是主窗口的终端标签，`sftp` 是主窗口的
            //   文件标签，`files` 是文件窗口的标签，`term` 是第一个独立终端窗口的标签。
            "tab_drag" if a == "spans" => {
                use crate::tab_drag::DragKey;
                let spans = match _b {
                    "" => {
                        let keys: Vec<DragKey> = this
                            .workspace
                            .read(cx)
                            .tab_ids()
                            .into_iter()
                            .map(DragKey::Terminal)
                            .collect();
                        this.tab_dragger.spans().collect(&keys)
                    }
                    "sftp" => {
                        let keys: Vec<DragKey> = this
                            .docks
                            .sftp_tabs
                            .iter()
                            .map(|tab| DragKey::Sftp(tab.id))
                            .collect();
                        this.tab_dragger.spans().collect(&keys)
                    }
                    "files" => crate::file_window::window_handle(cx)
                        .and_then(|handle| {
                            handle.read_with(cx, |files, _| files.dev_tab_spans()).ok()
                        })
                        .flatten(),
                    "term" => crate::term_window::handles(cx)
                        .first()
                        .copied()
                        .and_then(|handle| {
                            handle.read_with(cx, |term, cx| term.dev_tab_spans(cx)).ok()
                        })
                        .flatten(),
                    _ => return Err("tab_drag:spans 后面只能是 sftp / files / term".to_owned()),
                };
                let Some(spans) = spans else {
                    return Ok(Outcome::Pending);
                };
                let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
                let text: Vec<String> = spans
                    .iter()
                    .map(|(left, right)| format!("{left:.0}-{right:.0}"))
                    .collect();
                super::super::dev_script::log_line(
                    &log,
                    &format!("tab_spans[{_b}] [{}]", text.join(", ")),
                );
            }
            "tab_drag" => {
                let active = crate::tab_drag::current(cx).is_some();
                let want = match a {
                    "idle" => false,
                    "active" => true,
                    _ => return Err("tab_drag 只能是 idle 或 active".to_owned()),
                };
                if active != want {
                    return Ok(Outcome::Pending);
                }
            }
            "tab_strip" => return tab_strip(this, a, _b, _c, window, cx),
            _ => return Err("还没有实现".to_owned()),
        }
        cx.notify();
        Ok(Outcome::Done)
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
