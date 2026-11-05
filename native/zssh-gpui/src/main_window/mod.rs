//! 主窗口：无边框窗口 + 自绘标题栏（会话标签）+ 可折叠主机树 + 主区
//! （欢迎页 / 主机页 / 终端 / 设置与管理）+ 底部状态栏。
//!
//! 行为规范是 0.2.14 前端（`ui/src/App.tsx`、`components/titlebar/TopTabs.tsx`）。终端本身
//! 是 [`TerminalWorkspace`]（和调试窗口共用）；这里只做外壳：标签条、主机树、页面切换、
//! 快捷键、窗口状态。AI 面板、SFTP 停靠、快捷命令栏、撰写栏在 G07c。
//!
//! 文件分工：`nav`（页面切换规则）、`host_tree` / `page_logic`（主机树和主机页的纯逻辑）、
//! `host_store`（读写主机库）、`titlebar`、`sidebar`（主机树）、`vault_page`（主机页）、
//! `vault_actions` / `vault_menus` / `drag`（主机页和树上的动作、右键菜单、拖放）、
//! `side_panel`（右侧面板框架）和它上面的 `host_panel` / `proxy_panel` / `quick_panel`、
//! `modal`（对话框）、`pages`（终端页、空白标签的选主机）、`welcome`（欢迎页）、
//! `settings_page`（「管理 + 设置」统一页，管理组里的凭据 / 快捷命令 / 代理 / 端口转发复用
//! `keychain_panel` / `quick_panel` / `proxy_panel` / `forward_panel` 的列表和表单）、
//! `status_bar`（底部状态栏）。

mod ai_builtin;
pub(crate) mod ai_endpoint_dialog;
pub(crate) mod ai_presets;
pub mod anchor;
mod app_lock_ui;
pub mod bar_height;
pub mod bar_settings;
mod bars;
pub(crate) mod bridge;
mod close_guard;
pub mod collation;
pub mod compose_history;
mod connections_ui;
// 调试驱动整套（`ZSSH_MAIN_SCRIPT` 步骤脚本 + 每张任务卡的动作实现 + 种子数据）只在
// debug 构建里编译：release 里这些模块不存在，`dev_script::spawn_from_env` 这类入口
// 也一起从 main.rs 门掉。给自动化加新动作时放这里，别放无门禁的模块。
#[cfg(debug_assertions)]
mod dev_acts;
#[cfg(debug_assertions)]
mod dev_acts_a4;
#[cfg(debug_assertions)]
mod dev_acts_a5;
#[cfg(debug_assertions)]
mod dev_acts_about;
#[cfg(debug_assertions)]
mod dev_acts_agent;
#[cfg(debug_assertions)]
mod dev_acts_c1;
#[cfg(debug_assertions)]
mod dev_acts_dock;
#[cfg(debug_assertions)]
mod dev_acts_edit;
#[cfg(debug_assertions)]
mod dev_acts_n4;
#[cfg(debug_assertions)]
mod dev_acts_pane;
#[cfg(debug_assertions)]
mod dev_acts_sync;
#[cfg(debug_assertions)]
mod dev_acts_tcp;
#[cfg(debug_assertions)]
mod dev_acts_u1;
#[cfg(debug_assertions)]
mod dev_acts_u13;
#[cfg(debug_assertions)]
mod dev_acts_u19;
#[cfg(debug_assertions)]
mod dev_acts_u2;
#[cfg(debug_assertions)]
mod dev_acts_u20;
#[cfg(debug_assertions)]
mod dev_acts_u25;
#[cfg(debug_assertions)]
mod dev_acts_u26;
#[cfg(debug_assertions)]
mod dev_acts_u28;
#[cfg(debug_assertions)]
mod dev_acts_u29;
#[cfg(debug_assertions)]
mod dev_acts_u30;
#[cfg(debug_assertions)]
mod dev_acts_u34;
#[cfg(debug_assertions)]
mod dev_acts_u35;
#[cfg(debug_assertions)]
mod dev_acts_u4;
#[cfg(debug_assertions)]
mod dev_acts_u6;
#[cfg(debug_assertions)]
mod dev_acts_u7;
// U12 / U18 / U21 是双模式：MainWindow 上挂着 `DevFake` 假数据槽，生产 UI（connections_ui /
// processes_ui）在 release 里调它的 always-None 查询，所以这三个模块无门禁（假数据逻辑和
// 其余动作实现只在 debug 编译，release 里只剩空槽和 always-None 查询）。
mod dev_acts_u12;
mod dev_acts_u18;
mod dev_acts_u21;
// 脚本解析器整套只编进 debug 构建；入口 main.rs 的调用一起门掉。
#[cfg(debug_assertions)]
pub mod dev_script;
#[cfg(debug_assertions)]
pub mod dev_seed;
pub mod dock_layout;
mod docks;
mod drag;
mod drag_scroll;
pub(crate) mod editor_tabs;
mod forward_panel;
mod host_delete;
mod host_dialog;
mod host_form;
mod host_panel;
mod host_props;
pub mod host_store;
mod host_test;
pub mod host_tree;
mod identity_form;
mod info_panel;
mod key_dialogs;
mod keychain_panel;
mod migrate_ui;
mod modal;
pub mod model_picker;
pub mod nav;
mod overview;
pub mod page_logic;
mod pages;
pub mod panel_width;
mod privacy;
mod processes_ui;
mod protocol_caps;
pub mod protocol_form;
mod proxy_panel;
pub mod quick_commands;
mod quick_panel;
mod recovery_ui;
mod reveal_ui;
pub mod scope_targets;
mod settings_page;
mod side_panel;
mod sidebar;
mod status_bar;
mod sync_bar;
mod tab_menu;
mod tab_move;
pub(crate) mod tab_strip;
pub(crate) mod titlebar;
mod tree_guides;
mod vault_actions;
mod vault_keys;
mod vault_menus;
mod vault_page;
mod welcome;
pub(crate) mod widgets;
mod x11_ui;

use std::collections::BTreeSet;
use std::time::Duration;

use ::gpui::{
    Entity, FocusHandle, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, Subscription,
    Window,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::split_tree::Axis;
use crate::terminal_workspace::TerminalWorkspace;
use crate::typography::UiFontStyled as _;
use crate::window_state::{self, MainLayout, WindowState};
use drag::DropHint;
use forward_panel::ForwardUi;
use host_panel::HostForm;
use host_store::HostStore;
use host_tree::UNGROUPED_KEY;
use keychain_panel::KeychainUi;
use modal::Modal;
use nav::{MainView, ViewNav};
use panel_width::DragStart;
use proxy_panel::ProxyUi;
use side_panel::PanelState;

/// 主机树当前选中的分组（主机页据此过滤卡片）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSel {
    All,
    Ungrouped,
    Group(i64),
}

/// 主机树的界面状态。
pub struct TreeState {
    pub width: f32,
    pub collapsed: bool,
    pub expanded: BTreeSet<String>,
    pub selected_group: GroupSel,
    /// 单击选中的主机（高亮）。
    pub selected_host: Option<i64>,
    /// 搜索时键盘移动到的那一台（回车连它；没动过连第一台）。
    pub cursor_host: Option<i64>,
    /// 正在拖动宽度：按下时指针到右边缘的偏移，拖动中用它换算。
    pub resizing: bool,
}

/// 「关闭标签」的几种范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseScope {
    Others,
    Left,
    Right,
    All,
}

use crate::i18n::{Msg, t};

/// 主窗口外壳自己的文案（搜索框占位、标签动作的提示、调试用的隐私检查说明）。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // 有些键只在测试 / 调试构建里用到
enum ShellMsg {
    SearchHosts,
    SearchVaultLong,
    ManageSearch,
    QuickTargetPlaceholder,
    ConnSearch,
    ProcSearch,
    SerialDuplicateBlocked,
    NothingToDuplicate,
    BreakSent,
    BreakNotConnected,
    CopiedPrefix,
    NoAddressToCopy,
    NoHostRecord,
    /// 隐私检查命中调用方给的禁用片段：本键 + 序号。
    ForbiddenFragmentPrefix,
    /// 隐私检查命中内置规则：本键 + 类别名。
    BuiltinRulePrefix,
}

