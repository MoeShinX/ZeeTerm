//! 设置页的「关于」一节（G12，R1 起是正式版的样子）：版本、检查更新、启动时自动检查、
//! known_hosts、第三方组件与许可。数据目录从 V2-SETTINGS-1 起放在「常规」页（[`data_dir_rows`]），
//! 「常规」页的检查更新入口和这里共用同一份实现（[`check_update_now`]、[`update_buttons`]）。
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

/// 「关于」页的说明文字。路径、产品名、版本号是数据，拼在文案里原样显示。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // 有些键只在测试 / 调试构建里用到
enum AboutMsg {
    UpdateReady,
    AutoCheck,
    SaveFailedPrefix,
    /// 调试断言里那行「启动时自动检查：开 / 关」。
    AutoCheckValuePrefix,
    On,
    Off,
    OpenReleases,
    CheckUpdate,
    AutoCheckTitle,
    OpenFolder,
    DataDir,
    DataDirNote,
    KnownHostsNote,
    HideNotices,
    ShowNotices,
    NoticesTitle,
    /// 产品名 + 本键（发布许可和第三方声明在哪看）。
    LicenseSuffix,
    Tagline,
    VersionInfo,
}

impl crate::i18n::Msg for AboutMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::UpdateReady => "点「检查更新」打开更新窗口，检查、下载并校验新版本。",
            Self::AutoCheck => {
                "每次打开程序时检查一次，程序开着时每 6 小时再查一次；关掉后只在点「检查更新」时检查。"
            }
            Self::SaveFailedPrefix => "保存失败：",
            Self::AutoCheckValuePrefix => "启动时自动检查：",
            Self::On => "开",
            Self::Off => "关",
            Self::OpenReleases => "打开发布页",
            Self::CheckUpdate => "检查更新",
            Self::AutoCheckTitle => "启动时自动检查",
            Self::OpenFolder => "打开所在文件夹",
            Self::DataDir => "数据目录",
            Self::DataDirNote => "主机库、设置和已知主机指纹都在这里。",
            Self::KnownHostsNote => "自己维护的指纹文件（系统那份只读）。",
            Self::HideNotices => "收起第三方声明",
            Self::ShowNotices => "第三方声明",
            Self::NoticesTitle => "第三方组件与许可",
            Self::LicenseSuffix => {
                " 以 GPL-3.0-or-later 许可发布；使用的第三方组件及其许可见第三方声明。"
            }
            Self::Tagline => {
                "面向 Windows 的 SSH 客户端：标签与分屏终端、跳板机与代理、AI 运维助手。"
            }
            Self::VersionInfo => "版本信息",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::UpdateReady => {
                "Click \"Check for Updates\" to open the update window, check, download, and verify a new version."
            }
            Self::AutoCheck => {
                "Checked once at startup and every 6 hours while the app is running; when disabled, only when you click \"Check for Updates\"."
            }
            Self::SaveFailedPrefix => "Failed to save: ",
            Self::AutoCheckValuePrefix => "Check at startup: ",
            Self::On => "On",
            Self::Off => "Off",
            Self::OpenReleases => "Open Releases Page",
            Self::CheckUpdate => "Check for Updates",
            Self::AutoCheckTitle => "Check for Updates at Startup",
            Self::OpenFolder => "Open Containing Folder",
            Self::DataDir => "Data Directory",
            Self::DataDirNote => {
                "The host library, settings, and known-host fingerprints all live here."
            }
            Self::KnownHostsNote => {
                "The fingerprint file this app maintains (the system-wide one is read-only)."
            }
            Self::HideNotices => "Hide Third-Party Notices",
            Self::ShowNotices => "Third-Party Notices",
            Self::NoticesTitle => "Third-Party Components and Licenses",
            Self::LicenseSuffix => {
                " is released under GPL-3.0-or-later; the third-party components it uses and their licenses are listed in the third-party notices."
            }
            Self::Tagline => {
                "An SSH client for Windows: tabbed and split terminals, jump hosts and proxies, and an AI operations assistant."
            }
            Self::VersionInfo => "Version Information",
        }
    }
}

/// 「检查更新」一行下面的说明。`signing_configured` 为假（公钥还是占位）时说明改成"更新签名未配置"。
pub fn update_description(signing_configured: bool) -> &'static str {
    if signing_configured {
        crate::i18n::t(AboutMsg::UpdateReady)
    } else {
        crate::update::signing_unconfigured()
    }
}

/// 「检查更新」：打开更新窗口（读清单、下载、验签、确认后安装）。关于页、常规页、托盘菜单共用这一个入口。
pub fn check_update_now(cx: &mut ::gpui::App) {
    if product::AUTO_UPDATE_ENABLED {
        crate::update::open_update_window(cx);
    }
}

/// 「检查更新」一行右边的按钮们：发布页（推得出地址才有）+ 检查更新。关于页和常规页各放一份，
/// 区别只在按钮 id 和点「检查更新」之后调谁。
pub fn update_buttons(
    releases_id: &'static str,
    check_id: &'static str,
    on_check: impl Fn(&mut Window, &mut ::gpui::App) + 'static,
) -> ::gpui::Div {
    let mut buttons = h_flex().gap_2().items_center();
    // 发布页是网络不通、签名没配置时手动下载的退路，正式版也留着。
    if let Some(url) = releases_url() {
        buttons = buttons.child(
            Button::new(releases_id)
                .label(crate::i18n::t(AboutMsg::OpenReleases))
                .compact()
                .on_click(move |_, _, cx| cx.open_url(&url)),
        );
    }
    buttons.child(
        Button::new(check_id)
            .label(crate::i18n::t(AboutMsg::CheckUpdate))
            .compact()
            .on_click(move |_, window, cx| on_check(window, cx)),
    )
}

