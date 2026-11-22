//! 调试驱动里 V2-U4（字体与排版）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩两个空函数。
//!
//! - `font_dump`：把界面字体的实际取值写进 `ZSSH_MAIN_LOG`（候选是否装了、主题字体族、系统界面字体落到哪、
//!   各字重是否选到了不同的字面），写进报告的字体调研用；
//! - `font_assert:family:<族名>`：主题现在用的界面字体族就是它；
//! - `text_mode:default|subpixel|grayscale`：换 GPUI 的文字渲染模式（调研「小字能不能更清晰」，截图对比用）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::{FontWeight, Window, font};
    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::{Outcome, log_line};

    const KNOWN: &[&str] = &["font_dump", "font_assert", "text_mode"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        _this: &mut MainWindow,
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
        Some(match name {
            "font_dump" => {
                dump(window, cx);
                Ok(Outcome::Done)
            }
            "text_mode" => {
                let mode = match a {
                    "default" => ::gpui::TextRenderingMode::PlatformDefault,
                    "subpixel" => ::gpui::TextRenderingMode::Subpixel,
                    "grayscale" => ::gpui::TextRenderingMode::Grayscale,
                    _ => return Some(Err("text_mode:default|subpixel|grayscale".to_owned())),
                };
                cx.set_text_rendering_mode(mode);
                cx.refresh_windows();
                Ok(Outcome::Done)
            }
            _ => match a {
                "family" => {
                    let now = cx.theme().font_family.to_string();
                    if now == b {
                        Ok(Outcome::Done)
                    } else {
                        Err(format!("断言没有成立：界面字体是 {now}，要 {b}"))
                    }
                }
                _ => Err("font_assert:family:<族名>".to_owned()),
            },
        })
    }

    /// 把字体相关的事实逐行写进日志。
    fn dump(window: &mut Window, cx: &mut Context<MainWindow>) {
        let log = crate::dev_hooks::var("ZSSH_MAIN_LOG").map(std::path::PathBuf::from);
        let installed = crate::fonts::installed_fonts(cx);
        let has = |name: &str| installed.iter().any(|n| n.eq_ignore_ascii_case(name));
        for name in [
            "Segoe UI Variable Text",
            "Segoe UI Variable",
            "Segoe UI",
            "Microsoft YaHei UI",
            "Microsoft YaHei",
        ] {
            log_line(&log, &format!("font: 已装 {name} = {}", has(name)));
        }
        let ui = crate::fonts::ui_font(cx);
        log_line(
            &log,
            &format!(
                "font: 选中 family={} fallbacks={:?}",
                ui.family, ui.fallbacks
            ),
        );
        log_line(
            &log,
            &format!("font: 主题 font_family={}", cx.theme().font_family),
        );
        let text_system = cx.text_system().clone();
        let family_of = |name: &str, weight: FontWeight| {
            let mut f = font(name.to_owned());
            f.weight = weight;
            let id = text_system.resolve_font(&f);
            let real = text_system
                .get_font_for_id(id)
                .map_or_else(|| "?".to_owned(), |f| f.family.to_string());
            format!("{real}#{}", id.0)
        };
        log_line(
            &log,
            &format!(
                "font: .SystemUIFont 落到 {}",
                family_of(".SystemUIFont", FontWeight::NORMAL)
            ),
        );
        let theme_family = cx.theme().font_family.to_string();
        // 字重有没有选到不同的字面：同一串字在各字重下排出来的宽度。宽度一样就是同一个字面。
        let width_of = |name: &str, weight: FontWeight, text: &str| {
            let mut f = font(name.to_owned());
            f.weight = weight;
            let run = ::gpui::TextRun {
                len: text.len(),
                font: f,
                color: ::gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                text.to_owned().into(),
                ::gpui::px(100.0),
                &[run],
                None,
            );
            f32::from(line.width)
        };
        for (label, weight) in [
            ("300", FontWeight::LIGHT),
            ("400", FontWeight::NORMAL),
            ("500", FontWeight::MEDIUM),
            ("600", FontWeight::SEMIBOLD),
            ("700", FontWeight::BOLD),
        ] {
            log_line(
                &log,
                &format!(
                    "font: 宽度 {theme_family} 字重 {label}: 西文 {:.2} / 中文 {:.2}",
                    width_of(&theme_family, weight, "Hamburgefonstiv Settings 0123456789"),
                    width_of(&theme_family, weight, "设置项名称与说明文字")
                ),
            );
        }
        for (label, weight) in [
            ("300", FontWeight::LIGHT),
            ("400", FontWeight::NORMAL),
            ("500", FontWeight::MEDIUM),
            ("600", FontWeight::SEMIBOLD),
            ("700", FontWeight::BOLD),
        ] {
            log_line(
                &log,
                &format!(
                    "font: {theme_family} 字重 {label} -> {}",
                    family_of(&theme_family, weight)
                ),
            );
        }
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
