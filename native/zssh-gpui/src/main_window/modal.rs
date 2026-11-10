//! 主窗口里的对话框：确认、分组新建 / 重命名、快速连接的密码、生成密钥、安装公钥、快捷命令和主机属性的新建 / 编辑。
//!
//! 同一时间一个，盖在整个窗口上面（半透明遮罩 + 居中卡片）。对话框是主窗口自己画的，不走系统
//! 模态框：状态都在 [`Modal`] 里，自动化驱动能直接看、直接点，Esc 取消、Enter 确认。
//! 字段和流程对照 0.2.14 的 `GroupFormDialog.tsx` / `QuickConnectDialog.tsx` / `ConfirmDialog` /
//! `SshKeyDialogs.tsx`。

use ::gpui::{Entity, SharedString};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::{Group, GroupDraft};

use super::MainWindow;
use super::key_dialogs::{GenerateKeyDialog, InstallKeyDialog};
use super::modal_drag;
use super::page_logic::{delete_group_message, flatten_groups, forbidden_parents};
use super::quick_commands::Field as QuickField;
use super::widgets::{
    ButtonLook, Choice, error_note, field, modal_card_dense, modal_scrim, note, select, text_button,
};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 确认之后要做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    DeleteHost(i64),
    DeleteGroup(i64),
    DeleteIdentity(i64),
    /// 删除内置助手的一个接口（连同它的 API Key）。
    DeleteAiEndpoint(i64),
    /// 断开 Tokenway（通知服务端停用令牌、删 Key、删接口配置）。
    DisconnectTokenway(i64),
    DeleteProxy(i64),
    DeleteForward(i64),
    ClearIdentitySecret(i64),
    ClearProxySecret(i64),
    /// 多行命令发送前的确认（快捷命令栏和撰写栏共用）；`compose` 为真时发完要清空撰写栏。
    SendCommand {
        data: String,
        all: bool,
        compose: Option<String>,
    },
    DeleteQuickCommand(String),
    /// 有进行中的工作时真正退出。
    QuitApp,
    CloseSftpTab(u64),
    /// AI 面板的执行模式切到「全部放行」。
    AllowAllCommands,
    /// Linux / macOS：钥匙串里找不到资料库密钥时，清掉所有已保存的密码并重来（设置页「重置已保存的密码」）。
    #[cfg(unix)]
    ResetSavedCredentials,
}

pub struct Confirm {
    pub title: String,
    pub message: String,
    /// `None` = 只能关掉（比如还有主机在用这条凭据，不给删）。
    pub confirm_label: Option<String>,
    pub destructive: bool,
    pub action: ConfirmAction,
}

pub struct GroupDialog {
    pub group: Option<Group>,
    pub parent: Option<i64>,
    /// 「未分组」重命名：建成分组，并把现在所有未分组的主机收进去。
    pub adopt_ungrouped: bool,
    pub ungrouped_count: usize,
    pub name: Entity<InputState>,
    pub error: Option<String>,
}

/// 快速连接对话框和输入校验的文案。
#[derive(Clone, Copy, Debug)]
enum QuickMsg {
    Empty,
    Format,
    Title,
    Hint,
    To,
    NotePrefix,
    NoteSuffix,
    Password,
    Cancel,
    Next,
    Connect,
}

impl crate::i18n::Msg for QuickMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Empty => "请输入 用户名@主机地址",
            Self::Format => "要写成 用户名@主机地址，比如 user@host:22",
            Self::Title => "快速连接",
            Self::Hint => "输入 用户名@主机地址（可以带 :端口），直接连，不会保存到主机库。",
            Self::To => "连接到",
            Self::NotePrefix => "直接连 ",
            Self::NoteSuffix => "，不会保存到主机库。",
            Self::Password => "密码",
            Self::Cancel => "取消",
            Self::Next => "下一步",
            Self::Connect => "连接",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Empty => "Enter user@host",
            Self::Format => "Use user@host, for example user@host:22",
            Self::Title => "Quick Connect",
            Self::Hint => {
                "Enter user@host (optionally with :port). It connects directly and is not saved to the host library."
            }
            Self::To => "Connect to",
            Self::NotePrefix => "Connect directly to ",
            Self::NoteSuffix => " — it is not saved to the host library.",
            Self::Password => "Password",
            Self::Cancel => "Cancel",
            Self::Next => "Next",
            Self::Connect => "Connect",
        }
    }
}

/// 删除确认、分组对话框的文案。
#[derive(Clone, Copy, Debug)]
enum ModalMsg {
    DeleteHostTitlePrefix,
    DeleteHostTitleSuffix,
    DeleteGroupTitlePrefix,
    DeleteGroupTitleSuffix,
    DeleteHostBody,
    DeleteConfirm,
    GroupNameEmpty,
    FlatFailedPrefix,
    Cancel,
    GotIt,
    RenameGroup,
    RenameUngrouped,
    NewGroup,
    TopLevel,
    UngroupedNotePrefix,
    UngroupedNoteMid,
    UngroupedNoteSuffix,
    NameLabel,
    ParentLabel,
    Save,
    /// `i18n::count` 的中文量词（英文走单复数，用不到它）。
    HostCountSuffix,
    /// `i18n::count` 的中文量词（同上）。
    ForwardCountSuffix,
    /// 删除主机时「经它跳转的主机也会改成直连」那一段：`{n 台/ports}` + 本键 + 主机名 + 下一键。
    ViaJumpMid,
    ViaJumpSuffix,
    /// 删除主机时「它的端口转发规则一起删」那一段。
    ForwardPrefix,
    ForwardSuffix,
}

