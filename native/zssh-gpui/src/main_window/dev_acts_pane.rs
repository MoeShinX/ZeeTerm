//! 调试驱动里和分屏窗格、回滚行数、外观设置有关的 `act:` 动作（V2-G06x）。只在 debug 构建里编进去；
//! release 里这里只剩两个空函数（驱动脚本本来也只在 debug 里读）。动作都走和界面按钮 / 快捷键同一批
//! 方法；`assert_*` 和 `pane_check` 检查结果，不对就让脚本失败。日志里只写数字和设置值，不写屏幕内容。
//!
//! - `pane_split:x|y`（Ctrl+Shift+D / E）、`pane_focus:<下标>`（点窗格）、`pane_close:<下标>`；
//! - `assert_area:<宽>:<高>`：终端区域是这么大（鼠标坐标按它算，对不上就等到超时失败）；
//! - `pane_report`：把窗格的矩形、行列数和分隔条写进日志；`pane_check`：布局铺满区域、不重叠，
//!   且每个窗格的终端尺寸和它的矩形一致；`assert_pane_focus:<下标>`；
//!   `assert_split_ratio:<第几个分割点>:<最小>:<最大>`；`pane_history:<窗格>:le|ge:<行数>`；
//! - `scrollback:<行数>`（设置页滑块松手）、`assert_scrollback:<行数>`：设置值和所有已开窗格的上限；
//!   `scrollback_between:<下限>:<上限>`：真实鼠标拖滑块之后核对；
//! - `skin:light|dark:<id>`、`terminal_theme:<id>`、`font:<名字>`（下拉里选常见字体）、
//!   `font_custom`（选「自定义…」）、`font_stack:<字体栈>`（在输入框里写）、
//!   `assert_look:<项>:<值>`、`assert_mono:<实际字体名>`。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::{Outcome, log_line};
    use crate::split_tree::Axis;

    const KNOWN: &[&str] = &[
        "pane_split",
        "pane_focus",
        "pane_close",
        "pane_report",
        "pane_check",
        "assert_area",
        "assert_pane_focus",
        "assert_split_ratio",
        "pane_history",
        "scrollback",
        "assert_scrollback",
        "scrollback_between",
        "skin",
        "terminal_theme",
        "font",
        "font_custom",
        "font_stack",
        "assert_look",
        "assert_mono",
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
        let name = spec.split(':').next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        // 字体栈里有逗号和空格，但没有冒号；别的参数最多三段。
        let mut parts = spec.splitn(4, ':');
        let _ = parts.next();
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        let c = parts.next().unwrap_or("");
        Some(run_inner(this, name, [a, b, c], window, cx))
    }

    fn number<T: std::str::FromStr>(raw: &str, what: &str) -> Result<T, String> {
        raw.trim().parse().map_err(|_| format!("{what}不是数字"))
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        [a, b, c]: [&str; 3],
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let ws = this.workspace.clone();
        match name {
            "pane_split" => {
                let axis = match a {
                    "x" | "" => Axis::X,
                    "y" => Axis::Y,
                    _ => return Err("方向只能是 x 或 y".to_owned()),
                };
                if !ws.update(cx, |ws, cx| ws.split_active(axis, cx)) {
                    return Err("没有可拆的窗格".to_owned());
                }
            }
            "pane_focus" => {
                let index: usize = number(a, "窗格下标")?;
                if !ws.update(cx, |ws, cx| ws.focus_pane_index(index, cx)) {
                    return Err("没有这个窗格".to_owned());
                }
            }
            "pane_close" => {
                let index: usize = number(a, "窗格下标")?;
                if !ws.update(cx, |ws, cx| ws.close_pane_index(index, cx)) {
                    return Err("关不了这个窗格（只剩一个或下标越界）".to_owned());
                }
            }
            "pane_report" => {
                let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
                for line in ws.read(cx).pane_report() {
                    log_line(&log, &format!("report {line}"));
                }
            }
            "pane_check" => {
                // 窗格尺寸是下一帧才重排的：没对上先等一拍，别误判。
                if ws.read(cx).check_layout().is_err() {
                    return Ok(Outcome::Pending);
                }
            }
            // assert_area:<宽>:<高>：终端区域是这么大（±1）。鼠标坐标是照它算的：状态栏、侧栏出现
            // 都会让区域变，坐标对不上时让脚本失败，而不是点到别的地方。
            "assert_area" => {
                let (want_w, want_h): (f32, f32) = (number(a, "宽")?, number(b, "高")?);
                let Some((_, _, w, h)) = ws.read(cx).area_rect() else {
                    return Ok(Outcome::Pending);
                };
                if (w - want_w).abs() > 1.0 || (h - want_h).abs() > 1.0 {
                    return Ok(Outcome::Pending);
                }
            }
            "assert_pane_focus" => {
                let want: usize = number(a, "窗格下标")?;
                let have = ws.read(cx).focused_pane_index();
                if have != Some(want) {
                    return Err(format!("聚焦的是 {have:?}，不是 {want}"));
                }
            }
            "assert_split_ratio" => {
                let index: usize = number(a, "分割点序号")?;
                let lo: f32 = number(b, "下限")?;
                let hi: f32 = number(c, "上限")?;
                let splits = ws.read(cx).split_ratios();
                let Some((_, _, ratio)) = splits.get(index).copied() else {
                    return Err(format!("只有 {} 个分割点", splits.len()));
                };
                if ratio < lo || ratio > hi {
                    return Err(format!("比例是 {ratio:.3}，不在 {lo}～{hi}"));
                }
            }
            "pane_history" => {
                let index: usize = number(a, "窗格下标")?;
                let limit: usize = number(c, "行数")?;
                let sizes = ws.read(cx).history_sizes();
                let Some(have) = sizes.get(index).copied() else {
                    return Err("没有这个窗格".to_owned());
                };
                let ok = match b {
                    "le" => have <= limit,
                    "ge" => have >= limit,
                    _ => return Err("比较只能是 le 或 ge".to_owned()),
                };
                if !ok {
                    return Ok(Outcome::Pending);
                }
            }
            "scrollback" => {
                let lines: usize = number(a, "行数")?;
                ws.update(cx, |ws, cx| ws.set_scrollback(lines, cx));
                // 设置页的滑块跟着挪到这个值（真实操作里是滑块松手触发 set_scrollback）。
                let slider = ws.read(cx).scrollback_slider().clone();
                slider.update(cx, |state, cx| state.set_value(lines as f32, window, cx));
            }
            "assert_scrollback" => {
                let want: usize = number(a, "行数")?;
                let have = ws.read(cx).settings().scrollback;
                if have != want {
                    return Err(format!("设置里是 {have} 行，不是 {want}"));
                }
                let limits = ws.read(cx).history_limits();
                if let Some(bad) = limits.iter().find(|limit| **limit != want) {
                    return Err(format!("有窗格的上限是 {bad}，不是 {want}"));
                }
            }
            // scrollback_between:<下限>:<上限>：设置值落在这个区间里，且所有已开窗格的上限就是设置值。
            // 真实鼠标拖设置页的滑块之后用它核对（拖到哪儿取决于滑块的像素宽度，所以只给区间）。
            "scrollback_between" => {
                let (lo, hi): (usize, usize) = (number(a, "下限")?, number(b, "上限")?);
                let have = ws.read(cx).settings().scrollback;
                if have < lo || have > hi {
                    return Ok(Outcome::Pending);
                }
                let limits = ws.read(cx).history_limits();
                if limits.iter().any(|limit| *limit != have) {
                    return Err("有窗格的上限和设置值不一致".to_owned());
                }
            }
            "skin" => {
                let dark = match a {
                    "dark" => true,
                    "light" => false,
                    _ => return Err("明暗只能是 light 或 dark".to_owned()),
                };
                ws.update(cx, |ws, cx| ws.set_skin(dark, b, cx));
            }
            "terminal_theme" => {
                ws.update(cx, |ws, cx| ws.set_terminal_theme(a, cx));
            }
            "font" => {
                ws.update(cx, |ws, cx| ws.pick_font(a, window, cx));
            }
            "font_custom" => {
                ws.update(cx, |ws, cx| ws.pick_custom_font(cx));
            }
            "font_stack" => {
                // 和在输入框里打字同一条路：写进输入框，由它的变化事件改设置。
                let stack = ws.read(cx).font_input().clone();
                stack.update(cx, |state, cx| state.set_value(a, window, cx));
            }
            "assert_look" => {
                let s = ws.read(cx).settings().clone();
                let have =
                    match a {
                        "skin_light" => s.skin_light,
                        "skin_dark" => s.skin_dark,
                        "terminal_theme" => s.terminal_theme,
                        "font_family" => s.font_family,
                        "dark" => (if s.dark { "on" } else { "off" }).to_owned(),
                        _ => return Err(
                            "项只能是 skin_light / skin_dark / terminal_theme / font_family / dark"
                                .to_owned(),
                        ),
                    };
                // 值里可能有冒号以外的任何字符；b 和 c 是被冒号切开的两半，拼回去。
                let want = if c.is_empty() {
                    b.to_owned()
                } else {
                    format!("{b}:{c}")
                };
                if have != want {
                    return Err(format!("{a} 现在是 {have}，不是 {want}"));
                }
            }
            "assert_mono" => {
                let have = ws.update(cx, |ws, cx| ws.resolved_font(cx));
                let want = if b.is_empty() {
                    a.to_owned()
                } else {
                    format!("{a}:{b}")
                };
                // `!名字`：实际用的字体不是它（设置里写了本机没有的字体，不能真去用它）。
                match want.strip_prefix('!') {
                    Some(unwanted) => {
                        if have.eq_ignore_ascii_case(unwanted) {
                            return Err(format!("终端用了本机没有的字体 {have}"));
                        }
                    }
                    None => {
                        if !have.eq_ignore_ascii_case(&want) {
                            return Err(format!("终端实际用的字体是 {have}，不是 {want}"));
                        }
                    }
                }
            }
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
