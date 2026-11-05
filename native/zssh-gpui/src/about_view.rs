//! 设置页的「关于」一节（G12，R1 起是正式版的样子）：版本、检查更新、启动时自动检查、数据目录、
//! known_hosts、第三方组件与许可。
//!
//! 「检查更新」打开更新窗口（`update`）：读 `latest.json`、下载、校验签名、确认后安装。
//! 「启动时自动检查」默认开，存在本机设置里（`zssh_core::updater::AUTO_CHECK_KEY`，不参与同步）；
//! 它只管后台自动检查（`update_auto`），手动「检查更新」不受影响。
//! 更新签名公钥还是占位值时（`zssh_update::is_placeholder_pubkey`）这一行改成提示"更新签名未配置"。
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
use crate::typography::{Role, Typo as _};

/// 关于页的状态：两个界面开关，加上「启动时自动检查」这一项的本机偏好。
pub struct AboutView {
    /// 「启动时自动检查」当前值（建页时从设置库读一次，切换时写回）。
    auto_check: bool,
    /// 保存偏好失败时的提示（显示在开关下面的说明里）。
    save_error: Option<String>,
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

/// 「检查更新」一行下面的说明。`signing_configured` 为假（公钥还是占位）时说明改成"更新签名未配置"。
pub fn update_description(signing_configured: bool) -> &'static str {
    if signing_configured {
        "点「检查更新」打开更新窗口，检查、下载并校验新版本。"
    } else {
        crate::update::SIGNING_UNCONFIGURED
    }
}

/// 更新签名公钥是不是已经配置好（占位值、内置配置读不出来都算没配；调试构建里钩子给的公钥也算）。
pub fn signing_configured() -> bool {
    crate::update::update_pubkey().is_ok()
}

/// 「启动时自动检查」一行的说明。
pub fn auto_check_description() -> &'static str {
    "每次打开程序时检查一次，程序开着时每 6 小时再查一次；关掉后只在点「检查更新」时检查。"
}

/// 发布页的地址：由内嵌的更新源推出；推不出来就没有这个按钮。
pub fn releases_url() -> Option<String> {
    let config = zssh_update::embedded_config().ok()?;
    config
        .endpoints
        .iter()
        .find_map(|endpoint| zssh_update::releases_page_url(endpoint))
}

/// 读「启动时自动检查」偏好；设置库打不开或值读不懂就按缺省（开）。
fn load_auto_check() -> bool {
    crate::host_db::open_database()
        .ok()
        .and_then(|db| zssh_core::updater::load_prefs(&db).ok())
        .is_none_or(|prefs| zssh_core::updater::should_auto_check(&prefs))
}

impl AboutView {
    pub fn new() -> Self {
        Self {
            auto_check: load_auto_check(),
            save_error: None,
            license_open: false,
        }
    }

    /// 「检查更新」：打开更新窗口（手动检查，不受「启动时自动检查」开关影响）。
    pub fn check_update(&mut self, cx: &mut Context<Self>) {
        if product::AUTO_UPDATE_ENABLED {
            crate::update::open_update_window(cx);
        }
        cx.notify();
    }

    /// 切换「启动时自动检查」并写回设置库；写失败就保持原值并给出原因。
    pub fn toggle_auto_check(&mut self, cx: &mut Context<Self>) {
        let next = !self.auto_check;
        let saved = crate::host_db::open_database()
            .map_err(|err| err.to_string())
            .and_then(|db| {
                zssh_core::updater::set_auto_check(&db, next).map_err(|err| err.to_string())
            });
        match saved {
            Ok(()) => {
                self.auto_check = next;
                self.save_error = None;
            }
            Err(err) => self.save_error = Some(format!("保存失败：{err}")),
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
        [
            product::NAME.to_owned(),
            version_text(),
            update_description(signing_configured()).to_owned(),
            auto_check_description().to_owned(),
            format!(
                "启动时自动检查：{}",
                if self.auto_check { "开" } else { "关" }
            ),
            data_dir_text(),
            known_hosts_text(),
        ]
        .join("\n")
    }

    #[cfg(debug_assertions)]
    pub fn license_open(&self) -> bool {
        self.license_open
    }

    #[cfg(debug_assertions)]
    pub fn auto_check(&self) -> bool {
        self.auto_check
    }
}

impl Default for AboutView {
    fn default() -> Self {
        Self::new()
    }
}

fn muted(text: impl Into<SharedString>, tokens: &Tokens) -> AnyElement {
    div()
        .typo(Role::Caption, tokens)
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
        .min_h(px(28.0))
        .gap_4()
        .items_center()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(div().typo(Role::ItemName, tokens).child(title.into()))
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
        let toggle = cx.entity();
        let mut buttons = h_flex().gap_2().items_center();
        // 发布页是网络不通、签名没配置时手动下载的退路，正式版也留着。
        if let Some(url) = releases_url() {
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
        let auto_note = match &self.save_error {
            Some(error) => format!("{}（{error}）", auto_check_description()),
            None => auto_check_description().to_owned(),
        };
        vec![
            setting(
                "检查更新",
                update_description(signing_configured()),
                buttons,
                tokens,
            ),
            setting(
                "启动时自动检查",
                auto_note,
                Switch::new("about-auto-check")
                    .checked(self.auto_check)
                    .accessibility_label("启动时自动检查")
                    .on_click(move |_, _, cx| {
                        toggle.update(cx, |this, cx| this.toggle_auto_check(cx));
                    }),
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
            "第三方组件与许可",
            format!(
                "{} 以 GPL-3.0-or-later 许可发布；使用的第三方组件及其许可见第三方声明。",
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
                    .typo(Role::Caption, tokens)
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
        card("about-card", "版本信息", None, &tokens, rows)
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        auto_check_description, data_dir_text, display_path, known_hosts_text, releases_url,
        signing_configured, update_description, version_text,
    };
    use crate::product;

    #[test]
    fn the_official_build_offers_online_updates_with_no_trial_wording() {
        const {
            assert!(product::AUTO_UPDATE_ENABLED);
        }
        let configured = update_description(true);
        assert!(
            configured.contains("更新窗口"),
            "点「检查更新」打开更新窗口"
        );
        assert!(!configured.contains("试用"));
        assert!(!auto_check_description().contains("试用"));
        assert!(
            auto_check_description().contains("每次打开程序时检查一次"),
            "说明里写清自动检查的时机"
        );
    }

    #[test]
    fn an_unconfigured_update_key_is_called_out_instead_of_promising_updates() {
        let text = update_description(false);
        assert_eq!(text, crate::update::SIGNING_UNCONFIGURED);
        assert!(text.contains("更新签名未配置"));
        // 界面上"配没配"和更新流程里拿不拿得到公钥是同一个判断。
        assert_eq!(signing_configured(), crate::update::update_pubkey().is_ok());
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