impl crate::i18n::Msg for ModalMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::DeleteHostTitlePrefix => "删除主机「",
            Self::DeleteHostTitleSuffix => "」？",
            Self::DeleteGroupTitlePrefix => "删除分组「",
            Self::DeleteGroupTitleSuffix => "」？",
            Self::DeleteHostBody => "主机的记录会被删掉。",
            Self::DeleteConfirm => "删除",
            Self::GroupNameEmpty => "分组名称不能为空",
            Self::FlatFailedPrefix => "没能记住「平铺」设置：",
            Self::Cancel => "取消",
            Self::GotIt => "知道了",
            Self::RenameGroup => "重命名分组",
            Self::RenameUngrouped => "重命名「未分组」",
            Self::NewGroup => "新建分组",
            Self::TopLevel => "（顶层）",
            Self::UngroupedNotePrefix => {
                "「未分组」不是一个真的分组，只是没分组的主机放在一起。这里会建一个这个名字的分组，把现在 "
            }
            Self::UngroupedNoteMid => " 台未分组的主机都移进去。",
            Self::UngroupedNoteSuffix => "",
            Self::NameLabel => "名称",
            Self::ParentLabel => "上级分组",
            Self::Save => "保存",
            Self::HostCountSuffix => " 台",
            Self::ForwardCountSuffix => " 条",
            Self::ViaJumpMid => "经它跳转的主机（",
            Self::ViaJumpSuffix => "）会改成直连目标地址。",
            Self::ForwardPrefix => "\n它的 ",
            Self::ForwardSuffix => "端口转发规则会一起删除。",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::DeleteHostTitlePrefix => "Delete host \"",
            Self::DeleteHostTitleSuffix => "\"?",
            Self::DeleteGroupTitlePrefix => "Delete group \"",
            Self::DeleteGroupTitleSuffix => "\"?",
            Self::DeleteHostBody => "The host record will be deleted.",
            Self::DeleteConfirm => "Delete",
            Self::GroupNameEmpty => "Group name cannot be empty",
            Self::FlatFailedPrefix => "Could not save the \"flat layout\" setting: ",
            Self::Cancel => "Cancel",
            Self::GotIt => "Got it",
            Self::RenameGroup => "Rename Group",
            Self::RenameUngrouped => "Rename \"Ungrouped\"",
            Self::NewGroup => "New Group",
            Self::TopLevel => "(Top level)",
            Self::UngroupedNotePrefix => {
                "\"Ungrouped\" is not a real group — it is where hosts without a group are listed. Creating a group with this name will move all "
            }
            Self::UngroupedNoteMid => " ungrouped ",
            Self::UngroupedNoteSuffix => " into it.",
            Self::NameLabel => "Name",
            Self::ParentLabel => "Parent group",
            Self::Save => "Save",
            Self::HostCountSuffix => " ",
            Self::ForwardCountSuffix => " ",
            Self::ViaJumpMid => " that use it as a jump host (",
            Self::ViaJumpSuffix => ") will connect to their targets directly.",
            Self::ForwardPrefix => "\nIts ",
            Self::ForwardSuffix => " will be deleted too.",
        }
    }
}

/// 快速连接输入的检查：要写成 `user@host[:port]`，解析规则同主机页搜索框里回车（`parse_quick_target`）。
pub fn quick_target_check(text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(crate::i18n::t(QuickMsg::Empty).to_owned());
    }
    if !text.contains('@') {
        return Err(crate::i18n::t(QuickMsg::Format).to_owned());
    }
    zssh_core::model::parse_quick_target(text)
        .map(|_| ())
        .map_err(|err| crate::error_text::error_text(&err))
}

pub struct QuickDialog {
    pub target: String,
    pub password: Entity<InputState>,
}

/// 新建 / 编辑快捷命令的对话框。输入框（名称、分组、命令）是主窗口 `quick_ui` 里的，这里是其余字段和
/// 保存失败的提示；取消 = 直接关掉，列表和存储一个字都不动。
pub struct QuickEditDialog {
    /// `None` = 新建。
    pub id: Option<String>,
    pub color: Option<String>,
    pub enter: bool,
    pub no_suggest: bool,
    pub error: Option<(QuickField, String)>,
}

/// 欢迎页的「快速连接」：先问 `user@host[:port]`，填对了再走原来的密码对话框。
pub struct QuickTargetDialog {
    pub error: Option<String>,
}