impl Msg for ShellMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::SearchHosts => "搜索主机",
            Self::SearchVaultLong => "搜索主机名、地址、标签，回车连接",
            Self::ManageSearch => "按名称、地址搜索",
            Self::QuickTargetPlaceholder => "user@host 或 user@host:端口",
            Self::ConnSearch => "按地址、端口、进程名搜索",
            Self::ProcSearch => "按 PID、用户、命令搜索",
            Self::SerialDuplicateBlocked => "串口同一时间只能开一个标签，不能复制",
            Self::NothingToDuplicate => "这个标签还没连主机，没有可复制的",
            Self::BreakSent => "已发送 Break",
            Self::BreakNotConnected => "这个标签没有连着，不能发送 Break",
            Self::CopiedPrefix => "已复制：",
            Self::NoAddressToCopy => "这个标签没有可复制的地址",
            Self::NoHostRecord => "这个标签没有对应的主机记录",
            Self::ForbiddenFragmentPrefix => "禁用片段 #",
            Self::BuiltinRulePrefix => "内置规则：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::SearchHosts => "Search hosts",
            Self::SearchVaultLong => "Search host name, address, or tag — press Enter to connect",
            Self::ManageSearch => "Search by name or address",
            Self::QuickTargetPlaceholder => "user@host or user@host:port",
            Self::ConnSearch => "Search by address, port, or process name",
            Self::ProcSearch => "Search by PID, user, or command",
            Self::SerialDuplicateBlocked => {
                "A serial port can only be open in one tab, so it cannot be duplicated"
            }
            Self::NothingToDuplicate => {
                "This tab is not connected to a host yet, so there is nothing to duplicate"
            }
            Self::BreakSent => "Break sent",
            Self::BreakNotConnected => "This tab is not connected, so Break cannot be sent",
            Self::CopiedPrefix => "Copied: ",
            Self::NoAddressToCopy => "This tab has no address to copy",
            Self::NoHostRecord => "This tab has no matching host record",
            Self::ForbiddenFragmentPrefix => "Forbidden fragment #",
            Self::BuiltinRulePrefix => "Built-in rule: ",
        }
    }
}

pub struct MainWindow {
    pub(super) workspace: Entity<TerminalWorkspace>,
    /// SSH 提问（指纹确认、密码输入）的应用内对话框层（V2-U29）：盖在窗口最上面。
    pub(super) ssh_prompt: Entity<crate::ssh_prompt_ui::SshPromptLayer>,
    /// 根元素的焦点：没有输入框或终端有焦点时，快捷键靠它也能收到。
    pub(super) focus: FocusHandle,
    pub(super) nav: ViewNav,
    pub(super) store: HostStore,
    /// 上一次看到的库版本（`PRAGMA data_version`）：变了才重读主机库（审查 M-14）。
    pub(super) db_version: Option<i64>,
    pub(super) tree: TreeState,
    pub(super) tree_query: Entity<InputState>,
    pub(super) vault_query: Entity<InputState>,
    pub(super) launcher_query: Entity<InputState>,
    /// 「管理 + 设置」页里凭据 / 代理 / 端口转发列表上方的搜索框（换分类时清空）。
    pub(super) manage_search: Entity<InputState>,
    /// 换了分类：下次画页面时清空搜索框（清空要用到 `Window`，所以先记着）。
    pub(super) manage_search_reset: bool,
    /// 欢迎页「快速连接」弹窗里的 `user@host[:port]` 输入框。
    pub(super) quick_target: Entity<InputState>,
    /// 「所有网络连接」对话框的搜索框，和给迟到结果对号的采集编号（V2-U12）。
    pub(super) conn_query: Entity<InputState>,
    pub(super) conn_generation: u64,
    /// 「进程管理」对话框的搜索框和打开计数（V2-U18）。
    pub(super) proc_query: Entity<InputState>,
    pub(super) proc_generation: u64,
    /// 调试驱动喂的假数据（概览面板 / 连接列表），release 里是空结构。
    pub(super) dev_fake: dev_acts_u12::DevFake,
    /// 空白标签里选主机的光标位置。
    pub(super) launcher_cursor: usize,
    /// 设置页 AI 一节（智能体检测 / 路径 / 下载）：第一次打开设置页时才建。
    pub(super) agent_settings: Option<Entity<crate::agents::AgentsDemo>>,
    /// 设置页同步一节（连接存储 / 主密码 / 同步 / 冲突）和备份一节：第一次打开设置页时建。
    pub(super) sync_settings: Option<Entity<crate::sync::SyncDemo>>,
    pub(super) backup_settings: Option<Entity<crate::backup::BackupDemo>>,
    /// 设置页同步一节最上面的「从 Z-SSH 迁移」（V2-U19）：欢迎页的入口也会建它。
    pub(super) migrate_settings: Option<Entity<crate::migrate::MigrateDemo>>,
    /// 欢迎页「检测到 Z-SSH 的数据」提示卡的状态。
    pub(super) migrate_hint: migrate_ui::MigrateHint,
    /// 设置页「关于」一节（版本 / 检查更新 / 数据目录 / 开源许可）：第一次打开设置页时建。
    pub(super) about_settings: Option<Entity<crate::about_view::AboutView>>,
    /// 底部同步状态条现在画的内容（版本号用来判断要不要重绘）。
    pub(super) sync_bar: (u64, crate::sync::SyncBarModel),
    /// 设置页的滚动位置（驱动脚本要把某一块滚到可见处截图）。
    pub(super) settings_scroll: ::gpui::ScrollHandle,
    /// 设置页现在显示哪个分类（记在窗口状态里，重启后恢复）。
    pub(super) settings_section: crate::settings_nav::SettingsSection,
    /// 数据库里的连接 / SFTP / 窗口设置（`app_prefs`）：进设置页时重读，改的时候同步更新。
    pub(super) prefs: crate::app_prefs::AppPrefs,
    /// 锁屏（V2-U26）：库里的设置、主窗口的锁屏页、忘记密码验证的进度。
    pub(super) app_lock: app_lock_ui::AppLockUi,
    /// 数值设置的滑块。
    pub(super) pref_sliders: settings_page::PrefSliders,
    /// 设置页「终端 → 关键词高亮」的规则编辑器：第一次进「终端」分类时才建。
    pub(super) highlight_editor: Option<Entity<crate::highlight_editor::HighlightEditor>>,
    /// 右侧面板内容区的滚动句柄（调试驱动把它滚到底，看面板下半部分用）。
    pub(super) panel_scroll: ::gpui::ScrollHandle,
    /// AI 改主机记忆的账本我看到哪一条了（主机详情里开着的记忆框要跟着刷新）。
    pub(super) memory_seen: u64,
    /// 标签上 AI 活动点上次画出来的样子（变了才通知重画）。
    pub(super) ai_dots: Vec<(u64, bool)>,
    /// 状态栏正在订阅哪台主机的采集。
    pub(super) status_host: Option<i64>,
    pub(super) status: Option<crate::status::HostStatus>,
    pub(super) window_width: f32,
    pub(super) window_height: f32,
    tick_count: u64,
    /// 读过的 Tokenway 余额版本号：变了主窗口就重画（标题栏上的余额、设置页的余额）。
    tokenway_seen: u64,
    /// 启动时的恢复提示扫过没有（只扫一次）。
    recovery_scanned: bool,
    // ---- 右侧面板、对话框、主机页的界面状态 ----
    pub(super) panels: PanelState,
    /// 右侧面板的宽度（拖动、双击恢复、记在设置表里）。
    pub(super) aside_width: f32,
    pub(super) panel_drag: Option<DragStart>,
    /// 面板外壳的焦点：焦点在面板里时 Esc 才收起面板（终端里的 Esc 要留给远端）。
    pub(super) panel_focus: FocusHandle,
    pub(super) host_form: Option<HostForm>,
    /// 设置 → AI → 内置助手那一块的列表状态（V2-A4）。
    pub(super) ai_builtin: ai_builtin::AiBuiltinUi,
    /// X11：设置页那一节和「需要 X 显示程序」提示的状态（一键安装的进度也在这里）。
    pub(super) x11_ui: x11_ui::X11Ui,
    pub(super) keychain: KeychainUi,
    pub(super) proxy_ui: ProxyUi,
    pub(super) forward_ui: ForwardUi,
    pub(super) bars: bars::BarUi,
    pub(super) docks: docks::DockState,
    /// 标题栏标签条的溢出 / 滚动状态（V2-U1）。
    pub(super) tab_strip: tab_strip::TabStrip,
    /// 标签拖出窗口 / 拖到别的窗口的状态机（V2-U1 第 7 条，见 `tab_drag`）。
    pub(super) tab_dragger: crate::tab_drag::Dragger,
    /// 主机页每张卡片 / 每一行的键盘焦点句柄，和当前显示的次序（V2-U1：Tab、方向键、Enter、F2）。
    pub(super) host_focus: std::collections::HashMap<i64, FocusHandle>,
    pub(super) vault_nav: vault_keys::VaultNav,
    pub(super) quick_ui: quick_panel::QuickUi,
    /// 撰写栏翻历史时要填进输入框的内容（按键处理里拿不到 `Window`，渲染前再写）。
    pub(super) pending_compose_text: Option<String>,
    pub(super) modal: Option<Modal>,
    /// 查看 / 复制已保存密码前的锁屏密码验证小对话框（V2-U35）；压在 `modal` 上面，锁屏时丢掉。
    pub(super) reveal_ask: Option<reveal_ui::RevealAsk>,
    /// 树底部属性面板现在显示哪台主机。
    pub(super) info_host: Option<i64>,
    /// 属性面板里点了小眼睛取回来的明文密码（只属于 `info_host` 那台；隐藏 / 切换 / 收起 / 锁屏都丢掉）。
    pub(super) info_secret: Option<info_panel::InfoSecret>,
    /// 再点一次「收起」的待执行任务编号：双击连接时让它作废。
    pub(super) info_collapse_gen: u64,
    pub(super) drop_hint: Option<DropHint>,
    /// 拖动时靠近侧栏树 / 主机页上下沿自动滚动的状态（V2-U28）。
    pub(super) drag_scroll: drag_scroll::DragScroll,
    /// 「切换到手动排序后才能拖动」已经提示过一次。
    pub(super) mode_hint_shown: bool,
    /// 已经自动开启过转发的标签（标签 id）：同一次连接只开一次。
    autostarted_tabs: std::collections::HashSet<u64>,
    forward_seen: u64,
    _subs: Vec<Subscription>,
}

