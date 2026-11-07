//! macOS 的应用菜单（菜单栏里程序名那一栏）：关于、设置…、隐藏、退出。
//!
//! 为什么要有：macOS 上 Cmd+Q / Cmd+H / Cmd+, 是应用菜单提供的。没有菜单时只有主窗口自己的按键监听认
//! Cmd+Q，焦点在文件窗口、独立终端窗口、更新窗口时按了没反应。「退出」走主窗口同一条退出流程
//! （有进行中的会话、传输、AI 回合或没保存的编辑器先确认，确认后由主窗口自己收尾退出）。
//!
//! 只在 macOS 上装（[`install`]）；菜单和快捷键是纯数据，各平台的测试都编译它们。
//! 复制 / 粘贴不需要「编辑」菜单：输入框（gpui-kit）在 macOS 上自己绑了 Cmd+C / Cmd+V，
//! 终端在按键处理里认 Cmd+C / Cmd+V。

use ::gpui::{KeyBinding, Menu, MenuItem, actions};

use crate::i18n::{Msg, t};

actions!(zeeterm_app, [About, OpenSettings, Hide, Quit]);

#[derive(Clone, Copy, Debug)]
enum AppMenuMsg {
    About,
    Settings,
    Hide,
    Quit,
}

impl Msg for AppMenuMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::About => "关于 ZeeTerm",
            Self::Settings => "设置…",
            Self::Hide => "隐藏 ZeeTerm",
            Self::Quit => "退出 ZeeTerm",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::About => "About ZeeTerm",
            Self::Settings => "Settings…",
            Self::Hide => "Hide ZeeTerm",
            Self::Quit => "Quit ZeeTerm",
        }
    }
}

/// 菜单栏：只有应用菜单一栏（系统会把第一栏的标题换成程序名）。
pub(crate) fn menus() -> Vec<Menu> {
    vec![Menu::new(crate::product::NAME).items([
        MenuItem::action(t(AppMenuMsg::About), About),
        MenuItem::separator(),
        MenuItem::action(t(AppMenuMsg::Settings), OpenSettings),
        MenuItem::separator(),
        MenuItem::action(t(AppMenuMsg::Hide), Hide),
        MenuItem::separator(),
        MenuItem::action(t(AppMenuMsg::Quit), Quit),
    ])]
}

/// 应用级快捷键（任何窗口里都生效；菜单项上显示的快捷键也取自这里）。
pub(crate) fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-h", Hide, None),
    ]
}

/// 装上菜单栏和快捷键。`handle` / `view` 是主窗口：退出、设置、关于都交给它处理。
#[cfg(target_os = "macos")]
pub(crate) fn install(
    cx: &mut ::gpui::App,
    handle: ::gpui::AnyWindowHandle,
    view: ::gpui::Entity<crate::main_window::MainWindow>,
) {
    cx.bind_keys(key_bindings());
    {
        let view = view.clone();
        cx.on_action(move |_: &Quit, cx| {
            let view = view.clone();
            let _ = handle.update(cx, |_, window, cx| {
                view.update(cx, |this, cx| {
                    if this.request_quit(cx) {
                        this.do_quit(window, cx);
                    } else {
                        // 要先确认：确认框在主窗口里，把它叫到前面。
                        window.activate_window();
                    }
                });
            });
        });
    }
    {
        let view = view.clone();
        cx.on_action(move |_: &OpenSettings, cx| {
            let view = view.clone();
            let _ = handle.update(cx, |_, window, cx| {
                window.activate_window();
                // 锁着的时候只把窗口叫出来，锁屏页之外什么都不打开。
                if !crate::app_lock::is_locked() {
                    view.update(cx, |this, cx| {
                        this.show_view(crate::main_window::nav::MainView::Settings, window, cx)
                    });
                }
            });
        });
    }
    cx.on_action(move |_: &About, cx| {
        let view = view.clone();
        let _ = handle.update(cx, |_, window, cx| {
            window.activate_window();
            if !crate::app_lock::is_locked() {
                view.update(cx, |this, cx| {
                    this.open_settings_section(
                        crate::settings_nav::SettingsSection::About,
                        window,
                        cx,
                    )
                });
            }
        });
    });
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.set_menus(menus());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_menu_has_about_settings_hide_and_quit() {
        let menus = menus();
        assert_eq!(menus.len(), 1);
        let names: Vec<Option<String>> = menus[0]
            .items
            .iter()
            .map(|item| match item {
                MenuItem::Action { name, .. } => Some(name.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(names.iter().flatten().count(), 4, "{names:?}");
        assert_eq!(names.last(), Some(&Some(t(AppMenuMsg::Quit).to_owned())));
        assert_eq!(key_bindings().len(), 3);
    }

    #[test]
    fn menu_texts_exist_in_both_languages() {
        for msg in [
            AppMenuMsg::About,
            AppMenuMsg::Settings,
            AppMenuMsg::Hide,
            AppMenuMsg::Quit,
        ] {
            assert!(!msg.zh().is_empty() && !msg.en().is_empty());
            assert_ne!(msg.zh(), msg.en());
        }
    }
}