/// 更新签名公钥是不是已经配置好（占位值、内置配置读不出来都算没配；调试构建里钩子给的公钥也算）。
pub fn signing_configured() -> bool {
    crate::update::update_pubkey().is_ok()
}

/// 「启动时自动检查」一行的说明。
pub fn auto_check_description() -> &'static str {
    crate::i18n::t(AboutMsg::AutoCheck)
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
        check_update_now(cx);
        cx.notify();
    }

    /// 切换「启动时自动检查」并写回设置库；写失败就保持原值并给出原因。
    pub fn toggle_auto_check(&mut self, cx: &mut Context<Self>) {
        let next = !self.auto_check;
        let saved = crate::host_db::open_database()
            .map_err(|err| crate::error_text::error_text(&err))
            .and_then(|db| {
                zssh_core::updater::set_auto_check(&db, next)
                    .map_err(|err| crate::error_text::error_text(&err))
            });
        match saved {
            Ok(()) => {
                self.auto_check = next;
                self.save_error = None;
            }
            Err(err) => {
                self.save_error = Some(format!(
                    "{}{err}",
                    crate::i18n::t(AboutMsg::SaveFailedPrefix)
                ))
            }
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
                "{}{}",
                crate::i18n::t(AboutMsg::AutoCheckValuePrefix),
                crate::i18n::t(if self.auto_check {
                    AboutMsg::On
                } else {
                    AboutMsg::Off
                })
            ),
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
        .flex_wrap()
        .min_h(px(28.0))
        .gap_3()
        .items_center()
        .justify_between()
        .child(
            v_flex()
                .flex_1()
                .min_w(px(220.0))
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

/// 「常规」页的数据位置：数据目录的说明 + 打开按钮，下面一行是（已脱敏的）路径。
pub(crate) fn data_dir_rows(tokens: &Tokens) -> Vec<AnyElement> {
    let open = Button::new("general-open-data-dir")
        .label(crate::i18n::t(AboutMsg::OpenFolder))
        .compact()
        .on_click(move |_, _, cx| {
            let dir = crate::settings::data_dir();
            // 还没写过任何数据时目录可能不存在：先建出来，资源管理器才打得开。
            let _ = std::fs::create_dir_all(&dir);
            cx.reveal_path(&dir);
        });
    vec![
        setting(
            crate::i18n::t(AboutMsg::DataDir),
            crate::i18n::t(AboutMsg::DataDirNote),
            open,
            tokens,
        ),
        path_box(data_dir_text(), tokens),
    ]
}

impl AboutView {
    #[inline(never)]
    fn update_rows(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let check = cx.entity();
        let toggle = cx.entity();
        let buttons = update_buttons("about-open-releases", "about-check-update", move |_, cx| {
            check.update(cx, |this, cx| this.check_update(cx));
        });
        let auto_note = match &self.save_error {
            Some(error) => crate::i18n::paren(auto_check_description(), error),
            None => auto_check_description().to_owned(),
        };
        vec![
            setting(
                crate::i18n::t(AboutMsg::CheckUpdate),
                update_description(signing_configured()),
                buttons,
                tokens,
            ),
            setting(
                crate::i18n::t(AboutMsg::AutoCheckTitle),
                auto_note,
                Switch::new("about-auto-check")
                    .checked(self.auto_check)
                    .accessibility_label(crate::i18n::t(AboutMsg::AutoCheckTitle))
                    .on_click(move |_, _, cx| {
                        toggle.update(cx, |this, cx| this.toggle_auto_check(cx));
                    }),
                tokens,
            ),
        ]
    }

    #[inline(never)]
    fn path_rows(tokens: &Tokens) -> Vec<AnyElement> {
        vec![
            setting(
                "known_hosts",
                crate::i18n::t(AboutMsg::KnownHostsNote),
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
            .label(crate::i18n::t(if self.license_open {
                AboutMsg::HideNotices
            } else {
                AboutMsg::ShowNotices
            }))
            .compact()
            .on_click(move |_, _, cx| {
                toggle.update(cx, |this, cx| this.toggle_license(cx));
            });
        let mut rows = vec![setting(
            crate::i18n::t(AboutMsg::NoticesTitle),
            format!(
                "{}{}",
                product::NAME,
                crate::i18n::t(AboutMsg::LicenseSuffix)
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
            crate::i18n::t(AboutMsg::Tagline),
            div()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(version_text()),
            &tokens,
        )];
        rows.extend(self.update_rows(&tokens, cx));
        rows.extend(Self::path_rows(&tokens));
        rows.extend(self.license_rows(&tokens, cx));
        card(
            "about-card",
            crate::i18n::t(AboutMsg::VersionInfo),
            None,
            &tokens,
            rows,
        )
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
        assert_eq!(text, crate::update::signing_unconfigured());
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
