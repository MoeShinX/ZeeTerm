//! 调试驱动里 V2-U6（标题栏精简）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩两个空函数。
//!
//! - `assert_u6:left:<id,id,...>`：标题栏左侧块现在的图标（`sidebar` `hosts` `ai` `files` `settings`，
//!   从左到右）就是这一串；
//! - `assert_u6:switches:on|off`：状态栏右侧的快捷命令栏 / 撰写栏开关现在有没有；
//! - `assert_u6:statusbar_stats:on|off`：状态栏左侧有没有服务器状态（订阅了主机才有）；
//! - `assert_u6:theme:follow|light|dark`、`assert_u6:skin:<light|dark>:<皮肤 id>`：外观页改过的主题设置。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::titlebar::{LeftIcon, left_icons};

    const KNOWN: &[&str] = &["assert_u6"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    fn label(icon: LeftIcon) -> &'static str {
        match icon {
            LeftIcon::Sidebar => "sidebar",
            LeftIcon::HostPage => "hosts",
            LeftIcon::Ai => "ai",
            LeftIcon::Files => "files",
            LeftIcon::NewHost => "new_host",
            LeftIcon::NewGroup => "new_group",
            LeftIcon::Settings => "settings",
        }
    }

    fn on_off(raw: &str) -> Result<bool, String> {
        match raw {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        _window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(match a {
            "left" => {
                let have: Vec<&str> = left_icons(this.tree.collapsed)
                    .into_iter()
                    .map(label)
                    .collect();
                let want: Vec<&str> = b.split(',').collect();
                if have == want {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("标题栏左侧图标是 {}，不是 {b}", have.join(",")))
                }
            }
            "switches" => on_off(b).and_then(|want| {
                if this.bar_switches_now(cx) == want {
                    Ok(Outcome::Done)
                } else {
                    Err("状态栏开关的有无和预期不同".to_owned())
                }
            }),
            "statusbar_stats" => on_off(b).and_then(|want| {
                if this.status_host.is_some() == want {
                    Ok(Outcome::Done)
                } else {
                    Err("状态栏服务器状态的有无和预期不同".to_owned())
                }
            }),
            // assert_u6:theme:follow|light|dark（应用外观模式）；
            // assert_u6:skin:<light|dark>:<皮肤 id>（那一栏现在选的皮肤，id 带冒号以外的字符）。
            "theme" => {
                let s = this.workspace.read(cx).settings().clone();
                let have = if s.follow_system {
                    "follow"
                } else if s.dark {
                    "dark"
                } else {
                    "light"
                };
                if have == b {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("应用外观是 {have}，不是 {b}"))
                }
            }
            "skin" => {
                let s = this.workspace.read(cx).settings().clone();
                let (slot, id) = b.split_once(':').unwrap_or((b, ""));
                let have = match slot {
                    "light" => s.skin_light,
                    "dark" => s.skin_dark,
                    _ => return Some(Err("skin 的栏只能是 light 或 dark".to_owned())),
                };
                if have == id {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("{slot} 栏的皮肤是 {have}，不是 {id}"))
                }
            }
            _ => Err("assert_u6:left|switches|statusbar_stats|theme|skin:<值>".to_owned()),
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