impl MainWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(|cx| TerminalWorkspace::new(window, cx, "main".to_owned()));
        let ssh_prompt = cx.new(|cx| crate::ssh_prompt_ui::SshPromptLayer::new(window, cx, true));
        let layout: MainLayout = WindowState::load().layout;
        let tree_query =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::SearchHosts)));
        let vault_query =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::SearchHosts)));
        let launcher_query =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::SearchVaultLong)));
        let manage_search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::ManageSearch)));
        let quick_target = cx
            .new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::QuickTargetPlaceholder)));
        let conn_query =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::ConnSearch)));
        let proc_query =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(ShellMsg::ProcSearch)));
        let saved_view = layout.view.as_deref().and_then(MainView::parse);

        let mut subs = Vec::new();
        // 终端工作区变了（标签、状态点、标题）标题栏跟着重画。
        subs.push(cx.observe(&workspace, |_this, _ws, cx| cx.notify()));
        // 输入框是 MainWindow 自己画出来的元素：它的状态（文字、光标、焦点）一变，主窗口要跟着重画。
        for input in [
            &tree_query,
            &vault_query,
            &launcher_query,
            &manage_search,
            &quick_target,
            &conn_query,
            &proc_query,
        ] {
            subs.push(cx.observe(input, |_this, _input, cx| cx.notify()));
        }
        // 输入框里的文字一变，搜索结果就变，键盘选中的那一项作废、回到第一项。只认文字变化：
        // 光标闪烁、移动也会让输入框通知界面，那些不能重置选中的项。
        subs.push(cx.subscribe_in(
            &tree_query,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.connect_from_tree_search(window, cx),
                InputEvent::Change => {
                    this.tree.cursor_host = None;
                    cx.notify();
                }
                _ => {}
            },
        ));
        subs.push(cx.subscribe_in(
            &vault_query,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.search_enter(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        ));
        // 连接列表的搜索框：文字一变就重算要显示的行（光标闪烁这类通知不算）。
        subs.push(cx.subscribe_in(
            &conn_query,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.connections_refilter(cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &proc_query,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.processes_refilter(cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &launcher_query,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.connect_from_launcher(window, cx),
                InputEvent::Change => {
                    this.launcher_cursor = 0;
                    cx.notify();
                }
                _ => {}
            },
        ));

        // 输入框自己绑定了 ↑ ↓（单行输入框里就是移光标），键盘事件里的动作比 `capture_key_down`
        // 先处理，所以搜索框里的 ↑ ↓ 要在按键分发之前拦下来。只管主窗口自己的按键。
        let entity = cx.entity().downgrade();
        let handle = window.window_handle();
        subs.push(cx.intercept_keystrokes(move |event, window, cx| {
            // Ctrl+Alt+L 锁定：任何一个 ZeeTerm 窗口里按都行（V2-U26）。
            let locked_now = entity
                .update(cx, |this, cx| this.lock_shortcut(event, cx))
                .unwrap_or(false);
            if locked_now {
                cx.stop_propagation();
                return;
            }
            if window.window_handle() != handle {
                return;
            }
            let _ = entity.update(cx, |this, cx| this.intercept_arrow(event, window, cx));
        }));

        let width_now = f32::from(window.viewport_size().width);
        let store = HostStore::open();
        let aside_width = panel_width::sanitize_stored(
            store
                .db()
                .ok()
                .and_then(|db| {
                    db.setting_get(panel_width::PanelKind::Aside.setting_key())
                        .ok()
                        .flatten()
                })
                .and_then(|raw| panel_width::parse_stored(Some(&raw))),
            panel_width::ASIDE_DEFAULT,
        );
        let prefs = store
            .db()
            .map(crate::app_prefs::AppPrefs::load)
            .unwrap_or_default();
        let pref_sliders = settings_page::PrefSliders::new(&prefs, cx);
        let app_lock = app_lock_ui::AppLockUi::load(store.db().ok());
        crate::tray::set_lock_menu(app_lock.config.status.is_enabled());
        // “启动时锁定”：第一帧就是锁屏页（V2-U35）。这时窗口还没画过，托盘和单实例唤起也只会看到
        // 锁屏页；会话、传输没有任何东西在启动时自动开，恢复提示等也等解锁后再出（见 `tick`）。
        app_lock_ui::apply_start_lock(&app_lock.config);
        let keychain = KeychainUi::new(window, cx);
        let proxy_ui = ProxyUi::new(window, cx);
        let bars = bars::BarUi::new(window, cx);
        let quick_ui = quick_panel::QuickUi::new(window, cx);
        subs.push(cx.subscribe_in(
            &bars.compose,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                bars::on_compose_event(this, event, window, cx)
            },
        ));
        subs.push(cx.observe(&bars.compose, |_this, _input, cx| cx.notify()));
        subs.push(cx.observe(&quick_ui.command, |_this, _input, cx| cx.notify()));
        for input in quick_ui.input_entities() {
            subs.push(cx.observe(&input, |_this, _input, cx| cx.notify()));
        }
        let this = Self {
            workspace,
            ssh_prompt,
            focus: cx.focus_handle(),
            nav: ViewNav::restored(saved_view),
            store,
            db_version: None,
            tree: TreeState {
                width: window_state::clamp_tree_width(layout.tree_width, width_now),
                collapsed: layout.tree_collapsed,
                expanded: host_tree::parse_expanded(&layout.tree_expanded),
                selected_group: GroupSel::All,
                selected_host: None,
                cursor_host: None,
                resizing: false,
            },
            tree_query,
            vault_query,
            launcher_query,
            manage_search,
            manage_search_reset: false,
            quick_target,
            conn_query,
            conn_generation: 0,
            proc_query,
            proc_generation: 0,
            dev_fake: Default::default(),
            launcher_cursor: 0,
            agent_settings: None,
            sync_settings: None,
            backup_settings: None,
            migrate_settings: None,
            migrate_hint: migrate_ui::MigrateHint::default(),
            about_settings: None,
            sync_bar: crate::sync::bar_model(),
            settings_scroll: ::gpui::ScrollHandle::new(),
            settings_section: crate::settings_nav::SettingsSection::restored(
                layout.settings_section.as_deref(),
            ),
            prefs,
            app_lock,
            pref_sliders,
            highlight_editor: None,
            panel_scroll: ::gpui::ScrollHandle::new(),
            memory_seen: crate::host_activity::memory_version(),
            ai_dots: Vec::new(),
            status_host: None,
            status: None,
            window_width: width_now,
            window_height: f32::from(window.viewport_size().height),
            tick_count: 0,
            tokenway_seen: 0,
            recovery_scanned: false,
            panels: PanelState::default(),
            aside_width,
            panel_drag: None,
            panel_focus: cx.focus_handle(),
            host_form: None,
            ai_builtin: ai_builtin::AiBuiltinUi::default(),
            x11_ui: x11_ui::X11Ui::new(window, cx),
            keychain,
            proxy_ui,
            forward_ui: ForwardUi::default(),
            bars,
            docks: docks::DockState::default(),
            tab_strip: tab_strip::TabStrip::default(),
            tab_dragger: crate::tab_drag::Dragger::default(),
            host_focus: std::collections::HashMap::new(),
            vault_nav: vault_keys::VaultNav::default(),
            quick_ui,
            pending_compose_text: None,
            modal: None,
            reveal_ask: None,
            info_host: None,
            info_secret: None,
            info_collapse_gen: 0,
            drop_hint: None,
            drag_scroll: drag_scroll::DragScroll::default(),
            mode_hint_shown: false,
            autostarted_tabs: std::collections::HashSet::new(),
            forward_seen: crate::forward_runtime::manager().version(),
            _subs: subs,
        };
        // 滑块松手写库；保活间隔、状态栏间隔这两个全局行为也在这里套上。
        let slider_subs = this.subscribe_pref_sliders(window, cx);
        this.apply_prefs();
        let mut this = this;
        this._subs.extend(slider_subs);
        // 模型资料库（models.dev）的后台更新：启动后延迟一会儿再做，不跟启动抢网络；设置里关了就不做。
        static MODELS_REFRESH: std::sync::Once = std::sync::Once::new();
        MODELS_REFRESH.call_once(|| {
            crate::agent_setup::builtin::spawn_background_refresh(Duration::from_secs(30));
            // Tokenway 的预设清单：每 24 小时拉一次并验签（公钥还是占位时什么都不拉，只用内置清单）。
            crate::agent_setup::tokenway::spawn_background_refresh(Duration::from_secs(45));
        });

        // 后台节拍：重读主机库（别的窗口改了主机这里能看到）、跟着活动标签换状态栏订阅、
        // 取状态栏数据。窗口没了 `update_in` 会报错，任务自己退出。
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    // ---- 节拍 ----

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tick_count += 1;
        self.lock_tick(window, cx);
        let mut changed = false;
        // 主机库：别的连接（其它窗口、同步、备份恢复）改过才重读，不再每 2 秒无条件跑 12 条 SQL；
        // 每 15 秒兜底一次，防止个别环境下 `PRAGMA data_version` 不翻（审查 M-14）。
        let version = self.store.data_version();
        let moved = version != self.db_version;
        let fallback = self.tick_count.is_multiple_of(30);
        if moved || fallback {
            self.db_version = version;
            if self.store.reload() {
                self.drop_stale_state();
                changed = true;
            }
            // 内置助手的接口清单也一起跟：同步下来的、别的窗口改的接口，面板的智能体下拉里要能看到。
            if let Ok(db) = self.store.db() {
                crate::agent_setup::builtin::reload_with(db);
            }
        }
        changed |= self.sync_status(cx);
        changed |= self.tokenway_tick(window, cx);
        self.connections_tick(window, cx);
        self.processes_tick(window, cx);
        changed |= self.sync_sync_bar();
        changed |= self.sync_forwards(cx);
        changed |= self.x11_poll(window, cx);
        self.sync_docks(window, cx);
        // 终端内容有没有摆在窗口里（主机页 / 设置页 / 文件和编辑器标签盖住它时没有）：延迟测量据此暂停。
        let terminal_shown = self.nav.view == MainView::Terminal
            && self.active_sftp_view().is_none()
            && self.active_editor_view().is_none();
        self.workspace
            .update(cx, |ws, _cx| ws.set_surface_shown(terminal_shown));
        self.progress_unsaved_ask(window, cx);
        // 启动时锁定的话，上次没处理完的修改等解锁以后再扫、再提示（V2-U35：解锁前什么都不恢复）。
        if self.tick_count >= 2 && app_lock_ui::startup_scans_allowed() {
            self.scan_recovery_once(window, cx);
            self.scan_zssh_once(window, cx);
        }
        self.record_net_history();
        if changed {
            cx.notify();
        }
    }

    /// 转发状态：运行层的版本号变了就重绘；转发面板开着且有转发在跑时每个节拍刷新计数。
    /// 另外，标签连上一台主机后，把这台主机上勾了「自动开启」的转发开起来（同一个标签只开一次）。
    fn sync_forwards(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let version = crate::forward_runtime::manager().version();
        if version != self.forward_seen {
            self.forward_seen = version;
            changed = true;
        }
        if self.nav.view == MainView::Settings
            && self.settings_section == crate::settings_nav::SettingsSection::Forwards
            && crate::forward_runtime::manager()
                .statuses()
                .iter()
                .any(|status| status.active())
        {
            changed = true;
        }
        let connected: Vec<(u64, i64)> = self
            .workspace
            .read(cx)
            .tab_infos()
            .into_iter()
            .filter(|info| info.status == crate::terminal_workspace::TabStatus::Connected)
            .filter_map(|info| info.host_id.map(|host| (info.id, host)))
            .collect();
        let live: std::collections::HashSet<u64> = connected.iter().map(|(id, _)| *id).collect();
        // 标签断了 / 关了：下次再连上要重新开。
        self.autostarted_tabs.retain(|id| live.contains(id));
        for (tab_id, host_id) in connected {
            if self.autostarted_tabs.insert(tab_id) {
                crate::forward_runtime::auto_start_for_host(host_id, self.store.forwards());
            }
        }
        changed
    }

    /// 库里的东西被别处改了 / 删了（调试窗口、同步、恢复备份）：界面上还指着它们的状态要作废——
    /// 编辑到一半的主机没了、选中的凭据 / 代理没了、对应规则没了的转发还在跑。
    fn drop_stale_state(&mut self) {
        if let Some(form) = &self.host_form
            && let Some(id) = form.host_id
            && self.store.host(id).is_none()
        {
            self.close_host_props_now();
        }
        if self
            .tree
            .selected_host
            .is_some_and(|id| self.store.host(id).is_none())
        {
            self.tree.selected_host = None;
        }
        if self
            .info_host
            .is_some_and(|id| self.store.host(id).is_none())
        {
            self.info_host = None;
        }
        if self
            .keychain
            .selected
            .is_some_and(|id| !self.store.identities().iter().any(|item| item.id == id))
        {
            self.keychain.selected = None;
            self.keychain.creating = false;
        }
        if self
            .proxy_ui
            .selected
            .is_some_and(|id| !self.store.proxies().iter().any(|item| item.id == id))
        {
            self.proxy_ui.selected = None;
            self.proxy_ui.creating = false;
        }
        let existing = self.store.forwards().iter().map(|rule| rule.id).collect();
        crate::forward_runtime::reconcile(&existing);
    }

    /// 状态栏订阅跟着「终端页的活动会话」走；返回要不要重绘。
    fn sync_status(&mut self, cx: &mut Context<Self>) -> bool {
        let wanted = {
            let ws = self.workspace.read(cx);
            // 设置里关掉「显示服务器状态栏」：不订阅采集，状态栏也不画。
            (self.prefs.stats_show
                && self.nav.view == MainView::Terminal
                && ws.active_pane_is_live())
            .then(|| ws.active_host_id())
            .flatten()
            // 服务器状态是在 SSH 连接上跑命令采的：Telnet / 串口标签不订阅，状态栏也不画。
            .filter(|id| self.caps_of(*id).server_status)
        };
        // 调试驱动塞了假状态：不订阅真实采集，概览面板 / 状态栏直接用它。
        #[cfg(debug_assertions)]
        if let Some((host, status)) = self.dev_fake.status.clone() {
            let mut changed = false;
            if self.status_host != Some(host) {
                if let Some(old) = self.status_host.take() {
                    crate::status::unsubscribe_host(old);
                }
                self.status_host = Some(host);
                changed = true;
            }
            if self.status.as_ref() != Some(&status) {
                self.status = Some(status);
                changed = true;
            }
            return changed;
        }
        let mut changed = false;
        if wanted != self.status_host {
            if let Some(old) = self.status_host.take() {
                crate::status::unsubscribe_host(old);
            }
            if let Some(new) = wanted {
                crate::status::subscribe_host(new);
            }
            self.status_host = wanted;
            self.status = None;
            changed = true;
        }
        if let Some(host_id) = self.status_host {
            let latest = crate::status::host_status(host_id);
            if latest != self.status {
                self.status = latest;
                changed = true;
            }
        }
        changed
    }

    // ---- 布局持久化 ----

    pub(super) fn layout_snapshot(&self) -> MainLayout {
        MainLayout {
            view: Some(self.nav.view.as_str().to_owned()),
            settings_section: Some(self.settings_section.id().to_owned()),
            tree_width: self.tree.width,
            tree_collapsed: self.tree.collapsed,
            tree_expanded: self.tree.expanded.iter().cloned().collect(),
        }
    }

    pub(super) fn save_layout(&self) {
        let mut state = WindowState::load();
        state.layout = self.layout_snapshot();
        state.save();
    }

    /// 记下窗口位置和布局（关到托盘时也要记）。
    pub(crate) fn save_state(&self, window: &Window) {
        let mut state = WindowState::load();
        state.main = Some(WindowState::capture(window));
        state.layout = self.layout_snapshot();
        state.save();
    }

    /// 窗口真正关闭（退出）：结束所有会话，释放状态栏订阅，记下窗口位置和布局。
    pub(crate) fn on_close(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, _cx| ws.close_all());
        // 开着的端口转发跟着退出一起停（监听和隧道不留在进程里）。
        self.shutdown_docks(cx);
        self.shutdown_editors(cx);
        crate::forward_runtime::shutdown();
        if let Some(old) = self.status_host.take() {
            crate::status::unsubscribe_host(old);
        }
        self.save_state(window);
    }

    // ---- 页面 ----

    pub(crate) fn show_view(
        &mut self,
        view: MainView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 切到终端 / 主机页 / 设置都不再停在 SFTP 标签上（点 SFTP 标签才进去）。
        self.docks.active_sftp = None;
        self.docks.active_editor = None;
        self.nav = self.nav.show(view);
        self.after_view_change(window, cx);
    }

    pub(crate) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_tab = self.workspace.read(cx).tab_count() > 0;
        self.nav = self.nav.toggle_settings(has_tab);
        self.after_view_change(window, cx);
    }

    /// 有没有标签（终端、文件、编辑器）：关掉设置 / 主机页时回终端页还有东西可看。
    fn has_any_tab(&self, cx: &App) -> bool {
        self.workspace.read(cx).tab_count() > 0
            || !self.docks.sftp_tabs.is_empty()
            || !self.docks.editor_tabs.is_empty()
    }

    /// 标题栏的「主机」图标：进主机页；在主机页上再点一次回到进来之前的地方。
    pub(crate) fn toggle_vault_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.docks.active_sftp = None;
        self.docks.active_editor = None;
        let has_tab = self.has_any_tab(cx);
        self.nav = self.nav.toggle_vault(has_tab);
        self.after_view_change(window, cx);
    }

    /// 打开「管理 + 设置」页并定位到某个分类（主机右键「端口转发」、设置页里的「管理代理」、
    /// 欢迎页 / 主机页的快捷命令图标、生成密钥之后看凭据……）。
    pub(crate) fn open_settings_section(
        &mut self,
        section: crate::settings_nav::SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_view(MainView::Settings, window, cx);
        self.set_settings_section(section, cx);
    }

    /// 托盘菜单和「设置 → 关于」共用的「检查更新」入口：打开更新窗口（读清单、下载、验签、确认后安装）。
    pub(crate) fn check_for_updates(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        crate::update::open_update_window(cx);
    }

    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_tab = self.workspace.read(cx).tab_count() > 0;
        self.nav = self.nav.close_settings(has_tab);
        self.after_view_change(window, cx);
    }

    fn after_view_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav.view == MainView::Settings {
            // 别处（比如配置同步）可能改过这几项，进设置页时重读一遍。
            self.reload_prefs();
        }
        self.save_layout();
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 把键盘焦点交给当前页里该拿的那一个：终端页给终端（空白标签给选主机的输入框），
    /// 其他页给根元素。
    pub(super) fn focus_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav.view == MainView::Terminal {
            let ws = self.workspace.read(cx);
            if ws.tab_count() > 0 {
                if ws.active_tab_is_blank() {
                    let handle = self.launcher_query.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                } else {
                    let handle = ws.focus_handle();
                    window.focus(&handle, cx);
                }
                return;
            }
        }
        window.focus(&self.focus, cx);
    }

    // ---- 标签 ----

    pub(crate) fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.select_tab(index, cx));
        self.show_view(MainView::Terminal, window, cx);
    }

    /// 「+」和 Ctrl+Shift+T：开一个空白标签，等用户选主机。
    pub(crate) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.new_empty_tab(cx));
        self.launcher_cursor = 0;
        self.launcher_query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.show_view(MainView::Terminal, window, cx);
    }

    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let closed = self.workspace.update(cx, |ws, cx| ws.close_tab(index, cx));
        if closed {
            self.after_tabs_closed(window, cx);
        }
    }

    pub(crate) fn close_tabs(
        &mut self,
        index: usize,
        scope: CloseScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.workspace.update(cx, |ws, cx| match scope {
            CloseScope::Others => ws.close_other_tabs(index, cx),
            CloseScope::Left => ws.close_tabs_left(index, cx),
            CloseScope::Right => ws.close_tabs_right(index, cx),
            CloseScope::All => ws.close_all_tabs(cx),
        });
        if count > 0 {
            self.after_tabs_closed(window, cx);
        }
    }

    /// 标签关完了终端页就没东西可看，回欢迎页；否则焦点跟着活动标签。
    fn after_tabs_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.read(cx).tab_count() == 0 {
            // 还有 SFTP / 编辑器标签开着：终端页不是空的，停在其中一个上。
            if !self.docks.sftp_tabs.is_empty() || !self.docks.editor_tabs.is_empty() {
                self.fall_back_after_extra_tab(window, cx);
                return;
            }
            self.nav = self.nav.tabs_emptied();
        }
        self.after_view_change(window, cx);
    }

    /// Ctrl+Shift+W：分屏时关聚焦窗格，否则关标签。
    pub(crate) fn close_focused_pane_or_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.nav.view != MainView::Terminal {
            return;
        }
        let acted = self
            .workspace
            .update(cx, |ws, cx| ws.close_focused_pane_or_tab(cx));
        if acted {
            self.after_tabs_closed(window, cx);
        }
    }

    /// 复制标签（双击标签和右键「复制会话」是同一个动作）。
    pub(crate) fn duplicate_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 串口同一时间只能被一个标签打开：复制出来的标签只会报「被别的程序占用」。
        if self.tab_caps(index, cx).send_break {
            self.notice(t(ShellMsg::SerialDuplicateBlocked), window, cx);
            return;
        }
        let done = self
            .workspace
            .update(cx, |ws, cx| ws.duplicate_tab(index, cx));
        if done {
            self.show_view(MainView::Terminal, window, cx);
        } else {
            self.notice(t(ShellMsg::NothingToDuplicate), window, cx);
        }
    }

    pub(crate) fn reconnect_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self
            .workspace
            .update(cx, |ws, cx| ws.reconnect_tab(index, cx));
        if count > 0 {
            self.show_view(MainView::Terminal, window, cx);
        }
    }

    /// 右键「断开连接」：断开这个标签（分屏的每个窗格）的会话，标签留着显示已断开，
    /// 之后可以「重新连接」。同一条 SSH 连接上别的标签 / SFTP 不受影响（只放掉自己那一份）。
    pub(crate) fn disconnect_tab(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |ws, cx| ws.disconnect_tab(index, cx));
    }

    /// 右键「发送 Break」（串口标签）：给这个标签聚焦的窗格发一个 Break 信号。
    pub(crate) fn send_break_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sent = self.workspace.update(cx, |ws, _| ws.send_break_tab(index));
        if sent {
            self.notice(t(ShellMsg::BreakSent), window, cx);
        } else {
            self.notice(t(ShellMsg::BreakNotConnected), window, cx);
        }
    }

    /// Ctrl+Shift+K：给活动的串口标签发 Break；别的标签不处理（返回 `false`，按键照常交给终端）。
    fn send_break_active(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.nav.view != MainView::Terminal || !self.active_caps(cx).send_break {
            return false;
        }
        let sent = self.workspace.update(cx, |ws, _| ws.send_break_active());
        self.notice(
            if sent {
                t(ShellMsg::BreakSent)
            } else {
                t(ShellMsg::BreakNotConnected)
            },
            window,
            cx,
        );
        true
    }

    /// 右键「复制地址」：复制这个标签连的那台主机的地址。
    pub(crate) fn copy_tab_address(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ws = self.workspace.read(cx);
        let address = ws
            .tab_host_id(index)
            .and_then(|id| self.store.address_of(id))
            .or_else(|| {
                ws.tab_quick_target(index)
                    .and_then(|target| page_logic::quick_target_address(&target))
            });
        match address {
            Some(address) => {
                cx.write_to_clipboard(ClipboardItem::new_string(address.clone()));
                self.notice(
                    &format!("{}{address}", t(ShellMsg::CopiedPrefix)),
                    window,
                    cx,
                );
            }
            None => self.notice(t(ShellMsg::NoAddressToCopy), window, cx),
        }
    }

    /// 右键「复制主机名」：复制这个标签连的那台主机在 ZeeTerm 里的名称（快速连接等没有主机记录的标签没有）。
    pub(crate) fn copy_tab_host_name(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.workspace.read(cx).tab_host_id(index) {
            Some(id) => self.copy_host_field(id, CopyField::Name, window, cx),
            None => self.notice(t(ShellMsg::NoHostRecord), window, cx),
        }
    }

    pub(crate) fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let moved = self.workspace.update(cx, |ws, cx| ws.cycle_tab(step, cx));
        if moved || self.nav.view != MainView::Terminal {
            self.show_view(MainView::Terminal, window, cx);
        }
    }

    // ---- 连接 ----

    /// 打开一台主机（双击、回车）：当前终端页上是个空白标签就在它里面连，否则开新标签。
    /// 开了 X11 而本机没有可用的 X 服务器时先弹「需要 X 显示程序」（`x11_gate`），由用户决定。
    pub(crate) fn open_host(&mut self, host_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.x11_gate(host_id, false, window, cx) {
            return;
        }
        self.open_host_now(host_id, window, cx);
    }

    pub(super) fn open_host_now(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self.store.host(host_id).map(|host| host.name.clone()) else {
            return;
        };
        let reuse_blank =
            self.nav.view == MainView::Terminal && self.workspace.read(cx).active_tab_is_blank();
        if reuse_blank {
            self.workspace
                .update(cx, |ws, cx| ws.connect_active_empty(host_id, title, cx));
        } else {
            self.workspace
                .update(cx, |ws, cx| ws.open_host_tab(host_id, title, cx));
        }
        self.tree.selected_host = Some(host_id);
        self.store.record_recent(host_id);
        self.show_view(MainView::Terminal, window, cx);
    }

    /// 右键「在新标签连接」：不管当前是不是空白标签，都另开一个。
    pub(crate) fn open_host_in_new_tab(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.x11_gate(host_id, true, window, cx) {
            return;
        }
        self.open_host_in_new_tab_now(host_id, window, cx);
    }

    pub(super) fn open_host_in_new_tab_now(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self.store.host(host_id).map(|host| host.name.clone()) else {
            return;
        };
        self.workspace
            .update(cx, |ws, cx| ws.open_host_tab(host_id, title, cx));
        self.tree.selected_host = Some(host_id);
        self.store.record_recent(host_id);
        self.show_view(MainView::Terminal, window, cx);
    }

    pub(crate) fn copy_host_field(
        &mut self,
        host_id: i64,
        what: CopyField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match what {
            CopyField::Name => self.store.host(host_id).map(|host| host.name.clone()),
            CopyField::Address => self.store.address_of(host_id),
        };
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            self.notice(&format!("{}{text}", t(ShellMsg::CopiedPrefix)), window, cx);
        }
    }

    pub(crate) fn notice(&mut self, message: &str, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(message.to_owned(), cx);
    }

    // ---- 主机树 ----

    pub(crate) fn toggle_tree(&mut self, cx: &mut Context<Self>) {
        self.tree.collapsed = !self.tree.collapsed;
        self.save_layout();
        cx.notify();
    }

    pub(super) fn toggle_group(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.tree.expanded.remove(&key) {
            self.tree.expanded.insert(key);
        }
        self.save_layout();
        cx.notify();
    }

    /// 单击分组行：选中这个分组（主机页按它过滤），同时展开 / 收起。
    pub(super) fn click_group(&mut self, sel: GroupSel, cx: &mut Context<Self>) {
        self.tree.selected_group = sel;
        match sel {
            GroupSel::Group(id) => self.toggle_group(host_tree::group_key(Some(id)), cx),
            GroupSel::Ungrouped => self.toggle_group(UNGROUPED_KEY.to_owned(), cx),
            GroupSel::All => cx.notify(),
        }
    }

    pub(super) fn reset_tree_width(&mut self, cx: &mut Context<Self>) {
        self.tree.width = window_state::TREE_DEFAULT_WIDTH;
        self.save_layout();
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.bars_mouse_move(event, cx) {
            return;
        }
        if self.docks_mouse_move(event, cx) {
            return;
        }
        if let Some(start) = self.panel_drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.panel_drag = None;
                self.save_panel_width();
                return;
            }
            let width_now = f32::from(window.viewport_size().width);
            self.aside_width = panel_width::dragged_width(
                self.panel_kind(),
                start,
                f32::from(event.position.x),
                width_now,
                0.0,
            );
            cx.notify();
            return;
        }
        if !self.tree.resizing {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.tree.resizing = false;
            self.save_layout();
            return;
        }
        let width_now = f32::from(window.viewport_size().width);
        self.tree.width = window_state::clamp_tree_width(f32::from(event.position.x), width_now);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.bars_mouse_up(cx);
        self.docks_mouse_up(cx);
        if self.panel_drag.take().is_some() {
            self.save_panel_width();
            cx.notify();
        }
        if self.tree.resizing {
            self.tree.resizing = false;
            self.save_layout();
            cx.notify();
        }
    }

    /// 搜索框回车：连选中的那台，没选就连第一台。
    fn connect_from_tree_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.tree_query.read(cx).value().to_string();
        let target = self.first_match(&query, self.tree.cursor_host);
        if let Some(id) = target {
            self.open_host(id, window, cx);
        }
    }

    pub(super) fn connect_from_vault_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.vault_query.read(cx).value().to_string();
        if let Some(id) = self.first_match(&query, None) {
            self.open_host(id, window, cx);
        }
    }

    fn connect_from_launcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.launcher_query.read(cx).value().to_string();
        let shown = pages::launcher_hosts(self.store.hosts(), &query);
        let at = self.launcher_cursor.min(shown.len().saturating_sub(1));
        if let Some(id) = shown.get(at).copied() {
            self.open_host(id, window, cx);
        }
    }

    /// 搜索结果里要连的那台：键盘选过的（还在结果里）优先，否则第一台。
    fn first_match(&self, query: &str, cursor: Option<i64>) -> Option<i64> {
        let filter = host_tree::tree_filter(
            self.store.groups(),
            self.store.hosts(),
            query,
            self.store.flat(),
        )?;
        let order = host_tree::filtered_host_order(
            self.store.groups(),
            self.store.hosts(),
            &filter,
            self.store.flat(),
        );
        cursor
            .filter(|id| order.contains(id))
            .or_else(|| order.first().copied())
    }

    /// 搜索框有焦点时 ↑ ↓ 在结果里移动。
    fn move_search_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.tree_query.read(cx).value().to_string();
        let Some(filter) = host_tree::tree_filter(
            self.store.groups(),
            self.store.hosts(),
            &query,
            self.store.flat(),
        ) else {
            return;
        };
        let order = host_tree::filtered_host_order(
            self.store.groups(),
            self.store.hosts(),
            &filter,
            self.store.flat(),
        );
        self.tree.cursor_host = host_tree::step_cursor(&order, self.tree.cursor_host, delta);
        cx.notify();
    }

    fn move_launcher_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.launcher_query.read(cx).value().to_string();
        let count = pages::launcher_hosts(self.store.hosts(), &query).len();
        if count == 0 {
            return;
        }
        self.launcher_cursor =
            (self.launcher_cursor as isize + delta).clamp(0, count as isize - 1) as usize;
        cx.notify();
    }

    // ---- 快捷键 ----

    /// 窗口级快捷键（0.2.14：捕获阶段处理，先于终端，也不会发给远端）：
    /// Ctrl+Shift+T 新标签、+W 关窗格/标签、+D/+E 分屏、+B 广播、+A AI、+F 文件（后两个先给提示）；Ctrl+, 设置；
    /// Ctrl+Tab 切标签；设置页里 Esc 关设置。
    fn on_capture_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 锁着时主窗口只画锁屏页，不该走到这里；万一走到也不处理任何窗口级快捷键（V2-U26）。
        // （锁屏页上弹出的“重置锁屏密码”框的 Esc / Enter 由锁屏页自己那层处理，见 `render_lock_cover`。）
        if crate::app_lock::is_locked() {
            return;
        }
        // SSH 提问对话框开着：按键都归它（Esc / Enter 在它自己的卡片上处理），窗口级快捷键先不响应（V2-U29）。
        if self.ssh_prompt.read(cx).is_open() {
            return;
        }
        let keystroke = &event.keystroke;
        let key = keystroke.key.to_ascii_lowercase();
        let mods = keystroke.modifiers;

        // 查看 / 复制密码前的锁屏密码验证框开着：它压在一切上面，Esc 取消、Enter 验证（V2-U35）。
        if self.reveal_ask.is_some() {
            if self.reveal_ask_key(&key, !mods.modified(), window, cx) {
                cx.stop_propagation();
            }
            return;
        }

        // 正在拖标签：Esc 取消（V2-U1，见 `tab_move`）。
        if self.tab_dragger.pending() && key == "escape" {
            self.cancel_tab_drag(cx);
            cx.stop_propagation();
            return;
        }

        // 标签溢出菜单开着：Esc 收起它，别的按键照常走。
        if self.tab_strip.menu_open && key == "escape" && !mods.modified() {
            self.close_tab_menu(cx);
            cx.stop_propagation();
            return;
        }

        // 对话框开着：Esc 取消、Enter 确认（带多行输入框的对话框里是 Ctrl+Enter，普通 Enter 留给输入框
        // 换行），别的按键照常给输入框；其余窗口级快捷键先不响应。
        let ctrl_only = mods.control && !mods.shift && !mods.alt && !mods.platform;
        let memory_focused = self.host_memory_focused(window, cx);
        if let Some(action) = self.modal.as_ref().map(|modal| {
            modal::modal_key(
                &key,
                !mods.modified(),
                ctrl_only,
                modal.multiline_form(memory_focused),
            )
        }) {
            match action {
                modal::ModalKey::Cancel => {
                    self.close_modal(window, cx);
                    cx.stop_propagation();
                }
                modal::ModalKey::Confirm => {
                    self.confirm_modal(window, cx);
                    cx.stop_propagation();
                }
                modal::ModalKey::Pass => {}
            }
            return;
        }

        if mods.control && mods.shift && !mods.alt {
            let handled = match key.as_str() {
                "t" => {
                    self.new_tab(window, cx);
                    true
                }
                "w" => {
                    self.close_focused_pane_or_tab(window, cx);
                    true
                }
                // 0.2.14：Ctrl+Shift+D 左右分屏，Ctrl+Shift+E 上下分屏；拆的是聚焦的窗格，可以接着拆。
                "d" | "e" => {
                    if self.nav.view == MainView::Terminal {
                        let axis = if key == "d" { Axis::X } else { Axis::Y };
                        self.workspace
                            .update(cx, |ws, cx| ws.split_active(axis, cx));
                    }
                    true
                }
                "b" => {
                    if self.nav.view == MainView::Terminal {
                        self.workspace.update(cx, |ws, cx| ws.toggle_broadcast(cx));
                    }
                    true
                }
                "a" => {
                    self.toggle_ai_panel(window, cx);
                    true
                }
                // 串口标签：发送 Break。别的标签上这个组合键不拦，照常交给终端。
                "k" => self.send_break_active(window, cx),
                // Ctrl+Shift+F 是文件（SFTP）面板（同 0.2.14）；先拦下来，不然会被终端当成 Ctrl+F 发给远端。
                "f" => {
                    self.toggle_sftp_dock(window, cx);
                    true
                }
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                return;
            }
        }
        if mods.control && !mods.shift && !mods.alt && key == "," {
            self.toggle_settings(window, cx);
            cx.stop_propagation();
            return;
        }
        if mods.control && !mods.alt && key == "tab" {
            self.cycle_tab(if mods.shift { -1 } else { 1 }, window, cx);
            cx.stop_propagation();
            return;
        }

        if mods.modified() {
            return;
        }
        // 设置与管理页里 Esc 回到原来的页面；输入框有焦点时 Esc 先留给输入框（清空搜索）。
        if key == "escape" && self.nav.view == MainView::Settings {
            self.close_settings(window, cx);
            cx.stop_propagation();
            return;
        }
        // 撰写栏里按 Esc：把键盘还给当前终端。
        if key == "escape" && self.compose_bar_open() && self.compose_has_focus(window, cx) {
            self.focus_content(window, cx);
            cx.stop_propagation();
            return;
        }
        // 面板开着：焦点在面板里（或在主机页，那里没有终端要用 Esc）时 Esc 收起它。
        if key == "escape"
            && self.panels_on_page()
            && self.panels.visible().is_some()
            && (matches!(self.nav.view, MainView::Vault | MainView::Welcome)
                || self.panel_focus.contains_focused(window, cx))
        {
            self.close_panel(window, cx);
            cx.stop_propagation();
        }
    }

    /// 搜索框 / 选主机输入框有焦点时，↑ ↓ 在结果里移动（没有修饰键）。
    fn intercept_arrow(
        &mut self,
        event: &::gpui::KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 锁着时输入框在锁屏页上：别让主窗口的对话框 / 搜索框逻辑吞掉回车和方向键（V2-U26）。
        if crate::app_lock::is_locked() {
            return;
        }
        let keystroke = &event.keystroke;
        // 带多行输入框的对话框（新建 / 编辑快捷命令）：命令框自己绑了 Ctrl+Enter（插一个换行），
        // 动作比 `capture_key_down` 先跑，所以这里先把它拦下来；真正的「保存」由随后的
        // `on_capture_key` 做（拦下来之后按键事件还会走到它）。
        let mods = keystroke.modifiers;
        let ctrl_only = mods.control && !mods.shift && !mods.alt && !mods.platform;
        let memory_focused = self.host_memory_focused(window, cx);
        if self.modal.as_ref().is_some_and(|modal| {
            modal::modal_key(
                &keystroke.key,
                false,
                ctrl_only,
                modal.multiline_form(memory_focused),
            ) == modal::ModalKey::Confirm
        }) {
            cx.stop_propagation();
            return;
        }
        if keystroke.modifiers.modified() {
            return;
        }
        let delta = match keystroke.key.as_str() {
            "down" => 1,
            "up" => -1,
            _ => return,
        };
        if self.tree_query.read(cx).focus_handle(cx).is_focused(window) {
            self.move_search_cursor(delta, cx);
            cx.stop_propagation();
        } else if self
            .launcher_query
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
        {
            self.move_launcher_cursor(delta, cx);
            cx.stop_propagation();
        } else if self.compose_bar_open()
            && self.compose_has_focus(window, cx)
            && self.compose_history_key(delta < 0, cx)
        {
            // 撰写栏：光标在第一行 / 最后一行时 ↑ ↓ 翻发送历史。
            cx.stop_propagation();
        }
    }

    // ---- 调试钩子（只在 debug 构建里从 main.rs 调） ----

    /// 主机树现在画了几行（搜索后的结果也算）：驱动脚本断言搜索过滤用。
    #[cfg(debug_assertions)]
    pub(crate) fn tree_row_count(&self, cx: &App) -> usize {
        let query = self.tree_query.read(cx).value().to_string();
        let filter = host_tree::tree_filter(
            self.store.groups(),
            self.store.hosts(),
            &query,
            self.store.flat(),
        );
        host_tree::visible_rows(
            self.store.groups(),
            self.store.hosts(),
            filter.as_ref(),
            &self.tree.expanded,
            self.store.flat(),
        )
        .len()
    }

    /// 截图前的隐私检查：收集窗口上会出现的文字（标签标题、主机树和主机页的名字、终端屏幕），
    /// 命中任何一个禁用片段就返回它。片段只来自环境变量，不写进代码。
    #[cfg(debug_assertions)]
    pub(crate) fn privacy_hits(&self, forbidden: &[String], cx: &App) -> Vec<String> {
        let ws = self.workspace.read(cx);
        let mut texts: Vec<String> = Vec::new();
        texts.extend(ws.tab_infos().into_iter().map(|info| info.title));
        texts.extend(ws.visible_text());
        // SSH 提问对话框上的主机、指纹、问题（V2-U29）。
        texts.extend(self.ssh_prompt.read(cx).privacy_texts());
        texts.extend(self.store.hosts().iter().map(|host| host.name.clone()));
        texts.extend(self.store.groups().iter().map(|group| group.name.clone()));
        // 主机页的卡片会显示「用户@地址:端口」，只在主机页上才算画面里的文字。
        if self.nav.view == MainView::Vault {
            texts.extend(self.store.hosts().iter().map(protocol_form::host_detail));
        }
        // 设置页上的「从 Z-SSH 迁移」会显示数据文件夹、备份文件路径和结果文字（V2-U19）。
        if self.nav.view == MainView::Settings
            && let Some(migrate) = &self.migrate_settings
        {
            texts.extend(migrate.read(cx).privacy_texts(cx));
        }
        // 调用方给的禁用片段（地址、用户名）+ 内置规则（公网 IP、Last login、用户目录）。
        // 命中只返回类别或序号，不返回原文：原文正是不能出现的东西。
        let mut hits = Vec::new();
        for (number, needle) in forbidden
            .iter()
            .filter(|needle| !needle.is_empty())
            .enumerate()
        {
            if texts
                .iter()
                .any(|text| text.to_lowercase().contains(&needle.to_lowercase()))
            {
                hits.push(format!("{}{number}", t(ShellMsg::ForbiddenFragmentPrefix)));
            }
        }
        for text in &texts {
            for category in privacy::builtin_hits(text) {
                let hit = format!("{}{category}", t(ShellMsg::BuiltinRulePrefix));
                if !hits.contains(&hit) {
                    hits.push(hit);
                }
            }
        }
        hits
    }
}

