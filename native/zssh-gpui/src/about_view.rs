//! 设置页的「关于」一节（G12）：版本、检查更新、启动时自动检查、数据目录、known_hosts、开源许可。
//! 对齐 0.2.14 设置页的 `AboutSection` / `UpdateRow`，但按试用版的规则显示（见 `product`）：
//! 试用版不连任何更新源——"检查更新"只显示说明并给"打开发布页"，"启动时自动检查"锁在关闭。
//!
//! 数据目录、known_hosts 的路径显示前过 `redact_paths`（用户目录换成 `%USERPROFILE%`），
//! 截图和录屏不会带出用户名。

use ::gpui::{AnyElement, Context, SharedString, Window, px};
use gpui_kit::component::button::Button;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::*;

use crate::agent_text::redact_paths;
use crate::app_theme::Tokens;
use crate::product;
use crate::settings_view::card;

/// 关于页的状态：只有两个界面开关，没有任何后台任务。
#[derive(Default)]
pub struct AboutView {
    /// 点过「检查更新」：说明从默认提示换成试用版的完整说明。
    checked: bool,
    /// 第三方声明展开着。
    license_open: bool,
}

/// 版本行右边的文字。
pub fn version_text() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// 路径的显示文字：用户目录换成 `%USERPROFILE%`。
pub fn display_path(path: &std::path::Path) -> String {
    redact_paths(&path.display().to_string())
}

/// 数据目录的显示文字（已脱敏）。
pub fn data_dir_text() -> String {
    display_path(&crate::settings::data_dir())
}

/// known_hosts 的显示文字（已脱敏）。
pub fn known_hosts_text() -> String {
    display_path(&crate::settings::data_dir().join("known_hosts"))
}

/// 「检查更新」一行下面的说明。
pub fn update_description(auto_update: bool, checked: bool) -> &'static str {
    match (auto_update, checked) {
        // 自动更新开着时由更新窗口负责呈现结果，这一行只留一句引导。
        (true, _) => "点「检查更新」打开更新窗口，检查、下载并校验新版本。",
        (false, false) => product::TRIAL_UPDATE_HINT,
        (false, true) => product::TRIAL_UPDATE_NOTICE,
    }
}

/// 「启动时自动检查」一行的说明和开关能不能点：`(说明, 能不能点)`。
pub fn auto_check_row(auto_update: bool) -> (&'static str, bool) {
    if auto_update {
        (
            "每次打开时检查一次，程序开着时定时再查；关掉后只在点「检查更新」时检查。",
            true,
        )
    } else {
        (product::TRIAL_AUTO_CHECK_NOTE, false)
    }
}

/// 试用版"打开发布页"的地址：由内嵌的更新源推出；推不出来就没有这个按钮。
pub fn releases_url() -> Option<String> {
    let config = zssh_update::embedded_config().ok()?;
    config
        .endpoints
        .iter()
        .find_map(|endpoint| zssh_update::releases_page_url(endpoint))
}

impl AboutView {
    pub fn new() -> Self {
        Self::default()
    }

    /// 「检查更新」。试用版只显示说明，不发任何请求；自动更新开着时打开更新窗口（F2 的完整流程）。
    pub fn check_update(&mut self, cx: &mut Context<Self>) {
        if product::AUTO_UPDATE_ENABLED {
            crate::update_demo::open_update_window(cx);
        } else {
            self.checked = true;
        }
        cx.notify();
    }

    pub fn toggle_license(&mut self, cx: &mut Context<Self>) {
        self.license_open = !self.license_open;
        cx.notify();
    }

    /// 驱动脚本断言用：当前界面上这几行文字拼在一起（不含第三方声明正文）。
    #[cfg(debug_assertions)]
    pub fn visible_text(&self) -> String {
        let (auto_note, _) = auto_check_row(product::AUTO_UPDATE_ENABLED);
        [
            product::NAME.to_owned(),
            version_text(),
            update_description(product::AUTO_UPDATE_ENABLED, self.checked).to_owned(),
            auto_note.to_owned(),
            data_dir_text(),
            known_hosts_text(),
        ]
        .join("\n")
    }

    #[cfg(debug_assertions)]
    pub fn license_open(&self) -> bool {
        self.license_open
    }
}

fn muted(text: impl Into<SharedString>, tokens: &Tokens) -> AnyElement {
    div()
        .text_xs()
        .text_color(tokens.text_muted)
        .child(text.into())
        .into_any_element()
}

/// 一行设置：左边名字和说明，右边是控件（同设置页别处的画法）。
fn setting(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> AnyElement {
    h_flex()
        .w_full()
        .min_h(px(40.0))
        .py_1()
        .gap_4()
        .items_center()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().text_color(tokens.text).child(title.into()))
                .child(muted(description, tokens)),
        )
        .child(h_flex().flex_none().gap_1().items_center().child(control))
        .into_any_element()
}

/// 只读的路径框：小字，放不下就截断。
fn path_box(text: String, tokens: &Tokens) -> AnyElement {
    div()
        .w_full()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(tokens.border)
        .text_xs()
        .text_color(tokens.text)
        .truncate()
        .child(text)
        .into_any_element()
}

