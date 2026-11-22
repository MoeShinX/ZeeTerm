//! 调试驱动里和同步、备份、服务器状态有关的 `act:` 动作（G11）。动作的实现在 `real.rs`，只在 debug 构建里
//! 编进去（要用本机桩和调试入口）；release 里这里只剩两个空函数，驱动脚本本来也只在 debug 里读。

#[cfg(debug_assertions)]
mod real;

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
