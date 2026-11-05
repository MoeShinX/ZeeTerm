//! 「管理 + 设置」统一页：左侧导航（顶部「← 返回应用」、「管理」组、「偏好」组）+ 右侧只显示当前分类
//! （偏好 8 类对齐 0.2.14 的 `SettingsPage.tsx`；管理 4 类是 V2-U3 把标题栏 / 主机页 / 侧面板里的
//! 管理入口收进来的）。
//!
//! 分类、顺序、图标、默认值和「记住上次的分类」的规则在 `settings_nav`（纯数据，有状态测试）；
//! 这里负责画导航栏、把各张设置卡放进对应分类，以及「连接」「SFTP」和外观里「主机树」「窗口」
//! 这几张读写数据库设置的卡（键名和取值范围见 `app_prefs`）。
//!
//! 管理组不重写业务：凭据 / 快捷命令 / 代理 / 端口转发各自仍由 `keychain_panel` / `quick_panel` /
//! `proxy_panel` / `forward_panel` 交出一份 [`PanelContent`]（标题、右上角按钮、列表 + 表单、底部按钮），
//! 这里把它画成整页：大标题、右上一排描边小按钮、筛选 + 搜索、列表、空状态。
//!
//! 外观 / 终端的卡由工作区驱动（`settings_view`），AI / 同步 / 备份 / 关于是各自的实体，
//! 这里只决定它们摆在哪个分类里。

use ::gpui::{App, AppContext as _, PathPromptOptions, SharedString, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use super::widgets::{Choice, pick, select};
use crate::app_prefs::{
    self, AppPrefs, CloseChoice, FileWindowChoice, KEEPALIVE_RANGE, KEEPALIVE_STEP,
    POOL_IDLE_RANGE, POOL_IDLE_STEP, SFTP_CONCURRENCY_RANGE, STATS_REFRESH_RANGE,
};
use crate::app_theme::Tokens;
use crate::settings_nav::{SHORTCUTS, SettingsSection};
use crate::settings_view::{card, row, row_with};
use crate::typography::{Role, Typo as _};
use zssh_core::sync::app_proxy::{APP_NETWORK_PROXY_KEY, AppProxyMode, SystemProxy};

/// 备份那一块在「同步」分类滚动容器里的下标（驱动脚本滚到备份一节用）：最上面是「从 Z-SSH 迁移」，然后是同步、备份。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub const BACKUP_ITEM: usize = 2;

/// 左侧导航栏宽度（比 0.2.14 的 `w-44` 宽一点，放得下「← 返回应用」和 12 个分类名）。
const NAV_WIDTH: f32 = 208.0;
/// 管理组内容区的限宽（列表 + 表单的阅读宽度）。
const MANAGE_WIDTH: f32 = 880.0;

/// 页面右侧最上面一行：大标题（可带灰色的数量）+ 靠右的一排按钮；宽度和下面的内容对齐。
fn page_header(
    title: &str,
    subtitle: Option<SharedString>,
    actions: Vec<::gpui::AnyElement>,
    max_width: f32,
    tokens: &Tokens,
) -> impl IntoElement {
    div().w_full().flex_none().child(
        h_flex()
            .w_full()
            .flex_wrap()
            .max_w(px(max_width))
            .mx_auto()
            .px_6()
            .pt_6()
            .pb_4()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_3()
                    .items_baseline()
                    .child(
                        div()
                            .typo(Role::PageTitle, tokens)
                            .child(SharedString::from(title.to_owned())),
                    )
                    .children(subtitle.map(|text| div().typo(Role::Caption, tokens).child(text))),
            )
            .child(
                h_flex()
                    .flex_shrink(1.0)
                    .min_w_0()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .children(actions),
            ),
    )
}

/// 设置页里的一块：与管理页保持相同阅读宽度和对齐方式。
fn settings_block(content: impl IntoElement, first: bool) -> impl IntoElement {
    div()
        .w_full()
        .flex_none()
        .max_w(px(MANAGE_WIDTH))
        .mx_auto()
        .px_6()
        .pb_6()
        .when(first, |d| d.pt_2())
        .child(content)
}

/// 数值型设置的滑块们：拖的过程中只让数字跟着动，松手才写库（同回滚行数那一条）。
pub(crate) struct PrefSliders {
    keepalive: Entity<SliderState>,
    pool_idle: Entity<SliderState>,
    stats_refresh: Entity<SliderState>,
    sftp_concurrency: Entity<SliderState>,
}

/// 数值设置的种类（滑块松手时按它分派）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrefNumber {
    Keepalive,
    PoolIdle,
    StatsRefresh,
    SftpConcurrency,
}

/// 开关型设置的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrefFlag {
    StatsShow,
    SftpPreserveMtime,
    SftpPreservePermissions,
    SftpShowHidden,
}

fn slider(range: (u32, u32), step: u32, value: u32, cx: &mut App) -> Entity<SliderState> {
    // 先设上限再设下限：组件每设一个就按当前的另一端夹一次值（同回滚行数的滑块）。
    cx.new(|_| {
        SliderState::new()
            .max(range.1 as f32)
            .min(range.0 as f32)
            .step(step as f32)
            .default_value(value as f32)
    })
}

impl PrefSliders {
    pub(super) fn new(prefs: &AppPrefs, cx: &mut App) -> Self {
        Self {
            keepalive: slider(KEEPALIVE_RANGE, KEEPALIVE_STEP, prefs.keepalive_secs, cx),
            pool_idle: slider(POOL_IDLE_RANGE, POOL_IDLE_STEP, prefs.pool_idle_secs, cx),
            stats_refresh: slider(STATS_REFRESH_RANGE, 1, prefs.stats_refresh_secs, cx),
            sftp_concurrency: slider(SFTP_CONCURRENCY_RANGE, 1, prefs.sftp_concurrency, cx),
        }
    }

    fn of(&self, kind: PrefNumber) -> &Entity<SliderState> {
        match kind {
            PrefNumber::Keepalive => &self.keepalive,
            PrefNumber::PoolIdle => &self.pool_idle,
            PrefNumber::StatsRefresh => &self.stats_refresh,
            PrefNumber::SftpConcurrency => &self.sftp_concurrency,
        }
    }

    fn all(&self) -> [(PrefNumber, &Entity<SliderState>); 4] {
        [
            (PrefNumber::Keepalive, &self.keepalive),
            (PrefNumber::PoolIdle, &self.pool_idle),
            (PrefNumber::StatsRefresh, &self.stats_refresh),
            (PrefNumber::SftpConcurrency, &self.sftp_concurrency),
        ]
    }
}