pub enum Modal {
    Confirm(Confirm),
    Group(GroupDialog),
    Quick(QuickDialog),
    QuickTarget(QuickTargetDialog),
    /// 新建 / 编辑快捷命令（所有入口共用）。
    QuickEdit(QuickEditDialog),
    /// 主机「属性」（编辑 / 新建主机，所有入口共用，V2-U7）；表单状态在 `MainWindow::host_form`。
    HostProps(super::host_dialog::HostPropsDialog),
    GenerateKey(GenerateKeyDialog),
    InstallKey(InstallKeyDialog),
    /// 设置 → AI → 内置助手：添加 / 编辑接口（V2-A4）；状态较大，装箱。
    AiEndpoint(Box<super::ai_endpoint_dialog::AiEndpointDialog>),
    /// 设置 → AI → 内置助手 →「添加接口」先选预设（Tokenway 推荐在第一位，V2-A5）。
    AiPresets,
    /// 「用 Tokenway 连接」：浏览器授权的进度、取消、粘贴 Key、注册（V2-A5）。
    Tokenway(Box<super::ai_presets::TokenwayDialog>),
    /// 编辑器有未保存修改：保存 / 不保存 / 取消。
    Unsaved(super::editor_tabs::UnsavedAsk),
    /// 启动时的恢复提示：上次没处理完的修改。
    Recovery(super::recovery_ui::RecoveryUi),
    /// 连开了 X11 的主机而本机没有可用的 X 服务器：一键安装 / 去设置 / 这次不用 X11 继续连接。
    X11Needed(super::x11_ui::X11Prompt),
    /// 锁屏的设置 / 修改 / 关闭 / 重置密码（V2-U26）；装箱免得撑大枚举。
    AppLock(Box<super::app_lock_ui::LockDialog>),
    /// 概览面板「网卡」一节的「所有连接」：只读的大对话框（V2-U12）；状态较大，装箱。
    Connections(Box<super::connections_ui::ConnectionsDialog>),
    /// 概览面板「进程」一节的「进程管理」：进程列表 + 结束进程（带确认，V2-U18）；状态较大，装箱。
    Processes(Box<super::processes_ui::ProcessesDialog>),
}

impl Modal {
    /// 对话框里有没有后台任务在跑（跑的时候不能点遮罩 / Esc 关掉）。
    pub fn busy(&self) -> bool {
        match self {
            Self::GenerateKey(dialog) => dialog.busy(),
            Self::InstallKey(dialog) => dialog.busy(),
            Self::Unsaved(ask) => ask.busy,
            // 点了确认、结束进程的命令还在跑：等结果回来再关。
            Self::Processes(dialog) => dialog.busy(),
            // 密码在后台算哈希：算完之前不能关，免得结果落在已经没有的对话框上。
            Self::AppLock(dialog) => dialog.busy(),
            // 断开连接 / 结束进程同理。
            Self::Connections(dialog) => dialog.busy(),
            _ => false,
        }
    }

    /// 里面有多行输入框：回车是换行，要 Ctrl+Enter 才确认。主机属性只有键盘焦点在「AI 记忆」那个多行框里时才是这样
    /// （`memory_focused`），在别的单行框里 Enter 就是确定。
    pub fn multiline_form(&self, memory_focused: bool) -> bool {
        match self {
            Self::QuickEdit(_) => true,
            // 额外模型是多行框：普通 Enter 不确认（免得在地址框里敲回车就把没填完的表单存了），Ctrl+Enter 才保存。
            Self::AiEndpoint(_) => true,
            Self::HostProps(dialog) => {
                memory_focused && matches!(dialog.overlay, super::host_dialog::Overlay::None)
            }
            _ => false,
        }
    }

    /// 填了半天的表单（有多行框的、主机属性）：卡片占满窗口高度、内部自己滚动。
    pub fn tall(&self) -> bool {
        matches!(
            self,
            Self::QuickEdit(_)
                | Self::HostProps(_)
                | Self::AiEndpoint(_)
                | Self::Connections(_)
                | Self::Processes(_)
        )
    }

    /// 点遮罩算不算取消。填了半天的表单不能被一次误点丢掉，只能用取消按钮或 Esc 关。
    pub fn closes_on_scrim(&self) -> bool {
        // 连接列表点遮罩可以关；确认框开着时不行（同进程管理）。填了半天的表单才不行。
        // 连接 Tokenway 的对话框里有进行中的授权，不能被一次误点取消。
        // 锁屏密码对话框里是刚敲的密码，不能被一次误点遮罩丢掉。
        // 主机属性例外：它有「放弃修改」的确认浮层兜底，点遮罩走取消也丢不了东西
        // （取消按钮被挡住、窗口又小的时候，这是唯一的退路）。
        (!self.tall() && !matches!(self, Self::Tokenway(_) | Self::AppLock(_)))
            || matches!(self, Self::HostProps(_))
            || matches!(self, Self::Connections(dialog) if dialog.ask.is_none())
            // 进程列表点遮罩可以关；确认框开着时不行（免得一次误点把正要确认的结束请求丢掉）。
            || matches!(self, Self::Processes(dialog) if dialog.ask.is_none())
    }
}

