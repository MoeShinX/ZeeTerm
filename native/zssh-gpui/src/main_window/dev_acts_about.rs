//! 调试驱动里和设置页「关于」一节有关的 `act:` 动作（G12）。只在 debug 构建里编进去；
//! release 里这里只剩两个空函数（驱动脚本本来也只在 debug 里读）。
//!
//! - `about_open`：打开设置页并把「关于」一节滚到顶；
//! - `about_check`：点「检查更新」（试用版只换说明文字，不发请求）；
//! - `about_license`：点「第三方声明」（展开 / 收起）；
//! - `about_assert:has|not:<文字>`：关于页的显示文字（版本、更新说明、路径）里有 / 没有这段文字；
//!   `about_assert:license:open|closed`：第三方声明是否展开；`about_assert:clean`：画面文字里没有用户目录和本机用户名。不对就让脚本失败。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;

    const KNOWN: &[&str] = &["about_open", "about_check", "about_license", "about_assert"];

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
        let mut parts = spec.splitn(3, ':');
        let _ = parts.next();
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(run_inner(this, name, a, b, window, cx))
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        a: &str,
        b: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        if name == "about_open" {
            this.show_view(super::super::nav::MainView::Settings, window, cx);
            this.set_settings_section(crate::settings_nav::SettingsSection::About, cx);
            return Ok(Outcome::Done);
        }
        // 关于一节第一次画出来之后才有实体：还没有就等下一拍。
        let Some(about) = this.about_settings.clone() else {
            return Ok(Outcome::Pending);
        };
        match name {
            "about_check" => about.update(cx, |view, cx| view.check_update(cx)),
            "about_license" => about.update(cx, |view, cx| view.toggle_license(cx)),
            "about_assert" => {
                let ok = match a {
                    "has" => about.read(cx).visible_text().contains(b),
                    "not" => !about.read(cx).visible_text().contains(b),
                    "license" => about.read(cx).license_open() == (b == "open"),
                    // clean：画面上的文字（含展开的第三方声明）里没有用户目录、`C:\Users\`、本机用户名。
                    // 用户名只在运行时从环境变量读，脚本和日志里不出现。
                    "clean" => {
                        let mut text = about.read(cx).visible_text();
                        if about.read(cx).license_open() {
                            text.push_str(&crate::product::notice_text());
                        }
                        let lower = text.to_lowercase();
                        let mut needles = vec![String::from(r"c:\users\")];
                        for var in ["USERNAME", "USERPROFILE"] {
                            if let Ok(value) = std::env::var(var)
                                && value.trim().len() >= 3
                            {
                                needles.push(value.trim().to_lowercase());
                            }
                        }
                        !needles.iter().any(|needle| lower.contains(needle))
                    }
                    _ => return Err("about_assert:has|not|license|clean[:<值>]".to_owned()),
                };
                if !ok {
                    return Err(format!("关于页的断言没有成立：{a}"));
                }
            }
            _ => return Err("还没有实现".to_owned()),
        }
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