/// 右键菜单里「复制主机名 / 复制地址」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyField {
    Name,
    Address,
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 锁着：整个窗口只画一页不透明的锁屏页，下面的终端、主机树、AI 面板这一帧根本不进元素树（V2-U26）。
        if crate::app_lock::is_locked() {
            self.window_width = f32::from(window.viewport_size().width);
            self.window_height = f32::from(window.viewport_size().height);
            // 锁屏页盖住了属性面板，但明文不该在锁着的时候还留在内存里。
            self.forget_info_secret(window, cx);
            // 验证小对话框里刚敲的密码和等着做的动作也一并作废。
            self.drop_reveal_ask();
            // 对话框里还显示着的明文（主机属性的密码、AI 接口的 Key）重新遮住，解锁后看到的是遮住的样子。
            self.remask_host_password(window, cx);
            self.remask_ai_dialog_key(window, cx);
            return self.render_lock_cover(window, cx);
        }
        self.after_unlock_refocus(window, cx);
        self.render_unlocked(window, cx).into_any_element()
    }
}

impl MainWindow {
    fn render_unlocked(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        self.window_width = f32::from(window.viewport_size().width);
        self.window_height = f32::from(window.viewport_size().height);
        self.apply_pending_compose_text(window, cx);
        let view = self.nav.view;
        let tree_on_page = view.has_sidebar();
        let tree_visible = !self.tree.collapsed && tree_on_page;
        let panel_host = sidebar::info_target(self.prefs.host_info_panel, self.info_host)
            .filter(|_| tree_visible);
        self.sync_info_secret(panel_host, window, cx);
        let tree_width = window_state::clamp_tree_width(self.tree.width, self.window_width);
        let panel_visible = self.panels_on_page() && self.panels.visible().is_some();
        let panel_width = if panel_visible {
            self.panel_width_now(self.window_width)
        } else {
            0.0
        };
        let page_width = self.window_width
            - if tree_visible { tree_width } else { 0.0 }
            - if tree_on_page && self.tree.collapsed && view == MainView::Vault {
                44.0
            } else {
                0.0
            }
            - panel_width;

        let content = match view {
            MainView::Welcome => self.render_welcome(&tokens, cx).into_any_element(),
            MainView::Vault => self
                .render_vault(&tokens, page_width, window, cx)
                .into_any_element(),
            MainView::Terminal => self.render_terminal(&tokens, window, cx).into_any_element(),
            MainView::Settings => self.render_settings(&tokens, window, cx).into_any_element(),
        };
        let docks = self.render_docks(&tokens, cx);
        let panel = self.render_side_panel(&tokens, window, cx);
        let modal = self.render_modal(&tokens, window, cx);
        let reveal_ask = self.render_reveal_ask(&tokens, cx);
        let tab_menu = self.render_tab_menu(&tokens, cx);
        let me_entity = cx.entity();
        let drag_sensor = crate::tab_drag::sensor(
            &me_entity,
            self.tab_dragger.pending(),
            |this, event, window, cx| this.on_tab_drag_move(event, window, cx),
            |this, event, window, cx| this.on_tab_drag_up(event, window, cx),
        );
        let drag_ghost = crate::tab_drag::ghost_chip(window, cx, &tokens);

        v_flex()
            .id("main-window")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_capture_key))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .bg(tokens.background)
            .text_color(tokens.text)
            .ui_font_fallbacks(cx)
            .child(drag_sensor)
            .child(self.render_title_bar(&tokens, tree_visible.then_some(tree_width), window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when(tree_visible, |d| {
                        d.child(self.render_tree(&tokens, tree_width, window, cx))
                    })
                    .when(self.tree.collapsed && view == MainView::Vault, |d| {
                        d.child(self.render_tree_collapsed(&tokens, cx))
                    })
                    .child(div().flex_1().min_w_0().h_full().child(content))
                    .children(docks)
                    .children(panel),
            )
            .children(self.render_status_bar(&tokens, cx))
            .children(self.render_sync_bar(&tokens, cx))
            .children(modal)
            .children(reveal_ask)
            .children(tab_menu)
            .children(drag_ghost)
            .child(self.ssh_prompt.clone())
    }
}