/// 对话框开着时一次按键的去向（`on_capture_key` 用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalKey {
    Cancel,
    Confirm,
    /// 不拦，交给输入框。
    Pass,
}

/// `plain`：没按任何修饰键；`ctrl_only`：只按着 Ctrl。普通对话框 Esc 取消、Enter 确认；
/// 带多行输入框的对话框里普通 Enter 是换行（交给输入框），Ctrl+Enter 才确认。
pub fn modal_key(key: &str, plain: bool, ctrl_only: bool, multiline_form: bool) -> ModalKey {
    match key {
        "escape" if plain => ModalKey::Cancel,
        "enter" if plain && !multiline_form => ModalKey::Confirm,
        "enter" if ctrl_only && multiline_form => ModalKey::Confirm,
        _ => ModalKey::Pass,
    }
}

impl MainWindow {
    // ---- 打开 ----

    pub(crate) fn ask_delete_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        let Some(host) = self.store.host(host_id).cloned() else {
            return;
        };
        // 读库里最新的主机和转发规则，列出连带影响（F3 X3）。
        let hosts = self
            .store
            .db()
            .ok()
            .and_then(|db| db.host_list().ok())
            .unwrap_or_else(|| self.store.hosts().to_vec());
        let forwards = self
            .store
            .db()
            .ok()
            .and_then(|db| db.forward_list().ok())
            .unwrap_or_default();
        let impact = crate::hosts::host_delete_impact(&hosts, &forwards, host_id);
        // 它的密码：独占的一起删，共用的保留，密钥不动（V2-U1 第 8 条，规则见 `host_delete`）。
        let identities = self
            .store
            .db()
            .ok()
            .and_then(|db| db.identity_list().ok())
            .unwrap_or_default();
        let credential_lines = super::host_delete::confirm_lines(&super::host_delete::plan(
            host_id,
            &hosts,
            &identities,
        ));
        let mut message = crate::i18n::t(ModalMsg::DeleteHostBody).to_owned();
        for line in credential_lines {
            message.push('\n');
            message.push_str(&line);
        }
        if !impact.via_jump.is_empty() {
            let n = impact.via_jump.len();
            // 名单的连接符跟着语言走（中文用顿号）。
            let names = impact.via_jump.join(crate::i18n::list_sep());
            let count = crate::i18n::count(
                n as u64,
                crate::i18n::t(ModalMsg::HostCountSuffix),
                "host",
                "hosts",
            );
            message.push_str(&format!(
                "\n{count}{}{names}{}",
                crate::i18n::t(ModalMsg::ViaJumpMid),
                crate::i18n::t(ModalMsg::ViaJumpSuffix)
            ));
        }
        if impact.forwards > 0 {
            let count = crate::i18n::count(
                impact.forwards as u64,
                crate::i18n::t(ModalMsg::ForwardCountSuffix),
                "port forwarding rule",
                "port forwarding rules",
            );
            message.push_str(&format!(
                "{}{count}{}",
                crate::i18n::t(ModalMsg::ForwardPrefix),
                crate::i18n::t(ModalMsg::ForwardSuffix)
            ));
        }
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!(
                "{}{}{}",
                crate::i18n::t(ModalMsg::DeleteHostTitlePrefix),
                host.name,
                crate::i18n::t(ModalMsg::DeleteHostTitleSuffix)
            ),
            message,
            confirm_label: Some(crate::i18n::t(ModalMsg::DeleteConfirm).to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteHost(host_id),
        }));
        cx.notify();
    }

    pub(crate) fn ask_delete_group(&mut self, group_id: i64, cx: &mut Context<Self>) {
        let Some(group) = self
            .store
            .groups()
            .iter()
            .find(|g| g.id == group_id)
            .cloned()
        else {
            return;
        };
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!(
                "{}{}{}",
                crate::i18n::t(ModalMsg::DeleteGroupTitlePrefix),
                group.name,
                crate::i18n::t(ModalMsg::DeleteGroupTitleSuffix)
            ),
            message: delete_group_message(self.store.flat()).to_owned(),
            confirm_label: Some(crate::i18n::t(ModalMsg::DeleteConfirm).to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteGroup(group_id),
        }));
        cx.notify();
    }

    /// 新建（`group` 为空）或重命名分组；`parent` 是新建时默认挂在哪个分组下。
    pub(crate) fn open_group_dialog(
        &mut self,
        group: Option<Group>,
        parent: Option<i64>,
        adopt_ungrouped: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = group.as_ref().map(|g| g.name.clone()).unwrap_or_default();
        let name = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let focus = name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let ungrouped_count = self
            .store
            .hosts()
            .iter()
            .filter(|host| host.group_id.is_none())
            .count();
        let parent = group.as_ref().map_or(parent, |g| g.parent_id);
        self.modal = Some(Modal::Group(GroupDialog {
            group,
            parent,
            adopt_ungrouped,
            ungrouped_count,
            name,
            error: None,
        }));
        cx.notify();
    }

    /// 欢迎页的「快速连接」：弹出输入 `user@host[:port]` 的小对话框（输入框是主窗口的 `quick_target`）。
    pub(crate) fn open_quick_target_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.quick_target.clone();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.modal = Some(Modal::QuickTarget(QuickTargetDialog { error: None }));
        cx.notify();
    }

    pub(crate) fn open_quick_dialog(
        &mut self,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let focus = password.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.modal = Some(Modal::Quick(QuickDialog { target, password }));
        cx.notify();
    }

    // ---- 关闭 / 确认 ----

    pub(crate) fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.as_ref().is_some_and(Modal::busy) {
            return;
        }
        // Tokenway 连接对话框：进行中的授权一并取消（监听关闭）。
        if matches!(self.modal, Some(Modal::Tokenway(_))) {
            self.tokenway_close(window, cx);
            return;
        }
        // 进程管理：确认框开着时，Esc / 关闭先收确认框（不结束任何进程），再按一次才关对话框。
        if let Some(Modal::Processes(dialog)) = &mut self.modal
            && dialog.ask.is_some()
        {
            dialog.ask = None;
            cx.notify();
            return;
        }
        // 所有网络连接：确认框开着时同理（断开连接、结束进程都只在点了确认框里的按钮后才执行）。
        if let Some(Modal::Connections(dialog)) = &mut self.modal
            && dialog.ask.is_some()
        {
            dialog.ask = None;
            cx.notify();
            return;
        }
        // 主机属性：先收小浮层；有没保存的修改先问是否放弃。
        if matches!(self.modal, Some(Modal::HostProps(_))) {
            self.host_props_cancel(window, cx);
            return;
        }
        self.modal = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    /// Enter / 主按钮。
    pub(crate) fn confirm_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.modal {
            Some(Modal::Confirm(_)) => self.run_confirm(window, cx),
            Some(Modal::Group(_)) => self.submit_group_dialog(window, cx),
            Some(Modal::Quick(_)) => self.submit_quick_dialog(window, cx),
            Some(Modal::QuickTarget(_)) => self.submit_quick_target(window, cx),
            Some(Modal::QuickEdit(_)) => self.quick_save(window, cx),
            Some(Modal::HostProps(_)) => self.host_props_confirm(window, cx),
            Some(Modal::AiEndpoint(_)) => self.ai_endpoint_save(window, cx),
            Some(Modal::AppLock(_)) => self.submit_lock_dialog(window, cx),
            // 预设列表每一行自己有按钮，回车不替用户选。
            Some(Modal::AiPresets) => {}
            Some(Modal::Tokenway(_)) => self.tokenway_confirm(window, cx),
            Some(Modal::GenerateKey(_)) => self.submit_generate_key(window, cx),
            Some(Modal::InstallKey(_)) => self.submit_install_key(window, cx),
            Some(Modal::Unsaved(_)) => self.answer_unsaved(true, window, cx),
            // 恢复提示每一项有自己的按钮，回车不替用户选。
            Some(Modal::Recovery(_)) => {}
            // 三个选项各有按钮：回车不替用户选（尤其不能一回车就开始安装）。
            Some(Modal::X11Needed(_)) => {}
            // Enter 留给搜索框；断开连接 / 结束进程只能点确认框里的按钮，Enter 不替用户确认。
            Some(Modal::Connections(_)) => {}
            // 结束进程只能点确认框里的按钮：Enter 不替用户确认（尤其不能一回车就结束进程）。
            Some(Modal::Processes(_)) => {}
            None => {}
        }
    }

    fn run_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Confirm(confirm)) = self.modal.take() else {
            return;
        };
        if confirm.confirm_label.is_none() {
            self.focus_content(window, cx);
            cx.notify();
            return;
        }
        match confirm.action {
            ConfirmAction::DeleteHost(id) => self.do_delete_host(id, window, cx),
            ConfirmAction::DeleteGroup(id) => self.do_delete_group(id, window, cx),
            ConfirmAction::DeleteIdentity(id) => self.do_delete_identity(id, window, cx),
            ConfirmAction::DeleteAiEndpoint(id) => self.do_delete_ai_endpoint(id, window, cx),
            ConfirmAction::DisconnectTokenway(id) => self.do_disconnect_tokenway(id, window, cx),
            ConfirmAction::DeleteProxy(id) => self.do_delete_proxy(id, window, cx),
            ConfirmAction::DeleteForward(id) => self.do_delete_forward(id, window, cx),
            ConfirmAction::ClearIdentitySecret(id) => self.do_clear_identity_secret(id, window, cx),
            ConfirmAction::ClearProxySecret(id) => self.do_clear_proxy_secret(id, window, cx),
            ConfirmAction::SendCommand { data, all, compose } => {
                self.do_send(&data, all, compose.as_deref(), window, cx);
            }
            ConfirmAction::DeleteQuickCommand(id) => self.do_delete_quick_command(&id, window, cx),
            ConfirmAction::QuitApp => {
                self.do_quit(window, cx);
                return;
            }
            ConfirmAction::CloseSftpTab(id) => self.do_close_sftp_tab(id, window, cx),
            ConfirmAction::AllowAllCommands => self.do_allow_all_commands(cx),
            #[cfg(unix)]
            ConfirmAction::ResetSavedCredentials => self.do_reset_saved_credentials(window, cx),
        }
        self.focus_content(window, cx);
        cx.notify();
    }

    fn submit_group_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Group(dialog)) = &self.modal else {
            return;
        };
        let name = dialog.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.set_group_error(crate::i18n::t(ModalMsg::GroupNameEmpty), cx);
            return;
        }
        let (existing, parent, adopt) =
            (dialog.group.clone(), dialog.parent, dialog.adopt_ungrouped);
        let draft = GroupDraft {
            id: existing.as_ref().map(|g| g.id),
            name: name.clone(),
            parent_id: parent,
            sort: existing.as_ref().map_or(0, |g| g.sort),
        };
        let saved = match self.store.save_group(&draft) {
            Ok(saved) => saved,
            Err(err) => {
                self.set_group_error(&err, cx);
                return;
            }
        };
        if adopt {
            let loose: Vec<i64> = self
                .store
                .hosts()
                .iter()
                .filter(|host| host.group_id.is_none())
                .map(|host| host.id)
                .collect();
            if let Err(err) = self.store.move_hosts(&loose, Some(saved.id)) {
                self.set_group_error(&err, cx);
                return;
            }
            // 「未分组」改名成功：树里不再有「未分组」这一行，之后不分组的主机直接排在树的根层。
            // 分组已经建好了，写开关失败只提示，不影响关窗口。
            if let Err(err) = self.store.set_flat(true) {
                self.notice_error(
                    &format!("{}{err}", crate::i18n::t(ModalMsg::FlatFailedPrefix)),
                    window,
                    cx,
                );
            }
        }
        self.modal = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    fn set_group_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(Modal::Group(dialog)) = &mut self.modal {
            dialog.error = Some(message.to_owned());
        }
        cx.notify();
    }

    /// 欢迎页快速连接第一步：检查写法（同主机页搜索框里回车的解析），对了才进密码对话框，
    /// 不对就把原因留在对话框里。
    fn submit_quick_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.quick_target.read(cx).value().trim().to_owned();
        match quick_target_check(&text) {
            Ok(()) => {
                self.modal = None;
                self.open_quick_dialog(text, window, cx);
            }
            Err(message) => {
                if let Some(Modal::QuickTarget(dialog)) = &mut self.modal {
                    dialog.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    fn submit_quick_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Quick(dialog)) = self.modal.take() else {
            return;
        };
        let password = dialog.password.read(cx).value().to_string();
        self.quick_connect(&dialog.target, password, window, cx);
    }

    // ---- 画 ----

    pub(super) fn render_modal(
        &mut self,
        tokens: &Tokens,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // 拖动偏移只属于当前这一个对话框：换了一种或关掉了，先清掉再画。
        let kind = self.modal.as_ref().map(std::mem::discriminant);
        self.modal_drag.sync(kind);
        let modal = self.modal.as_ref()?;
        let entity = cx.entity();
        let (width, content): (f32, AnyElement) = match modal {
            Modal::Confirm(confirm) => (420.0, self.confirm_body(confirm, tokens, cx)),
            Modal::Group(dialog) => (400.0, self.group_body(dialog, tokens, cx)),
            Modal::Quick(dialog) => (440.0, self.quick_body(dialog, tokens, cx)),
            Modal::QuickTarget(dialog) => (440.0, self.quick_target_body(dialog, tokens, cx)),
            Modal::QuickEdit(dialog) => (560.0, self.quick_edit_body(dialog, tokens, window, cx)),
            Modal::HostProps(dialog) => (
                super::host_dialog::DIALOG_WIDTH,
                self.host_props_body(dialog, tokens, cx),
            ),
            Modal::AiEndpoint(dialog) => (
                super::ai_endpoint_dialog::DIALOG_WIDTH,
                self.ai_endpoint_body(dialog, tokens, cx),
            ),
            Modal::AiPresets => (520.0, self.ai_presets_body(tokens, cx)),
            Modal::AppLock(dialog) => (440.0, self.lock_dialog_body(dialog, tokens, cx)),
            Modal::Tokenway(dialog) => (540.0, self.tokenway_body(dialog, tokens, cx)),
            Modal::GenerateKey(dialog) => (480.0, self.generate_key_body(dialog, tokens, cx)),
            Modal::InstallKey(dialog) => (480.0, self.install_key_body(dialog, tokens, cx)),
            Modal::Unsaved(ask) => (440.0, self.unsaved_body(ask, tokens, cx)),
            Modal::Recovery(ui) => (600.0, self.recovery_body(ui, tokens, cx)),
            Modal::X11Needed(prompt) => (460.0, self.x11_prompt_body(prompt, tokens, cx)),
            Modal::Connections(dialog) => (
                super::connections_ui::DIALOG_WIDTH,
                self.connections_body(dialog, tokens, cx),
            ),
            Modal::Processes(dialog) => (
                super::processes_ui::DIALOG_WIDTH,
                self.processes_body(dialog, tokens, cx),
            ),
        };
        let closes_on_scrim = modal.closes_on_scrim();
        let tall = modal.tall();
        let overlay = match modal {
            Modal::HostProps(dialog) => self.host_props_overlay(dialog, tokens, cx),
            _ => None,
        };
        // 点遮罩关闭是按下触发的：拖动从卡片上的把手开始（卡片会挡住按下事件，不会穿到遮罩），
        // 松开时不管指针在哪都只是结束拖动，不会被当成点了遮罩。
        let scrim = {
            let (close_entity, move_entity, up_entity) =
                (entity.clone(), entity.clone(), entity.clone());
            modal_scrim("modal-scrim", tall, move |window, cx| {
                if closes_on_scrim {
                    close_entity.update(cx, |this, cx| this.close_modal(window, cx));
                }
            })
            .on_mouse_move(move |event, window, cx| {
                let mouse = (f32::from(event.position.x), f32::from(event.position.y));
                let size = window.viewport_size();
                let viewport = (f32::from(size.width), f32::from(size.height));
                let pressed = event.pressed_button == Some(::gpui::MouseButton::Left);
                move_entity.update(cx, |this, cx| {
                    if this.modal_drag.motion(mouse, pressed, viewport) {
                        cx.notify();
                    }
                });
            })
            .on_mouse_up(::gpui::MouseButton::Left, move |_, _, cx| {
                up_entity.update(cx, |this, cx| {
                    if this.modal_drag.release() {
                        cx.notify();
                    }
                });
            })
        };
        let size = window.viewport_size();
        let (dx, dy) = self
            .modal_drag
            .offset_for_render((f32::from(size.width), f32::from(size.height)));
        let dragging = self.modal_drag.dragging();
        let handle = {
            let entity = entity.clone();
            modal_drag::handle(dragging, move |event, _, cx| {
                let mouse = (f32::from(event.position.x), f32::from(event.position.y));
                entity.update(cx, |this, _| this.modal_drag.press(mouse));
            })
        };
        // 主机属性对话框字段多，用紧凑版卡片（内边距 16、区块间距 12）；别的对话框不变。
        let compact = matches!(modal, Modal::HostProps(_));
        let card = modal_card_dense("modal-card", width, tall, compact, tokens)
            .relative()
            .left(px(dx))
            .top(px(dy))
            // 把手和测量画布先画（在内容下面），标题行里的按钮画在它们上面，点击不受影响。
            .child(handle)
            .child(modal_drag::measure(self.modal_drag.card_size_cell()))
            .child(content);
        Some(scrim.child(card).children(overlay).into_any_element())
    }

    fn confirm_body(
        &self,
        confirm: &Confirm,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity_cancel = cx.entity();
        let entity_ok = entity_cancel.clone();
        let look = if confirm.destructive {
            ButtonLook::Danger
        } else {
            ButtonLook::Primary
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(SharedString::from(confirm.title.clone())),
                    )
                    .child(
                        div()
                            .typo(Role::Body, tokens)
                            .child(SharedString::from(confirm.message.clone())),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(confirm.confirm_label.is_some(), |d| {
                        d.child(text_button(
                            "modal-cancel",
                            crate::i18n::t(ModalMsg::Cancel),
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                entity_cancel.update(cx, |this, cx| this.close_modal(window, cx));
                            },
                        ))
                    })
                    .child(text_button(
                        "modal-ok",
                        confirm
                            .confirm_label
                            .clone()
                            .unwrap_or_else(|| crate::i18n::t(ModalMsg::GotIt).to_owned()),
                        look,
                        move |_, window, cx| {
                            entity_ok.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn group_body(
        &self,
        dialog: &GroupDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let title = if dialog.group.is_some() {
            crate::i18n::t(ModalMsg::RenameGroup)
        } else if dialog.adopt_ungrouped {
            crate::i18n::t(ModalMsg::RenameUngrouped)
        } else {
            crate::i18n::t(ModalMsg::NewGroup)
        };
        let groups = self.store.groups();
        let forbidden = dialog
            .group
            .as_ref()
            .map(|g| forbidden_parents(groups, g.id))
            .unwrap_or_default();
        let ordered = flatten_groups(groups);
        let mut choices = vec![Choice::new(
            crate::i18n::t(ModalMsg::TopLevel),
            dialog.parent.is_none(),
        )];
        let mut targets: Vec<Option<i64>> = vec![None];
        for (group, depth) in &ordered {
            if forbidden.contains(&group.id) {
                continue;
            }
            choices.push(
                Choice::new(group.name.clone(), dialog.parent == Some(group.id)).depth(*depth),
            );
            targets.push(Some(group.id));
        }
        let current = match dialog.parent {
            None => crate::i18n::t(ModalMsg::TopLevel).to_owned(),
            Some(id) => groups.iter().find(|g| g.id == id).map_or_else(
                || crate::i18n::t(ModalMsg::TopLevel).to_owned(),
                |g| g.name.clone(),
            ),
        };
        let pick = {
            let entity = entity.clone();
            move |index: usize, _window: &mut Window, cx: &mut App| {
                let Some(target) = targets.get(index).copied() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    if let Some(Modal::Group(dialog)) = &mut this.modal {
                        dialog.parent = target;
                    }
                    cx.notify();
                });
            }
        };
        let cancel = entity.clone();
        let save = entity.clone();
        v_flex()
            .gap_4()
            .child(div().typo(Role::GroupTitle, tokens).child(title))
            .when(dialog.adopt_ungrouped, |d| {
                let n = dialog.ungrouped_count;
                let text = if crate::i18n::is_en() {
                    format!(
                        "{}{n}{}{}{}",
                        crate::i18n::t(ModalMsg::UngroupedNotePrefix),
                        crate::i18n::t(ModalMsg::UngroupedNoteMid),
                        crate::i18n::plural(n as u64, "host", "hosts"),
                        crate::i18n::t(ModalMsg::UngroupedNoteSuffix)
                    )
                } else {
                    format!(
                        "{}{n}{}",
                        crate::i18n::t(ModalMsg::UngroupedNotePrefix),
                        crate::i18n::t(ModalMsg::UngroupedNoteMid)
                    )
                };
                d.child(note(text, tokens))
            })
            .child(field(
                crate::i18n::t(ModalMsg::NameLabel),
                None,
                Input::new(&dialog.name),
                tokens,
            ))
            .child(field(
                crate::i18n::t(ModalMsg::ParentLabel),
                None,
                select("group-parent", current, choices, pick),
                tokens,
            ))
            .children(
                dialog
                    .error
                    .clone()
                    .map(|error| error_note(error, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        crate::i18n::t(ModalMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        crate::i18n::t(ModalMsg::Save),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            save.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn quick_target_body(
        &self,
        dialog: &QuickTargetDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cancel = cx.entity();
        let next = cancel.clone();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(crate::i18n::t(QuickMsg::Title)),
                    )
                    .child(note(crate::i18n::t(QuickMsg::Hint), tokens)),
            )
            .child(field(
                crate::i18n::t(QuickMsg::To),
                None,
                Input::new(&self.quick_target),
                tokens,
            ))
            .children(
                dialog
                    .error
                    .clone()
                    .map(|error| error_note(error, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        crate::i18n::t(QuickMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        crate::i18n::t(QuickMsg::Next),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            next.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn quick_body(
        &self,
        dialog: &QuickDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cancel = cx.entity();
        let connect = cancel.clone();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(crate::i18n::t(QuickMsg::Title)),
                    )
                    .child(note(
                        format!(
                            "{}{}{}",
                            crate::i18n::t(QuickMsg::NotePrefix),
                            dialog.target,
                            crate::i18n::t(QuickMsg::NoteSuffix)
                        ),
                        tokens,
                    )),
            )
            .child(field(
                crate::i18n::t(QuickMsg::Password),
                None,
                Input::new(&dialog.password),
                tokens,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        crate::i18n::t(QuickMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        crate::i18n::t(QuickMsg::Connect),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            connect.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{ModalKey, modal_key, quick_target_check};

    #[test]
    fn the_welcome_quick_connect_accepts_what_the_search_box_accepts() {
        assert!(quick_target_check("ops@192.0.2.10").is_ok());
        assert!(quick_target_check("  ops@192.0.2.10:2222 ").is_ok());
        assert!(quick_target_check("ops@[2001:db8::1]:22").is_ok());
    }

    #[test]
    fn the_welcome_quick_connect_explains_what_is_wrong() {
        assert!(quick_target_check("").is_err());
        let missing_user = quick_target_check("192.0.2.10").unwrap_err();
        assert!(missing_user.contains("用户名@主机地址"));
        assert!(quick_target_check("@192.0.2.10").is_err(), "没有用户名");
        assert!(quick_target_check("ops@").is_err(), "没有地址");
        assert!(quick_target_check("ops@host:99999").is_err(), "端口越界");
        assert!(quick_target_check("ops @host").is_err(), "有空白");
    }

    #[test]
    fn plain_dialogs_confirm_on_enter_and_cancel_on_escape() {
        assert_eq!(modal_key("enter", true, false, false), ModalKey::Confirm);
        assert_eq!(modal_key("escape", true, false, false), ModalKey::Cancel);
        assert_eq!(modal_key("a", true, false, false), ModalKey::Pass);
        assert_eq!(
            modal_key("enter", false, true, false),
            ModalKey::Pass,
            "带修饰键的 Enter 在普通对话框里不确认（和以前一致）"
        );
    }

    #[test]
    fn the_command_dialog_saves_on_ctrl_enter_and_leaves_plain_enter_to_the_text_box() {
        assert_eq!(modal_key("enter", false, true, true), ModalKey::Confirm);
        assert_eq!(
            modal_key("enter", true, false, true),
            ModalKey::Pass,
            "普通回车是换行，不能保存"
        );
        assert_eq!(
            modal_key("enter", false, false, true),
            ModalKey::Pass,
            "Shift+Enter / Alt+Enter 也只是换行"
        );
        assert_eq!(modal_key("escape", true, false, true), ModalKey::Cancel);
        assert_eq!(modal_key("escape", false, true, true), ModalKey::Pass);
    }
}