impl MainWindow {
    /// 滑块松手 → 写库。拖动过程只重画数字。
    pub(super) fn subscribe_pref_sliders(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<::gpui::Subscription> {
        self.pref_sliders
            .all()
            .into_iter()
            .map(|(kind, slider)| {
                cx.subscribe_in(
                    slider,
                    window,
                    move |this, _slider, event: &SliderEvent, window, cx| match event {
                        SliderEvent::Change(_) => cx.notify(),
                        SliderEvent::Release(value) => {
                            this.set_pref_number(kind, value.end().round() as u32, window, cx);
                        }
                    },
                )
            })
            .collect()
    }

    /// 启动时把设置里影响全局行为的两项套上：保活间隔、状态栏采集间隔。
    pub(super) fn apply_prefs(&self) {
        zssh_core::ssh::set_keepalive_interval(u64::from(self.prefs.keepalive_secs));
        self.prefs.apply_pool_idle();
        crate::status::set_refresh_secs(u64::from(self.prefs.stats_refresh_secs));
    }

    /// 重读这几项设置（进设置页时调：别处，比如配置同步，可能改过库）。
    pub(super) fn reload_prefs(&mut self) {
        if let Ok(db) = self.store.db() {
            self.prefs = AppPrefs::load(db);
            self.apply_prefs();
        }
    }

    fn set_pref_number(
        &mut self,
        kind: PrefNumber,
        value: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key, range) = match kind {
            PrefNumber::Keepalive => (
                zssh_core::store::settings::KEEPALIVE_INTERVAL_KEY,
                KEEPALIVE_RANGE,
            ),
            PrefNumber::PoolIdle => (
                zssh_core::store::settings::POOL_IDLE_TIMEOUT_KEY,
                POOL_IDLE_RANGE,
            ),
            PrefNumber::StatsRefresh => (app_prefs::STATS_REFRESH_KEY, STATS_REFRESH_RANGE),
            PrefNumber::SftpConcurrency => {
                (app_prefs::SFTP_CONCURRENCY_KEY, SFTP_CONCURRENCY_RANGE)
            }
        };
        let value = value.clamp(range.0, range.1);
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_number(db, key, value, range));
        match result {
            Ok(()) => {
                match kind {
                    PrefNumber::Keepalive => self.prefs.keepalive_secs = value,
                    PrefNumber::PoolIdle => self.prefs.pool_idle_secs = value,
                    PrefNumber::StatsRefresh => self.prefs.stats_refresh_secs = value,
                    PrefNumber::SftpConcurrency => self.prefs.sftp_concurrency = value,
                }
                self.apply_prefs();
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_pref_flag(
        &mut self,
        kind: PrefFlag,
        value: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = match kind {
            PrefFlag::StatsShow => app_prefs::STATS_SHOW_KEY,
            PrefFlag::SftpPreserveMtime => app_prefs::SFTP_PRESERVE_MTIME_KEY,
            PrefFlag::SftpPreservePermissions => app_prefs::SFTP_PRESERVE_PERMISSIONS_KEY,
            PrefFlag::SftpShowHidden => app_prefs::SFTP_SHOW_HIDDEN_KEY,
        };
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_flag(db, key, value));
        match result {
            Ok(()) => match kind {
                PrefFlag::StatsShow => self.prefs.stats_show = value,
                PrefFlag::SftpPreserveMtime => self.prefs.sftp_preserve_mtime = value,
                PrefFlag::SftpPreservePermissions => self.prefs.sftp_preserve_permissions = value,
                PrefFlag::SftpShowHidden => self.prefs.sftp_show_hidden = value,
            },
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_close_choice(
        &mut self,
        choice: CloseChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_text(db, app_prefs::CLOSE_ACTION_KEY, choice.id()));
        match result {
            Ok(()) => self.prefs.close_action = choice,
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_file_window_choice(
        &mut self,
        choice: FileWindowChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.store.db().and_then(|db| {
            app_prefs::write_text(db, app_prefs::FILE_WINDOW_CLOSE_KEY, choice.id())
        });
        match result {
            Ok(()) => self.prefs.file_window_close = choice,
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_ungrouped_flat(&mut self, flat: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = self.store.set_flat(flat) {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    fn set_tree_guides(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = self.store.set_tree_guides(on) {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    fn set_app_network_proxy(&mut self, raw: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_text(db, APP_NETWORK_PROXY_KEY, raw));
        if let Err(err) = result {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    /// 切到某个分类：滚动位置回到顶，并记下来（重启后恢复）。
    pub(crate) fn set_settings_section(
        &mut self,
        section: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        if self.settings_section != section {
            self.settings_section = section;
            self.settings_scroll
                .set_offset(::gpui::point(px(0.0), px(0.0)));
            // 换分类不带着上一页的搜索词；清空输入框要用 `Window`，画页面时再做。
            self.manage_search_reset = true;
            self.save_layout();
        }
        cx.notify();
    }

    /// 管理页顶部搜索框里的词；不在管理页时是空串（同一批列表也在别处用，不能被搜索词悄悄过滤）。
    pub(super) fn manage_query(&self, cx: &App) -> String {
        if self.nav.view == MainView::Settings && self.settings_section.is_manage() {
            self.manage_search.read(cx).value().trim().to_owned()
        } else {
            String::new()
        }
    }

    // ---- 调试驱动（只在 debug 构建里编进去，见 `dev_acts_u2`） ----

    /// 按名字改一项设置，走和界面控件同一批方法（滑块 / 开关 / 下拉选中以后调的那些）。
    #[cfg(debug_assertions)]
    pub(super) fn dev_set_pref(
        &mut self,
        key: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let number = || {
            value
                .parse::<u32>()
                .map_err(|_| format!("{key} 的值要是整数"))
        };
        let flag = || match value {
            "true" | "on" => Ok(true),
            "false" | "off" => Ok(false),
            _ => Err(format!("{key} 的值要是 on / off")),
        };
        match key {
            "keepalive" => self.set_pref_number(PrefNumber::Keepalive, number()?, window, cx),
            "pool_idle" => self.set_pref_number(PrefNumber::PoolIdle, number()?, window, cx),
            "stats_refresh" => {
                self.set_pref_number(PrefNumber::StatsRefresh, number()?, window, cx)
            }
            "sftp_concurrency" => {
                self.set_pref_number(PrefNumber::SftpConcurrency, number()?, window, cx);
            }
            "stats_show" => self.set_pref_flag(PrefFlag::StatsShow, flag()?, window, cx),
            "sftp_mtime" => self.set_pref_flag(PrefFlag::SftpPreserveMtime, flag()?, window, cx),
            "sftp_perms" => {
                self.set_pref_flag(PrefFlag::SftpPreservePermissions, flag()?, window, cx);
            }
            "sftp_hidden" => self.set_pref_flag(PrefFlag::SftpShowHidden, flag()?, window, cx),
            "ungrouped_flat" => self.set_ungrouped_flat(flag()?, window, cx),
            "session_log" => self.set_session_log_enabled(flag()?, window, cx),
            "host_info_panel" => self.set_host_info_panel(flag()?, window, cx),
            "session_log_dir" => self.set_session_log_dir(value.to_owned(), window, cx),
            "close_action" => {
                let choice = if value == "quit" {
                    CloseChoice::Quit
                } else {
                    CloseChoice::Tray
                };
                self.set_close_choice(choice, window, cx);
            }
            "file_window_close" => {
                let choice = if value == "merge" {
                    FileWindowChoice::Merge
                } else {
                    FileWindowChoice::Close
                };
                self.set_file_window_choice(choice, window, cx);
            }
            "default_proxy" => {
                let id = if value == "none" {
                    None
                } else {
                    Some(
                        value
                            .parse::<i64>()
                            .map_err(|_| "default_proxy 要是 none 或代理编号")?,
                    )
                };
                self.set_default_proxy(id, window, cx);
            }
            "app_proxy" => self.set_app_network_proxy(value, window, cx),
            _ => return Err(format!("不认识的设置 {key}")),
        }
        Ok(())
    }

    /// 重新从库里读一份再比：断言的是**落盘**的值，不是界面上缓存的。
    #[cfg(debug_assertions)]
    pub(super) fn dev_pref_text(&self, key: &str) -> Result<String, String> {
        let db = self.store.db()?;
        let prefs = AppPrefs::load(db);
        let text = |value: bool| if value { "on" } else { "off" }.to_owned();
        Ok(match key {
            "keepalive" => prefs.keepalive_secs.to_string(),
            // 连接代码真正在用的保活间隔（进程里的那个值），不是库里的。
            "keepalive_live" => zssh_core::ssh::keepalive_interval()
                .map_or_else(|| "0".to_owned(), |d| d.as_secs().to_string()),
            "pool_idle" => prefs.pool_idle_secs.to_string(),
            "stats_refresh" => prefs.stats_refresh_secs.to_string(),
            "sftp_concurrency" => prefs.sftp_concurrency.to_string(),
            "stats_show" => text(prefs.stats_show),
            "sftp_mtime" => text(prefs.sftp_preserve_mtime),
            "sftp_perms" => text(prefs.sftp_preserve_permissions),
            "sftp_hidden" => text(prefs.sftp_show_hidden),
            "session_log" => text(prefs.session_log_enabled),
            "host_info_panel" => text(prefs.host_info_panel),
            "session_log_dir" => prefs.session_log_dir,
            "close_action" => prefs.close_action.id().to_owned(),
            "file_window_close" => prefs.file_window_close.id().to_owned(),
            "ungrouped_flat" => text(self.store.flat()),
            "default_proxy" => db
                .default_proxy_id()
                .ok()
                .flatten()
                .map_or_else(|| "none".to_owned(), |id| id.to_string()),
            "app_proxy" => app_prefs::text_of(
                db.setting_get(APP_NETWORK_PROXY_KEY)
                    .ok()
                    .flatten()
                    .as_deref(),
            )
            .unwrap_or_default(),
            _ => return Err(format!("不认识的设置 {key}")),
        })
    }

    // ---- 画 ----

    pub(super) fn render_settings(
        &mut self,
        tokens: &Tokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // AI / 同步 / 备份 / 关于各是自己的实体：第一次打开设置页时建，之后一直留着
        // （输入到一半的内容切分类不丢）。
        let agents = match &self.agent_settings {
            Some(agents) => agents.clone(),
            None => {
                let agents = cx.new(|cx| crate::agents::AgentsDemo::new_embedded(window, cx));
                self.agent_settings = Some(agents.clone());
                agents
            }
        };
        let sync = match &self.sync_settings {
            Some(sync) => sync.clone(),
            None => {
                let sync = cx.new(|cx| crate::sync::SyncDemo::new_embedded(window, cx));
                self.sync_settings = Some(sync.clone());
                sync
            }
        };
        let backup = match &self.backup_settings {
            Some(backup) => backup.clone(),
            None => {
                let backup = cx.new(|cx| crate::backup::BackupDemo::new_embedded(window, cx));
                self.backup_settings = Some(backup.clone());
                backup
            }
        };
        let migrate = self.ensure_migrate(window, cx);
        let about = match &self.about_settings {
            Some(about) => about.clone(),
            None => {
                let about = cx.new(|_cx| crate::about_view::AboutView::new());
                self.about_settings = Some(about.clone());
                about
            }
        };
        // 换了分类：清空管理页的搜索框。
        if std::mem::take(&mut self.manage_search_reset) {
            let search = self.manage_search.clone();
            search.update(cx, |input, cx| input.set_value("", window, cx));
        }

        let section = self.settings_section;
        let content: ::gpui::AnyElement = if section.is_manage() {
            self.manage_section(section, tokens, window, cx)
        } else {
            let blocks: Vec<::gpui::AnyElement> = match section {
                SettingsSection::Appearance => self.appearance_blocks(tokens, cx),
                SettingsSection::Terminal => self.terminal_blocks(tokens, cx),
                SettingsSection::Connection => {
                    self.x11_ensure_loaded(window, cx);
                    let mut blocks = self.connection_cards(tokens, window, cx);
                    blocks.push(self.x11_card(tokens, cx));
                    blocks
                }
                SettingsSection::Sftp => vec![self.sftp_card(tokens, cx)],
                SettingsSection::Ai => vec![
                    self.ai_builtin_block(tokens, window, cx),
                    agents.into_any_element(),
                ],
                SettingsSection::Sync => {
                    vec![
                        migrate.into_any_element(),
                        sync.into_any_element(),
                        backup.into_any_element(),
                    ]
                }
                SettingsSection::Lock => self.lock_blocks(tokens, cx),
                SettingsSection::Shortcuts => shortcuts_cards(tokens),
                SettingsSection::About => vec![about.into_any_element()],
                _ => Vec::new(),
            };
            v_flex()
                .size_full()
                .child(page_header(
                    section.label(),
                    None,
                    Vec::new(),
                    MANAGE_WIDTH,
                    tokens,
                ))
                .child(
                    div()
                        .id("settings-scroll")
                        .track_scroll(&self.settings_scroll)
                        .flex_1()
                        .w_full()
                        .min_w_0()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        // 每一块是滚动容器的直接子元素：驱动脚本按下标滚到「备份」那一块。
                        .children(
                            blocks
                                .into_iter()
                                .enumerate()
                                .map(|(index, block)| settings_block(block, index == 0)),
                        ),
                )
                .into_any_element()
        };

        h_flex()
            .id("settings-page")
            .size_full()
            .child(self.settings_nav(tokens, cx))
            .child(div().flex_1().h_full().min_w_0().child(content))
    }

    /// 管理组的一页：大标题 + 右上角按钮、筛选 + 搜索、列表 / 表单、底部按钮。内容是原来侧面板的
    /// [`PanelContent`]，业务和表单一份都没改。
    fn manage_section(
        &mut self,
        section: SettingsSection,
        tokens: &Tokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let content = match section {
            SettingsSection::Credentials => self.keychain_panel_content(tokens, window, cx),
            SettingsSection::QuickCommands => self.quick_panel_content(tokens, window, cx),
            SettingsSection::Proxies => self.proxy_panel_content(tokens, window, cx),
            SettingsSection::Forwards => self.forward_panel_content(tokens, window, cx),
            _ => return div().into_any_element(),
        };
        let filter = self.manage_filter_bar(section, tokens, cx);
        v_flex()
            .size_full()
            .child(page_header(
                section.label(),
                content.subtitle,
                content.actions,
                MANAGE_WIDTH,
                tokens,
            ))
            .children(filter)
            .child(
                div()
                    .id("manage-scroll")
                    .track_scroll(&self.settings_scroll)
                    .flex_1()
                    .w_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(MANAGE_WIDTH))
                            .mx_auto()
                            .px_6()
                            .pb_8()
                            .gap_4()
                            .child(content.body)
                            .children(content.footer.map(|footer| {
                                div()
                                    .w_full()
                                    .pt_3()
                                    .border_t_1()
                                    .border_color(tokens.border)
                                    .child(footer)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 标题下面的一行：筛选下拉（有意义的页才有）+ 搜索框。列表是空的时候不画（那时是空状态）。
    fn manage_filter_bar(
        &self,
        section: SettingsSection,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<::gpui::AnyElement> {
        let entity = cx.entity();
        let (has_items, filter, search): (bool, Option<::gpui::AnyElement>, _) = match section {
            SettingsSection::Credentials => {
                let current = self.keychain.kind_filter;
                let kinds = super::identity_form::KINDS;
                let mut choices = vec![Choice::new("全部类型", current.is_none())];
                choices.extend(
                    kinds
                        .iter()
                        .map(|(kind, label)| Choice::new(*label, current == Some(*kind))),
                );
                let label = current.map_or("全部类型", super::identity_form::kind_label);
                (
                    !self.store.identities().is_empty(),
                    Some(
                        div()
                            .w(px(176.0))
                            .flex_none()
                            .child(select(
                                "manage-filter-kind",
                                label,
                                choices,
                                pick(&entity, move |this, index, _window, cx| {
                                    this.keychain.kind_filter =
                                        index.checked_sub(1).and_then(|at| {
                                            super::identity_form::KINDS
                                                .get(at)
                                                .map(|(kind, _)| *kind)
                                        });
                                    cx.notify();
                                }),
                            ))
                            .into_any_element(),
                    ),
                    self.manage_search.clone(),
                )
            }
            SettingsSection::Forwards => {
                let current = self.forward_ui.host_filter;
                let ssh_hosts: Vec<&zssh_core::model::Host> = self
                    .store
                    .hosts()
                    .iter()
                    .filter(|host| host.protocol.supports_port_forward())
                    .collect();
                let mut choices = vec![Choice::new("全部主机", current.is_none())];
                choices.extend(
                    ssh_hosts
                        .iter()
                        .map(|host| Choice::new(host.name.clone(), current == Some(host.id))),
                );
                let ids: Vec<i64> = ssh_hosts.iter().map(|host| host.id).collect();
                let label = current
                    .and_then(|id| self.store.host(id))
                    .map_or_else(|| "全部主机".to_owned(), |host| host.name.clone());
                (
                    !self.store.forwards().is_empty(),
                    Some(
                        div()
                            .w(px(176.0))
                            .flex_none()
                            .child(select(
                                "manage-filter-host",
                                label,
                                choices,
                                pick(&entity, move |this, index, _window, cx| {
                                    let host =
                                        index.checked_sub(1).and_then(|at| ids.get(at).copied());
                                    this.set_forward_host_filter(host, cx);
                                }),
                            ))
                            .into_any_element(),
                    ),
                    self.manage_search.clone(),
                )
            }
            SettingsSection::Proxies => (
                !self.store.proxies().is_empty(),
                None,
                self.manage_search.clone(),
            ),
            SettingsSection::QuickCommands => (
                !self.bar_settings().commands.is_empty(),
                None,
                self.quick_ui.search.clone(),
            ),
            _ => return None,
        };
        if !has_items {
            return None;
        }
        Some(
            div()
                .w_full()
                .flex_none()
                .child(
                    h_flex()
                        .w_full()
                        .max_w(px(MANAGE_WIDTH))
                        .mx_auto()
                        .px_6()
                        .pb_3()
                        .gap_2()
                        .items_center()
                        .children(filter)
                        .child(
                            div().flex_1().min_w_0().child(
                                gpui_kit::component::input::Input::new(&search)
                                    .prefix(
                                        Icon::new(IconName::Search)
                                            .size_4()
                                            .text_color(tokens.text_muted),
                                    )
                                    .cleanable(true),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 左侧导航：顶部「← 返回应用」，「管理」组 4 项，「偏好」组 8 项；选中的有底色和加粗。
    fn settings_nav(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let current = self.settings_section;
        let (hover, selected_bg, text, muted) =
            (tokens.hover, tokens.subtle, tokens.text, tokens.text_muted);
        let item = {
            let entity = entity.clone();
            let tokens = *tokens;
            move |index: usize, section: SettingsSection| {
                let entity = entity.clone();
                let is_current = section == current;
                h_flex()
                    .id(("settings-nav-item", index))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py(px(6.0))
                    .rounded_md()
                    .cursor_pointer()
                    .typo(
                        if is_current {
                            Role::NavSelected
                        } else {
                            Role::Nav
                        },
                        &tokens,
                    )
                    .when(is_current, |d| d.bg(selected_bg))
                    .when(!is_current, |d| d.hover(move |style| style.bg(hover)))
                    .child(
                        Icon::new(section.icon())
                            .size_4()
                            .text_color(if is_current { text } else { muted }),
                    )
                    .child(section.label())
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.set_settings_section(section, cx));
                    })
            }
        };
        let group_label = |title: &'static str| {
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .typo(Role::NavGroup, tokens)
                .child(title)
        };
        let back = {
            let entity = entity.clone();
            h_flex()
                .id("settings-back")
                .w_full()
                .gap_2()
                .items_center()
                .px_2()
                .py(px(7.0))
                .rounded_md()
                .typo(Role::Nav, tokens)
                .text_color(muted)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .tooltip(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new("返回应用 (Esc)").build(window, cx)
                })
                .child(Icon::new(IconName::ArrowLeft).size_4())
                .child("返回应用")
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.close_settings(window, cx));
                })
        };
        let manage_count = SettingsSection::MANAGE.len();
        v_flex()
            .id("settings-nav")
            .w(px(NAV_WIDTH))
            .h_full()
            .flex_none()
            .p_3()
            .gap_0p5()
            .bg(tokens.background)
            .border_r_1()
            .border_color(tokens.divider)
            .overflow_y_scroll()
            .child(back)
            .child(group_label("管理"))
            .children(
                SettingsSection::MANAGE
                    .into_iter()
                    .enumerate()
                    .map(|(index, section)| item(index, section)),
            )
            .child(group_label("偏好"))
            .children(
                SettingsSection::PREFERENCES
                    .into_iter()
                    .enumerate()
                    .map(move |(index, section)| item(manage_count + index, section)),
            )
    }

    // ---- 外观 ----

    fn appearance_blocks(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<::gpui::AnyElement> {
        let mut blocks = crate::settings_view::appearance_cards(&self.workspace, cx);
        blocks.push(self.host_tree_card(tokens, cx));
        blocks.push(self.window_card(tokens, cx));
        blocks.push(crate::settings_view::reset_row(&self.workspace));
        blocks
    }

    /// 主机树：不显示「未分组」文件夹（0.2.14 的 `HostTreeCard`，写同步键 `vault_ungrouped_flat`）。
    fn host_tree_card(&self, tokens: &Tokens, cx: &mut Context<Self>) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let info_entity = entity.clone();
        let guides_entity = entity.clone();
        card(
            "settings-host-tree",
            "主机树",
            None,
            tokens,
            vec![
                pref_switch(
                    "ungrouped-flat",
                    "主机树里不显示「未分组」文件夹，不分组的主机直接放在最下面",
                    "打开后，没有分组的主机排在所有分组下面，中间用一条线隔开。",
                    self.store.flat(),
                    tokens,
                    move |checked, window, cx| {
                        entity.update(cx, |this, cx| this.set_ungrouped_flat(checked, window, cx));
                    },
                ),
                pref_switch(
                    "host-info-panel",
                    "在侧栏显示主机属性面板",
                    "选中主机时，在侧栏底部显示名称、主机、用户名、协议、端口。默认关。",
                    self.prefs.host_info_panel,
                    tokens,
                    move |checked, window, cx| {
                        info_entity.update(cx, |this, cx| {
                            this.set_host_info_panel(checked, window, cx);
                        });
                    },
                ),
                pref_switch(
                    "host-tree-guides",
                    "主机树显示连接线",
                    "每一层子项左边画一条竖向的小虚线，横向连到子项，看得出谁属于哪个分组。只影响这台电脑。",
                    self.store.tree_guides(),
                    tokens,
                    move |checked, window, cx| {
                        guides_entity
                            .update(cx, |this, cx| this.set_tree_guides(checked, window, cx));
                    },
                ),
            ],
        )
    }

    /// 窗口：关闭主窗口 / 文件窗口时做什么（0.2.14 的 `WindowCard`），外加 ZeeTerm 的「启动时隐藏到托盘」。
    fn window_card(&self, tokens: &Tokens, cx: &mut Context<Self>) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let close = self.prefs.close_action;
        let file_close = self.prefs.file_window_close;
        let close_entity = entity.clone();
        let file_entity = entity.clone();
        let start_hidden = self.workspace.read(cx).settings().start_hidden;
        let ws = self.workspace.clone();
        card(
            "settings-window",
            "窗口",
            None,
            tokens,
            vec![
                row(
                    "关闭主窗口时",
                    Some(
                        "缩到托盘后会话、端口转发、AI 命令都继续运行，右键托盘图标选「退出」才真正退出。最小化仍然缩到任务栏。",
                    ),
                    div().w(px(224.0)).child(select(
                        "close-action",
                        match close {
                            CloseChoice::Tray => "缩到托盘",
                            CloseChoice::Quit => "退出程序",
                        },
                        vec![
                            Choice::new("缩到托盘", close == CloseChoice::Tray),
                            Choice::new("退出程序", close == CloseChoice::Quit),
                        ],
                        move |index, window, cx| {
                            let choice = if index == 0 {
                                CloseChoice::Tray
                            } else {
                                CloseChoice::Quit
                            };
                            close_entity
                                .update(cx, |this, cx| this.set_close_choice(choice, window, cx));
                        },
                    )),
                    tokens,
                ),
                row(
                    "关闭文件窗口时",
                    Some(
                        "断开并关闭：窗口里的 SFTP 全部断开（有传输在跑会先问）。合并回主窗口：标签原样交回主窗口，连接和传输都不中断。单个标签上的 × 始终是断开。",
                    ),
                    div().w(px(224.0)).child(select(
                        "file-window-close",
                        match file_close {
                            FileWindowChoice::Close => "断开并关闭",
                            FileWindowChoice::Merge => "合并回主窗口",
                        },
                        vec![
                            Choice::new("断开并关闭", file_close == FileWindowChoice::Close),
                            Choice::new("合并回主窗口", file_close == FileWindowChoice::Merge),
                        ],
                        move |index, window, cx| {
                            let choice = if index == 0 {
                                FileWindowChoice::Close
                            } else {
                                FileWindowChoice::Merge
                            };
                            file_entity.update(cx, |this, cx| {
                                this.set_file_window_choice(choice, window, cx);
                            });
                        },
                    )),
                    tokens,
                ),
                crate::settings_view::switch_row(
                    "start-hidden",
                    "启动时隐藏到托盘",
                    "下次启动不显示主窗口，从托盘图标打开",
                    start_hidden,
                    |this, cx| this.toggle_start_hidden(cx),
                    &ws,
                    tokens,
                ),
            ],
        )
    }

    // ---- 终端 ----

    fn terminal_blocks(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<::gpui::AnyElement> {
        // 关键词高亮的规则编辑器有自己的输入框，是个实体：第一次进「终端」时建，之后一直留着。
        let editor = match &self.highlight_editor {
            Some(editor) => editor.clone(),
            None => {
                let ws = self.workspace.clone();
                let editor = cx.new(|cx| crate::highlight_editor::HighlightEditor::new(ws, cx));
                self.highlight_editor = Some(editor.clone());
                editor
            }
        };
        let mut blocks = crate::settings_view::terminal_cards(&self.workspace, cx);
        blocks.push(editor.into_any_element());
        blocks.push(self.session_log_card(tokens, cx));
        blocks
    }

    /// 会话日志：开关、目录、打开目录（0.2.14 的 `SessionLogSection`）。
    fn session_log_card(&self, tokens: &Tokens, cx: &mut Context<Self>) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let toggle_entity = entity.clone();
        let (pick_entity, reset_entity, open_entity) =
            (entity.clone(), entity.clone(), entity.clone());
        let has_custom_dir = !self.prefs.session_log_dir.is_empty();
        // 目录只显示脱敏后的路径（用户目录换成 %USERPROFILE%），截图和录屏不带出用户名。
        let shown_dir = crate::about_view::display_path(&self.prefs.session_log_dir_path());
        // 日志出过的问题：本次运行里的（写失败、建不成文件、磁盘跟不上）加上上次退出时留下的。
        let mut problems = crate::app_prefs::session_log_problem_lines();
        if !self.prefs.session_log_exit_note.is_empty() {
            problems.push(format!("上次退出时：{}", self.prefs.session_log_exit_note));
        }
        card(
            "settings-session-log",
            "会话日志",
            Some(
                "把每个终端会话里显示的内容存成纯文本文件（去掉颜色和光标控制），对之后新打开的会话生效。日志里会有终端上显示过的一切，可能包括敏感信息，只存在本机。",
            ),
            tokens,
            vec![
                pref_switch(
                    "session-log-enabled",
                    "记录会话日志",
                    "默认关。",
                    self.prefs.session_log_enabled,
                    tokens,
                    move |checked, window, cx| {
                        toggle_entity.update(cx, |this, cx| {
                            this.set_session_log_enabled(checked, window, cx);
                        });
                    },
                ),
                row_with(
                    "日志目录",
                    Some(SharedString::from(shown_dir)),
                    h_flex()
                        .gap_1()
                        .when(has_custom_dir, |d| {
                            d.child(
                                Button::new("session-log-reset")
                                    .ghost()
                                    .compact()
                                    .label("恢复默认")
                                    .on_click(move |_, window, cx| {
                                        reset_entity.update(cx, |this, cx| {
                                            this.set_session_log_dir(String::new(), window, cx);
                                        });
                                    }),
                            )
                        })
                        .child(
                            Button::new("session-log-pick")
                                .outline()
                                .compact()
                                .label("选择…")
                                .on_click(move |_, window, cx| {
                                    pick_entity.update(cx, |this, cx| {
                                        this.pick_session_log_dir(window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("session-log-open")
                                .outline()
                                .compact()
                                .label("打开目录")
                                .on_click(move |_, window, cx| {
                                    open_entity.update(cx, |this, cx| {
                                        this.open_session_log_dir(window, cx);
                                    });
                                }),
                        ),
                    tokens,
                ),
            ]
            .into_iter()
            .chain((!problems.is_empty()).then(|| {
                row_with(
                    "日志有问题",
                    Some(SharedString::from(problems.join(
                        "
",
                    ))),
                    div(),
                    tokens,
                )
            }))
            .collect(),
        )
    }

    fn set_session_log_enabled(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_flag(db, app_prefs::SESSION_LOG_ENABLED_KEY, enabled));
        match result {
            Ok(()) => {
                self.prefs.session_log_enabled = enabled;
                // 连接时读的是缓存的设置（不在 UI 线程开库），改完要刷新。
                crate::app_prefs::refresh_cached(self.prefs.clone());
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    /// 「在侧栏显示主机属性面板」：只写本机设置；关掉时顺手清掉已经展开的那台。
    fn set_host_info_panel(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_flag(db, app_prefs::HOST_INFO_PANEL_KEY, enabled));
        match result {
            Ok(()) => {
                self.prefs.host_info_panel = enabled;
                if !enabled {
                    self.info_host = None;
                }
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_session_log_dir(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dir.trim().to_owned();
        let result = self
            .store
            .db()
            .and_then(|db| app_prefs::write_text(db, app_prefs::SESSION_LOG_DIR_KEY, &dir));
        match result {
            Ok(()) => {
                self.prefs.session_log_dir = dir;
                // 连接时读的是缓存的设置（不在 UI 线程开库），改完要刷新。
                crate::app_prefs::refresh_cached(self.prefs.clone());
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    /// 「选择…」：系统的选择文件夹对话框，选中的目录存进设置。
    fn pick_session_log_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择会话日志目录".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(picked) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_session_log_dir(picked.display().to_string(), window, cx);
            });
        })
        .detach();
    }

    /// 「打开目录」：用资源管理器打开（还没有就先建出来）。
    fn open_session_log_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.prefs.session_log_dir_path();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.notice_error(&format!("建不了目录：{err}"), window, cx);
            return;
        }
        if let Err(err) = std::process::Command::new("explorer").arg(&dir).spawn() {
            self.notice_error(&format!("打不开资源管理器：{err}"), window, cx);
        }
    }

    // ---- 连接 ----

    /// 连接分类的三组：代理、连接保持（空闲断开、保活）、服务器状态栏。
    fn connection_cards(
        &self,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<::gpui::AnyElement> {
        let entity = cx.entity();
        let proxies = self.store.proxies();
        let db = self.store.db().ok();
        let default_id = db.and_then(|db| db.default_proxy_id().ok().flatten());
        let app_mode = AppProxyMode::parse(
            db.and_then(|db| db.setting_get(APP_NETWORK_PROXY_KEY).ok().flatten())
                .as_deref(),
        );
        let proxy_label = |p: &zssh_core::model::Proxy| {
            format!("{}（{}）", p.name, crate::proxies::kind_label(p.kind))
        };

        // SSH 默认代理
        let mut default_choices = vec![Choice::new("不使用默认代理", default_id.is_none())];
        let mut default_targets: Vec<Option<i64>> = vec![None];
        for proxy in proxies {
            default_choices.push(Choice::new(
                proxy_label(proxy),
                default_id == Some(proxy.id),
            ));
            default_targets.push(Some(proxy.id));
        }
        let default_label = default_id
            .and_then(|id| proxies.iter().find(|p| p.id == id))
            .map_or_else(|| "不使用默认代理".to_owned(), proxy_label);
        let default_entity = entity.clone();
        let manage_entity = entity.clone();
        let mut default_control =
            v_flex()
                .w(px(224.0))
                .gap_1p5()
                .items_end()
                .child(div().w_full().child(select(
                    "ssh-default-proxy",
                    default_label,
                    default_choices,
                    move |index, window, cx| {
                        if let Some(target) = default_targets.get(index).copied() {
                            default_entity
                                .update(cx, |this, cx| this.set_default_proxy(target, window, cx));
                        }
                    },
                )));
        if proxies.is_empty() {
            default_control = default_control.child(
                div()
                    .w_full()
                    .typo(Role::Caption, tokens)
                    .child("还没有代理，点下面的「新建 / 管理代理」添加一个。"),
            );
        }
        default_control = default_control.child(
            Button::new("manage-proxies")
                .outline()
                .compact()
                .icon(IconName::Network)
                .label("新建 / 管理代理")
                .w_full()
                .on_click(move |_, _, cx| {
                    manage_entity.update(cx, |this, cx| {
                        this.set_settings_section(SettingsSection::Proxies, cx);
                    });
                }),
        );

        // 软件网络
        let mut app_choices = vec![
            Choice::new("使用 SSH 默认代理", app_mode == AppProxyMode::SshDefault),
            Choice::new("跟随系统代理", app_mode == AppProxyMode::System),
            Choice::new("不使用代理", app_mode == AppProxyMode::Direct),
        ];
        // 存的值和 `AppProxyMode::parse` 读的是同一套：选「使用 SSH 默认代理」就明确存 `ssh_default`，
        // 没有值才是从没选过（读作「跟随系统代理」）。
        let mut app_values: Vec<String> = [
            AppProxyMode::SshDefault,
            AppProxyMode::System,
            AppProxyMode::Direct,
        ]
        .iter()
        .map(AppProxyMode::setting_value)
        .collect();
        for proxy in proxies {
            app_choices.push(Choice::new(
                proxy_label(proxy),
                app_mode == AppProxyMode::Proxy(proxy.id),
            ));
            app_values.push(AppProxyMode::Proxy(proxy.id).setting_value());
        }
        // 选中的代理被删了：先显示成「使用 SSH 默认代理」，免得下拉框是空的（同 0.2.14 的 `appProxyChoice`）。
        let app_label = match &app_mode {
            AppProxyMode::System => "跟随系统代理".to_owned(),
            AppProxyMode::Direct => "不使用代理".to_owned(),
            AppProxyMode::Proxy(id) => proxies
                .iter()
                .find(|p| p.id == *id)
                .map_or_else(|| "使用 SSH 默认代理".to_owned(), proxy_label),
            AppProxyMode::SshDefault => "使用 SSH 默认代理".to_owned(),
        };
        let app_entity = entity.clone();
        let mut app_control = v_flex().w(px(224.0)).gap_1p5().child(select(
            "app-network-proxy",
            app_label,
            app_choices,
            move |index, window, cx| {
                if let Some(raw) = app_values.get(index).cloned() {
                    app_entity.update(cx, |this, cx| this.set_app_network_proxy(&raw, window, cx));
                }
            },
        ));
        if app_mode == AppProxyMode::System {
            app_control = app_control.child(
                div()
                    .id("system-proxy-text")
                    .typo(Role::Caption, tokens)
                    .child(system_proxy_text(&system_proxy_now())),
            );
        }

        let stats_entity = entity.clone();
        vec![
            card(
                "settings-connection",
                "代理",
                None,
                tokens,
                vec![
                    row(
                        "SSH 默认代理",
                        Some("只管 SSH 连接：「跟随默认代理」的主机和快速连接走这一个。"),
                        default_control,
                        tokens,
                    ),
                    row(
                        "软件网络",
                        Some(
                            "检查和下载更新、配置同步、下载 opencode 走这里，和 SSH 的代理分开。",
                        ),
                        app_control,
                        tokens,
                    ),
                ],
            ),
            card(
                "settings-connection-keep",
                "连接保持",
                None,
                tokens,
                vec![
                self.number_row(
                    PrefNumber::PoolIdle,
                    "连接空闲断开",
                    "终端、SFTP、端口转发等都不再使用这条连接之后，再空转多久断开；0 = 立刻断开。改动对已经空闲的连接也生效，从改动那一刻起重新计时。",
                    "s",
                    tokens,
                    cx,
                ),
                self.number_row(
                    PrefNumber::Keepalive,
                    "保活间隔",
                    "多久发一次 keepalive 探测断线；0 = 关闭。改完对之后新建的连接生效。",
                    "s",
                    tokens,
                    cx,
                ),
                ],
            ),
            card(
                "settings-connection-stats",
                "服务器状态栏",
                None,
                tokens,
                vec![
                pref_switch(
                    "stats-show",
                    "显示服务器状态栏",
                    "连上后在终端底部显示 CPU / 内存 / 磁盘 / 网速。",
                    self.prefs.stats_show,
                    tokens,
                    move |checked, window, cx| {
                        stats_entity.update(cx, |this, cx| {
                            this.set_pref_flag(PrefFlag::StatsShow, checked, window, cx);
                        });
                    },
                ),
                self.number_row(
                    PrefNumber::StatsRefresh,
                    "状态栏刷新间隔",
                    "多久采一次服务器状态；状态栏关掉时这项不起作用。",
                    "s",
                    tokens,
                    cx,
                ),
                ],
            ),
        ]
    }

    // ---- SFTP ----

    fn sftp_card(&self, tokens: &Tokens, cx: &mut Context<Self>) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let flag = |kind: PrefFlag,
                    id: &'static str,
                    title: &'static str,
                    description: &'static str,
                    checked: bool| {
            let entity = entity.clone();
            pref_switch(
                id,
                title,
                description,
                checked,
                tokens,
                move |value, window, cx| {
                    entity.update(cx, |this, cx| this.set_pref_flag(kind, value, window, cx));
                },
            )
        };
        card(
            "settings-sftp",
            "传输",
            Some("传输参数对之后打开的 SFTP 标签生效（正在传的任务不受影响）。"),
            tokens,
            vec![
                self.number_row(
                    PrefNumber::SftpConcurrency,
                    "同时传输的文件数",
                    "1～8，默认 3",
                    " 个",
                    tokens,
                    cx,
                ),
                flag(
                    PrefFlag::SftpPreserveMtime,
                    "sftp-preserve-mtime",
                    "保留修改时间",
                    "传完之后把源文件的修改时间设到目标上（远端走 setstat，本地按文件时间）。",
                    self.prefs.sftp_preserve_mtime,
                ),
                flag(
                    PrefFlag::SftpPreservePermissions,
                    "sftp-preserve-permissions",
                    "保留权限",
                    "远端按源的权限位设置；本地只有只读一位有意义。",
                    self.prefs.sftp_preserve_permissions,
                ),
                flag(
                    PrefFlag::SftpShowHidden,
                    "sftp-show-hidden",
                    "默认显示隐藏文件",
                    "两栏的初始开关；每个目录的隐藏文件按系统属性（远端按名字加点）判断。",
                    self.prefs.sftp_show_hidden,
                ),
            ],
        )
    }

    /// 一行滑块设置：左边名字和说明，右边滑块 + 当前值。
    fn number_row(
        &self,
        kind: PrefNumber,
        title: &'static str,
        description: &'static str,
        suffix: &'static str,
        tokens: &Tokens,
        cx: &App,
    ) -> ::gpui::AnyElement {
        let slider = self.pref_sliders.of(kind).clone();
        let shown = slider.read(cx).value().end().round() as u32;
        let saved = match kind {
            PrefNumber::Keepalive => self.prefs.keepalive_secs,
            PrefNumber::PoolIdle => self.prefs.pool_idle_secs,
            PrefNumber::StatsRefresh => self.prefs.stats_refresh_secs,
            PrefNumber::SftpConcurrency => self.prefs.sftp_concurrency,
        };
        // 拖的过程中数字用次级色：松手才写库。
        let pending = shown != saved;
        row(
            title,
            Some(description),
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .w(px(150.0))
                        .child(Slider::new(&slider).horizontal().accessibility_label(title)),
                )
                .child(
                    div()
                        .w(px(60.0))
                        .text_sm()
                        .text_right()
                        .text_color(if pending {
                            tokens.text_muted
                        } else {
                            tokens.text
                        })
                        .child(SharedString::from(format!("{shown}{suffix}"))),
                ),
            tokens,
        )
    }
}

/// 一行开关设置（读写数据库设置的那些）：左边名字和说明，右边开关。
fn pref_switch(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    checked: bool,
    tokens: &Tokens,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> ::gpui::AnyElement {
    row(
        title,
        Some(description),
        Switch::new(id)
            .checked(checked)
            .accessibility_label(title)
            .on_click(move |checked: &bool, window, cx| on_toggle(*checked, window, cx)),
        tokens,
    )
}

/// 「跟随系统代理」时显示读到的系统代理（同 0.2.14 的 `systemProxyText`）。
/// 设置页显示的「现在的系统代理」。调试钩子 `ZSSH_FAKE_SYSTEM_PROXY`（`none` 或 `host:port`）可以代替
/// 真实注册表，截图不会拍到开发机自己的代理地址；release 里钩子不存在。
fn system_proxy_now() -> SystemProxy {
    match crate::dev_hooks::var("ZSSH_FAKE_SYSTEM_PROXY").as_deref() {
        Some("none") => SystemProxy::None,
        Some(server) => zssh_core::sync::app_proxy::parse_system_proxy(true, Some(server), None),
        None => zssh_core::sync::app_proxy::read_system_proxy(),
    }
}

fn system_proxy_text(proxy: &SystemProxy) -> String {
    match proxy {
        SystemProxy::Manual { scheme, host, port } => format!(
            "系统代理：{}{host}:{port}",
            if scheme == "socks5" { "SOCKS5 " } else { "" }
        ),
        SystemProxy::AutoConfig { .. } => {
            "系统用的是自动配置脚本（PAC），暂不支持，按直连处理".to_owned()
        }
        SystemProxy::None => "系统没有设置代理，按直连处理".to_owned(),
    }
}

/// 按用途展示原有只读快捷键，索引仍指向唯一的数据清单。
const SHORTCUT_GROUPS: &[(&str, &[usize])] = &[
    ("标签与窗口", &[0, 1, 4, 5, 14, 15]),
    ("终端与分屏", &[2, 3, 6, 7, 10]),
    ("终端显示与操作", &[11, 12, 13]),
    ("面板", &[8, 9]),
];

fn shortcuts_cards(tokens: &Tokens) -> Vec<::gpui::AnyElement> {
    SHORTCUT_GROUPS
        .iter()
        .enumerate()
        .map(|(group_index, (title, indices))| {
            let rows = indices
                .iter()
                .map(|&index| {
                    let shortcut = &SHORTCUTS[index];
                    h_flex()
                        .id(("shortcut-row", index))
                        .w_full()
                        .min_h(px(28.0))
                        .flex_wrap()
                        .gap_3()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(140.0))
                                .typo(Role::Body, tokens)
                                .child(shortcut.what),
                        )
                        .child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .border_1()
                                .border_color(tokens.border)
                                .bg(tokens.background)
                                .text_xs()
                                .font_family("Cascadia Mono")
                                .text_color(tokens.text)
                                .child(shortcut.keys),
                        )
                        .into_any_element()
                })
                .collect();
            let id = match group_index {
                0 => "settings-shortcuts",
                1 => "settings-shortcuts-terminal",
                2 => "settings-shortcuts-editing",
                _ => "settings-shortcuts-panels",
            };
            card(
                id,
                title,
                (group_index == 0).then_some("当前快捷键不可自定义。"),
                tokens,
                rows,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{SHORTCUT_GROUPS, SystemProxy, app_prefs, system_proxy_text};
    use crate::settings_nav::SHORTCUTS;

    #[test]
    fn shortcut_groups_include_each_existing_shortcut_once() {
        let mut indices: Vec<usize> = SHORTCUT_GROUPS
            .iter()
            .flat_map(|(_, items)| items.iter().copied())
            .collect();
        indices.sort_unstable();
        assert_eq!(indices, (0..SHORTCUTS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn system_proxy_text_matches_0_2_14() {
        assert_eq!(
            system_proxy_text(&SystemProxy::None),
            "系统没有设置代理，按直连处理"
        );
        assert_eq!(
            system_proxy_text(&SystemProxy::Manual {
                scheme: "http".into(),
                host: "proxy.example".into(),
                port: 8080
            }),
            "系统代理：proxy.example:8080"
        );
        assert_eq!(
            system_proxy_text(&SystemProxy::Manual {
                scheme: "socks5".into(),
                host: "proxy.example".into(),
                port: 1080
            }),
            "系统代理：SOCKS5 proxy.example:1080"
        );
        assert!(
            system_proxy_text(&SystemProxy::AutoConfig {
                url: "http://pac.example/x.pac".into()
            })
            .contains("PAC")
        );
    }

    #[test]
    fn keepalive_default_matches_the_core_default() {
        // 设置页的默认值和 core 里没配置时的 30 秒一致。
        assert_eq!(app_prefs::KEEPALIVE_DEFAULT, 30);
    }
}
