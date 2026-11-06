//! 主机页和主机树上的动作：选中 / 看属性 / 编辑 / 连接 / 复制 / 分组管理 / 排序 / 快速连接 /
//! 顶部的视图开关。界面（`vault_page`、`sidebar`、右键菜单）只管触发，落库和提示都在这里，
//! 调试驱动和单测也能直接调。

use std::time::Duration;

use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::model::QuickTarget;

use super::GroupSel;
use super::MainWindow;
use super::host_tree::group_key;
use super::page_logic::{
    OrderState, can_step_group, can_step_host, group_layer_ids, host_layer_ids,
    looks_like_quick_target, order_state, step_in_layer,
};
use super::reveal_ui::RevealAction;
use crate::i18n::{Msg, t};

/// 本文件（主机页 / 主机树上的动作提示）的界面文案。主机名、分组名、地址来自用户或远端，
/// 拼在文案里原样显示。
#[derive(Clone, Copy, Debug)]
enum VaultActMsg {
    /// 传给 `ssh_only_notice` 的功能名（后面直接接「仅 SSH 主机可用」）。
    SftpFeature,
    PasswordCopied,
    CopyPasswordFailedPrefix,
    InfoCopiedWithPassword,
    InfoCopied,
    CopyInfoFailedPrefix,
    GroupDeletedPrefix,
    GroupDeletedSuffix,
    DeleteFailedPrefix,
    ReorderFailedPrefix,
    ResortedByName,
    ResortFailedPrefix,
}

impl Msg for VaultActMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::SftpFeature => "文件（SFTP）",
            Self::PasswordCopied if cfg!(windows) => "已复制密码，30 秒后自动清除",
            // Linux 的剪贴板没有「别记进历史」的通用标记（GPUI 的剪贴板接口挂不了自定义类型），
            // 剪贴板管理器可能已经把它记下来了，自动清除清不掉历史：直接说明。
            Self::PasswordCopied => "已复制密码，30 秒后自动清除；剪贴板管理器可能会记下这条内容",
            Self::CopyPasswordFailedPrefix => "复制密码失败：",
            Self::InfoCopiedWithPassword if cfg!(windows) => {
                "已复制连接信息（含密码），30 秒后自动清除"
            }
            Self::InfoCopiedWithPassword => {
                "已复制连接信息（含密码），30 秒后自动清除；剪贴板管理器可能会记下这条内容"
            }
            Self::InfoCopied => "已复制连接信息",
            Self::CopyInfoFailedPrefix => "复制连接信息失败：",
            Self::GroupDeletedPrefix => "已删除分组「",
            Self::GroupDeletedSuffix => "」",
            Self::DeleteFailedPrefix => "删除失败：",
            Self::ReorderFailedPrefix => "调整顺序失败：",
            Self::ResortedByName => "已按名字重新排序",
            Self::ResortFailedPrefix => "重新排序失败：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::SftpFeature => "SFTP files — ",
            Self::PasswordCopied if cfg!(windows) => {
                "Password copied; it is cleared automatically in 30 seconds"
            }
            Self::PasswordCopied => {
                "Password copied; it is cleared automatically in 30 seconds. A clipboard manager may keep a copy"
            }
            Self::CopyPasswordFailedPrefix => "Failed to copy the password: ",
            Self::InfoCopiedWithPassword if cfg!(windows) => {
                "Connection info copied (including the password); it is cleared automatically in 30 seconds"
            }
            Self::InfoCopiedWithPassword => {
                "Connection info copied (including the password); it is cleared automatically in 30 seconds. A clipboard manager may keep a copy"
            }
            Self::InfoCopied => "Connection info copied",
            Self::CopyInfoFailedPrefix => "Failed to copy the connection info: ",
            Self::GroupDeletedPrefix => "Deleted group “",
            Self::GroupDeletedSuffix => "”",
            Self::DeleteFailedPrefix => "Failed to delete: ",
            Self::ReorderFailedPrefix => "Failed to reorder: ",
            Self::ResortedByName => "Re-sorted by name",
            Self::ResortFailedPrefix => "Failed to re-sort: ",
        }
    }
}

/// 系统的双击间隔 + 一点余量：单击已经展开着的主机属性要「再点一下收起」，但双击（连接）的
/// 第二下不能让面板来回闪，所以收起要等过了双击间隔才执行。
fn collapse_delay() -> Duration {
    #[cfg(windows)]
    let ms = {
        // SAFETY: 无参数、只读系统设置。
        let ms =
            u64::from(unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() });
        if ms == 0 { 500 } else { ms.min(5000) }
    };
    #[cfg(not(windows))]
    let ms = 500;
    Duration::from_millis(ms + 50)
}

