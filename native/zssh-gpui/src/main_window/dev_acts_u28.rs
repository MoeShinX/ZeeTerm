//! 调试驱动里 V2-U28（拖动时靠近边缘自动滚动）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里只剩两个空函数。拖动本身用 `down` / `move` / `up` 步骤走真实的鼠标分发路径，这里只管核对结果：
//!
//! - `assert_scroll:tree|page:top|moved|bottom`：侧栏树 / 主机页现在在顶部、已经离开顶部、滚到了底；
//! - `assert_scroll_idle`：自动滚动的帧回调已经结束（没有常驻）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;

    const KNOWN: &[&str] = &["assert_scroll", "assert_scroll_idle"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        _window: &mut Window,
        _cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
        if !is_known(name) {
            return None;
        }
        Some(match name {
            "assert_scroll_idle" => {
                if this.drag_scroll.running() {
                    Err("自动滚动的帧回调还在跑".to_owned())
                } else {
                    Ok(Outcome::Done)
                }
            }
            _ => {
                let Some((which, want)) = arg.split_once(':') else {
                    return Some(Err(
                        "写成 assert_scroll:tree|page:top|moved|bottom".to_owned()
                    ));
                };
                let handle = match which {
                    "tree" => &this.drag_scroll.tree,
                    "page" => &this.drag_scroll.page,
                    _ => return Some(Err("滚动区只能是 tree 或 page".to_owned())),
                };
                let offset = f32::from(handle.offset().y);
                let max = f32::from(handle.max_offset().y);
                let state = if offset >= -0.5 {
                    "top"
                } else if max + offset <= 0.5 {
                    "bottom"
                } else {
                    "moved"
                };
                // "moved" 是"离开了顶部"，滚到底也算。
                if state == want || (want == "moved" && state == "bottom") {
                    Ok(Outcome::Done)
                } else {
                    Err(format!(
                        "断言没有成立：{which} 现在是 {state}（偏移 {offset:.0} / 最大 {max:.0}）"
                    ))
                }
            }
        })
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