impl AboutView {
    #[inline(never)]
    fn update_rows(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let check = cx.entity();
        let mut buttons = h_flex().gap_2().items_center();
        if !product::AUTO_UPDATE_ENABLED
            && let Some(url) = releases_url()
        {
            buttons = buttons.child(
                Button::new("about-open-releases")
                    .label("打开发布页")
                    .compact()
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            );
        }
        buttons = buttons.child(
            Button::new("about-check-update")
                .label("检查更新")
                .compact()
                .on_click(move |_, _, cx| {
                    check.update(cx, |this, cx| this.check_update(cx));
                }),
        );
        let (auto_note, auto_enabled) = auto_check_row(product::AUTO_UPDATE_ENABLED);
        vec![
            setting(
                "检查更新",
                update_description(product::AUTO_UPDATE_ENABLED, self.checked),
                buttons,
                tokens,
            ),
            setting(
                "启动时自动检查",
                auto_note,
                // 试用版：锁在关闭。接管身份、补上偏好存取之后再接开关的状态。
                Switch::new("about-auto-check")
                    .checked(false)
                    .disabled(!auto_enabled)
                    .accessibility_label("启动时自动检查"),
                tokens,
            ),
        ]
    }

    #[inline(never)]
    fn path_rows(tokens: &Tokens) -> Vec<AnyElement> {
        let open = Button::new("about-open-data-dir")
            .label("打开所在文件夹")
            .compact()
            .on_click(move |_, _, cx| {
                let dir = crate::settings::data_dir();
                // 还没写过任何数据时目录可能不存在：先建出来，资源管理器才打得开。
                let _ = std::fs::create_dir_all(&dir);
                cx.reveal_path(&dir);
            });
        vec![
            setting(
                "数据目录",
                "主机库、设置和已知主机指纹都在这里。",
                open,
                tokens,
            ),
            path_box(data_dir_text(), tokens),
            setting(
                "known_hosts",
                "自己维护的指纹文件（系统那份只读）。",
                div(),
                tokens,
            ),
            path_box(known_hosts_text(), tokens),
        ]
    }

    #[inline(never)]
    fn license_rows(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let toggle = cx.entity();
        let button = Button::new("about-license")
            .label(if self.license_open {
                "收起第三方声明"
            } else {
                "第三方声明"
            })
            .compact()
            .on_click(move |_, _, cx| {
                toggle.update(cx, |this, cx| this.toggle_license(cx));
            });
        let mut rows = vec![setting(
            "开源许可",
            format!(
                "{} 以 GPL-3.0-or-later 许可发布；第三方组件与致谢见第三方声明。",
                product::NAME
            ),
            button,
            tokens,
        )];
        if self.license_open {
            rows.push(
                div()
                    .id("about-notice")
                    .w_full()
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .child(product::notice_text())
                    .into_any_element(),
            );
        }
        rows
    }
}

impl Render for AboutView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        let mut rows = vec![setting(
            product::NAME,
            "面向 Windows 的 SSH 客户端：标签与分屏终端、跳板机与代理、AI 运维助手。",
            div()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(version_text()),
            &tokens,
        )];
        rows.extend(self.update_rows(&tokens, cx));
        rows.extend(Self::path_rows(&tokens));
        rows.extend(self.license_rows(&tokens, cx));
        card("about-card", "关于", None, &tokens, rows)
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        auto_check_row, data_dir_text, display_path, known_hosts_text, releases_url,
        update_description, version_text,
    };
    use crate::product;

    #[test]
    fn the_trial_build_never_offers_an_automatic_update() {
        assert_eq!(update_description(false, false), product::TRIAL_UPDATE_HINT);
        let after = update_description(false, true);
        assert_eq!(after, product::TRIAL_UPDATE_NOTICE);
        assert!(after.contains("试用版") && after.contains("发布页"));
        let (note, enabled) = auto_check_row(false);
        assert!(!enabled, "试用版里这个开关不能点");
        assert_eq!(note, product::TRIAL_AUTO_CHECK_NOTE);
    }

    #[test]
    fn with_automatic_updates_the_switch_works_and_the_update_window_takes_over() {
        assert!(auto_check_row(true).1);
        assert!(update_description(true, false).contains("更新窗口"));
        assert_eq!(
            update_description(true, true),
            update_description(true, false)
        );
    }

    #[test]
    fn the_version_comes_from_the_package_and_is_not_zero() {
        let text = version_text();
        assert!(text.starts_with('v'));
        assert_ne!(text, "v0.0.0");
    }

    #[test]
    fn the_releases_page_is_a_github_releases_url() {
        let url = releases_url().expect("内嵌的更新源应能推出发布页");
        assert!(url.starts_with("https://github.com/") && url.ends_with("/releases"));
    }

    #[test]
    fn paths_are_shown_without_the_user_directory() {
        // 用户目录前缀换成 %USERPROFILE%（有 USERPROFILE 时），不管数据目录在哪。
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        if home.is_empty() {
            eprintln!("跳过：读不到 USERPROFILE");
            return;
        }
        let dir = std::path::Path::new(&home)
            .join("AppData")
            .join("Roaming")
            .join("ZeeTerm");
        assert_eq!(display_path(&dir), r"%USERPROFILE%\AppData\Roaming\ZeeTerm");
    }

    #[test]
    fn a_user_directory_with_chinese_or_spaces_is_redacted_too() {
        // redact_home 按字符比较：中文、空格都不影响。
        let shown = crate::agent_text::redact_home(
            r"C:\Users\张 三\AppData\Roaming\ZeeTerm",
            r"C:\Users\张 三",
        );
        assert_eq!(shown, r"%USERPROFILE%\AppData\Roaming\ZeeTerm");
    }

    #[test]
    fn the_data_dir_under_the_user_profile_is_redacted_when_shown() {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        if home.is_empty() {
            eprintln!("跳过：读不到 USERPROFILE");
            return;
        }
        // 数据目录不在用户目录下（比如测试用的独立目录）时没有东西可脱敏，同样不应带出用户目录。
        for text in [data_dir_text(), known_hosts_text()] {
            assert!(
                !text.to_lowercase().contains(&home.to_lowercase()),
                "显示文字里不该有用户目录"
            );
        }
    }
}