impl MainWindow {
    // ---- 提示 ----

    pub(super) fn notice_success(
        &mut self,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::success(message.to_owned()), cx);
    }

    pub(super) fn notice_error(
        &mut self,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::error(message.to_owned()), cx);
    }

    // ---- 选中 / 属性面板 ----

    /// 树里单击主机：选中，并展开 / 切换树底部的属性面板；再点同一台等过双击间隔后收起。
    pub(crate) fn tree_click_host(
        &mut self,
        host_id: i64,
        click_count: usize,
        cx: &mut Context<Self>,
    ) {
        self.tree.selected_host = Some(host_id);
        self.info_collapse_gen += 1;
        if self.info_host == Some(host_id) {
            // 双击的第二下只取消待执行的收起，自己不安排任何动作。
            if click_count < 2 {
                let token = self.info_collapse_gen;
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(collapse_delay()).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.info_collapse_gen == token && this.info_host == Some(host_id) {
                            this.info_host = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        } else {
            self.info_host = Some(host_id);
        }
        cx.notify();
    }

    /// 主机页右边的卡片 / 列表单击：选中，并让左边树底部的属性面板跟着显示这台（再点不收起）。
    pub(crate) fn page_click_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        self.info_collapse_gen += 1;
        self.tree.selected_host = Some(host_id);
        self.info_host = Some(host_id);
        cx.notify();
    }

    /// 双击连接：先取消待执行的收起，免得面板来回闪。
    pub(crate) fn cancel_info_collapse(&mut self) {
        self.info_collapse_gen += 1;
    }

    pub(crate) fn close_info_panel(&mut self, cx: &mut Context<Self>) {
        self.info_collapse_gen += 1;
        self.info_host = None;
        cx.notify();
    }

    // ---- 编辑 / 新建 ----

    /// 右键「属性…」和卡片 / 行上的铅笔按钮（F2 也是）：弹出主机属性对话框（并选中）。
    pub(crate) fn edit_host(&mut self, host_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.tree.selected_host = Some(host_id);
        self.open_host_props(Some(host_id), None, window, cx);
    }

    /// 新建主机（带上现在选中的分组）。
    pub(crate) fn new_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = match self.tree.selected_group {
            GroupSel::Group(id) => Some(id),
            _ => None,
        };
        self.open_host_props(None, group, window, cx);
    }

    /// 打开这台主机的文件视图：主窗口里的 SFTP 标签（0.2.14 同样是主视图标签）。
    pub(crate) fn open_sftp(&mut self, host_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.caps_of(host_id).sftp {
            self.ssh_only_notice(Some(host_id), t(VaultActMsg::SftpFeature), window, cx);
            return;
        }
        let factory = self
            .workspace
            .read(cx)
            .tab_hosts()
            .iter()
            .position(|(_, host)| *host == Some(host_id))
            .and_then(|index| self.workspace.read(cx).tab_origin(index))
            .and_then(|(_, factory)| factory);
        self.open_files_for(host_id, factory, None, window, cx);
    }

    /// 终端标签右键「打开 SFTP」：复用这个标签已认证的连接。
    pub(crate) fn open_sftp_from_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let origin = self.workspace.read(cx).tab_origin(index);
        if let Some((host_id, factory)) = origin {
            if !self.caps_of(host_id).sftp {
                self.ssh_only_notice(Some(host_id), t(VaultActMsg::SftpFeature), window, cx);
                return;
            }
            self.open_files_for(host_id, factory, Some(index), window, cx);
        }
    }

    // ---- 复制 ----

    /// 右键「复制密码」：密码只在后端取出来直接写进系统剪贴板，30 秒后自动清除。
    pub(crate) fn copy_host_password(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 启用了锁屏的话，取明文之前先验证锁屏密码（V2-U35）。
        if !self.reveal_allowed(RevealAction::CopyHostPassword(host_id), window, cx) {
            return;
        }
        let result = self
            .store
            .db()
            .and_then(|db| {
                db.host_saved_password(host_id)
                    .map_err(|err| crate::error_text::error_text(&err))
            })
            // 经 `chat_copy` 写：和聊天复制同一个可替换的写入口，测试里不碰系统剪贴板。
            .and_then(|password| crate::chat_copy::write_sensitive(password.as_str(), cx));
        match result {
            Ok(()) => self.notice(t(VaultActMsg::PasswordCopied), window, cx),
            Err(err) => self.notice_error(
                &format!("{}{err}", t(VaultActMsg::CopyPasswordFailedPrefix)),
                window,
                cx,
            ),
        }
    }

    /// 右键「复制连接信息」：后端拼好文字写进剪贴板（带密码时 30 秒后自动清除）。
    pub(crate) fn copy_host_info(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = self.store.db().and_then(|db| {
            db.host_connection_info(host_id)
                .map_err(|err| crate::error_text::error_text(&err))
        });
        // 连接信息里带着密码才要验证锁屏密码（V2-U35）；不带密码的主机照旧直接复制。
        // 上面取到的明文还在内存里（`Zeroizing`），没验证通过就原样丢掉，不会显示、不会复制。
        if info.as_ref().is_ok_and(|info| info.has_password())
            && !self.reveal_allowed(RevealAction::CopyHostInfo(host_id), window, cx)
        {
            return;
        }
        let result = info.and_then(|info| {
            let text = zssh_core::store::connection_info::format_connection_info(&info);
            if info.has_password() {
                crate::chat_copy::write_sensitive(text.as_str(), cx)?;
            } else {
                crate::chat_copy::write_plain(text.as_str(), cx)?;
            }
            Ok(info.has_password())
        });
        match result {
            Ok(true) => self.notice(t(VaultActMsg::InfoCopiedWithPassword), window, cx),
            Ok(false) => self.notice(t(VaultActMsg::InfoCopied), window, cx),
            Err(err) => self.notice_error(
                &format!("{}{err}", t(VaultActMsg::CopyInfoFailedPrefix)),
                window,
                cx,
            ),
        }
    }

    // ---- 分组 ----

    pub(crate) fn new_group(
        &mut self,
        parent: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_group_dialog(None, parent, false, window, cx);
    }

    /// 右键「未分组」→ 重命名（建成分组）：建一个分组并把现有未分组主机收进去。
    pub(crate) fn adopt_ungrouped(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_group_dialog(None, None, true, window, cx);
    }

    pub(crate) fn rename_group(
        &mut self,
        group_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self
            .store
            .groups()
            .iter()
            .find(|g| g.id == group_id)
            .cloned()
        {
            self.open_group_dialog(Some(group.clone()), group.parent_id, false, window, cx);
        }
    }

    pub(crate) fn do_delete_group(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .store
            .groups()
            .iter()
            .find(|g| g.id == id)
            .map(|g| g.name.clone())
            .unwrap_or_default();
        match self.store.delete_group(id) {
            Ok(()) => {
                // 选中的正是被删的分组：回到「全部主机」。
                if self.tree.selected_group == GroupSel::Group(id) {
                    self.tree.selected_group = GroupSel::All;
                }
                self.tree.expanded.remove(&group_key(Some(id)));
                self.save_layout();
                self.notice_success(
                    &format!(
                        "{}{name}{}",
                        t(VaultActMsg::GroupDeletedPrefix),
                        t(VaultActMsg::GroupDeletedSuffix)
                    ),
                    window,
                    cx,
                );
            }
            Err(err) => self.notice_error(
                &format!("{}{err}", t(VaultActMsg::DeleteFailedPrefix)),
                window,
                cx,
            ),
        }
        cx.notify();
    }

    // ---- 排序 ----

    /// 右键「上移 / 下移」（主机）：传整层的新顺序。
    pub(crate) fn step_host(
        &mut self,
        host_id: i64,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 用库里最新的所在分组：菜单打开期间主机可能已经被挪走。
        let Some(current) = self.store.host(host_id).cloned() else {
            return;
        };
        let ids = host_layer_ids(self.store.hosts(), current.group_id);
        let Some(next) = step_in_layer(&ids, host_id, delta) else {
            return;
        };
        if let Err(err) = self.store.reorder_hosts(&next) {
            self.notice_error(
                &format!("{}{err}", t(VaultActMsg::ReorderFailedPrefix)),
                window,
                cx,
            );
        }
        cx.notify();
    }

    pub(crate) fn step_group(
        &mut self,
        group_id: i64,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self
            .store
            .groups()
            .iter()
            .find(|g| g.id == group_id)
            .cloned()
        else {
            return;
        };
        let ids = group_layer_ids(self.store.groups(), current.parent_id);
        let Some(next) = step_in_layer(&ids, group_id, delta) else {
            return;
        };
        if let Err(err) = self.store.reorder_groups(&next) {
            self.notice_error(
                &format!("{}{err}", t(VaultActMsg::ReorderFailedPrefix)),
                window,
                cx,
            );
        }
        cx.notify();
    }

    /// 「按名字重新排序」：这个分组下直接的主机和子分组回到按名字；`None` = 只管未分组的主机。
    pub(crate) fn reset_layer_order(
        &mut self,
        group: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.store.reset_order(group) {
            Ok(()) => self.notice_success(t(VaultActMsg::ResortedByName), window, cx),
            Err(err) => self.notice_error(
                &format!("{}{err}", t(VaultActMsg::ResortFailedPrefix)),
                window,
                cx,
            ),
        }
        cx.notify();
    }

    /// 这台主机的上移 / 下移现在能不能点：`(能上移, 能下移)`。
    pub(crate) fn host_step_state(&self, host_id: i64) -> (bool, bool) {
        self.store.host(host_id).map_or((false, false), |host| {
            can_step_host(self.store.hosts(), host)
        })
    }

    pub(crate) fn group_step_state(&self, group_id: i64) -> (bool, bool) {
        self.store
            .groups()
            .iter()
            .find(|g| g.id == group_id)
            .map_or((false, false), |group| {
                can_step_group(self.store.groups(), group)
            })
    }

    /// 主机页右边现在能不能调整顺序。
    pub(crate) fn page_order(&self, cx: &App) -> OrderState {
        order_state(
            self.store.layout().sort_mode,
            self.vault_query.read(cx).value().as_ref(),
        )
    }

    // ---- 顶部开关 ----

    pub(crate) fn set_view_mode(
        &mut self,
        mode: super::page_logic::ViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.store.set_view_mode(mode) {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn toggle_density(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.store.layout().density.toggled();
        if let Err(err) = self.store.set_density(next) {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn cycle_sort_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.store.layout().sort_mode.next();
        if let Err(err) = self.store.set_sort_mode(next) {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    // ---- 快速连接 ----

    /// 页面搜索框回车：写成 `user@host[:port]` 的就直接连（先问密码），不保存到主机库；
    /// 其余当搜索，回车连结果里的第一台（G07a 的行为）。
    pub(crate) fn search_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.vault_query.read(cx).value().trim().to_owned();
        if text.contains('@') && looks_like_quick_target(&text) {
            match zssh_core::model::parse_quick_target(&text) {
                Ok(_) => {
                    self.open_quick_dialog(text, window, cx);
                    return;
                }
                Err(err) => {
                    self.notice_error(&err.to_string(), window, cx);
                    return;
                }
            }
        }
        self.connect_from_vault_search(window, cx);
    }

    /// 快速连接对话框确认：开一个连这个地址的标签（不保存主机）。
    pub(crate) fn quick_connect(
        &mut self,
        target: &str,
        password: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parsed: QuickTarget = match zssh_core::model::parse_quick_target(target) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.notice_error(&err.to_string(), window, cx);
                return;
            }
        };
        let label = target.to_owned();
        // 搜索框里那串地址已经用掉了：清掉，回来时主机页是完整的。
        let query = self.vault_query.clone();
        query.update(cx, |input, cx| input.set_value("", window, cx));
        self.close_host_props_now();
        self.panels.close_side();
        self.workspace
            .update(cx, |ws, cx| ws.open_quick_tab(parsed, password, label, cx));
        self.show_view(super::nav::MainView::Terminal, window, cx);
    }
}

#[cfg(test)]
mod notice_tests {
    use super::VaultActMsg;
    use crate::i18n::Msg as _;

    /// 复制密码的提示：Windows 文案不变；其他系统多一句剪贴板管理器可能会记下（中英成对）。
    #[test]
    fn the_password_copy_notices_warn_about_clipboard_managers_except_on_windows() {
        for msg in [
            VaultActMsg::PasswordCopied,
            VaultActMsg::InfoCopiedWithPassword,
        ] {
            assert!(msg.zh().contains("30 秒后自动清除"), "{}", msg.zh());
            assert!(msg.en().contains("30 seconds"), "{}", msg.en());
            let (zh_warns, en_warns) = (
                msg.zh().contains("剪贴板管理器可能会记下这条内容"),
                msg.en().contains("clipboard manager may keep a copy"),
            );
            assert_eq!(zh_warns, !cfg!(windows), "{}", msg.zh());
            assert_eq!(en_warns, !cfg!(windows), "{}", msg.en());
        }
        if cfg!(windows) {
            assert_eq!(
                VaultActMsg::PasswordCopied.zh(),
                "已复制密码，30 秒后自动清除"
            );
            assert_eq!(
                VaultActMsg::InfoCopiedWithPassword.zh(),
                "已复制连接信息（含密码），30 秒后自动清除"
            );
        }
    }
}
