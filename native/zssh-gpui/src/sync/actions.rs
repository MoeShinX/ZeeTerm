//! 同步设置里 G11 补上的操作：修改主密码、防误删的第三个选项（用云端版本覆盖本地）、
//! 打开撤销 OneDrive 授权的页面，以及两个小开关（首次设置主密码的勾选、OneDrive「高级」）。
//!
//! 修改主密码走 core 的 `SyncContext::change_master_password`（0.2.14 同一个入口）：用旧密码
//! 解开云端包、新密码重新加密、带着读到的版本写回；云端刚被别处改过会失败，让用户先同步。
//! 它和同步、恢复共用一个独占开关——同一时刻只能有一个在改本地 / 云端。

use ::gpui::{AppContext as _, Context, Entity, Window};
use gpui_kit::component::input::InputState;
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::onedrive::browser::REVOKE_PAGE_URL;
use zssh_core::sync::sealer::SecretSealer;

use super::{
    DpapiSealer, ExclusiveKind, SyncContext, SyncDemo, SyncEvent, SyncInFlight,
    exclusive_busy_message, open_existing_db_at, save_master_password_at, validate_master_password,
};

/// 「修改主密码」表单的状态。
pub(super) struct ChangePw {
    pub open: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub old: Entity<InputState>,
    pub new: Entity<InputState>,
    pub confirm: Entity<InputState>,
}

impl ChangePw {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SyncDemo>) -> Self {
        let make = |window: &mut Window, cx: &mut Context<SyncDemo>, placeholder: &'static str| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(placeholder)
            })
        };
        Self {
            open: false,
            busy: false,
            error: None,
            old: make(window, cx, "旧主密码"),
            new: make(window, cx, "新主密码（至少 8 个字符）"),
            confirm: make(window, cx, "再输入一次新主密码"),
        }
    }
}

/// 检查「修改主密码」表单：旧密码要填，新密码够长且两遍一致。
pub(super) fn check_change_password(old: &str, new: &str, confirm: &str) -> Result<(), String> {
    if old.is_empty() {
        return Err("请输入旧主密码".to_owned());
    }
    validate_master_password(new)?;
    if new != confirm {
        return Err("两次输入的新主密码不一致".to_owned());
    }
    if old == new {
        return Err("新主密码和旧的一样，不用改".to_owned());
    }
    Ok(())
}

/// 在阻塞线程里改云端包的密码。加密实现由调用方给，测试用它绕开 DPAPI。
pub(super) fn change_password_with(
    handle: &tokio::runtime::Handle,
    data_dir: &std::path::Path,
    sealer: &dyn SecretSealer,
    old: &str,
    new: &str,
) -> Result<(), String> {
    validate_master_password(new)?;
    let db = open_existing_db_at(data_dir)?;
    let device_id = db
        .sync_device_id()
        .map_err(|err| format!("读取同步设备 ID 失败：{err}"))?;
    let context = SyncContext {
        db: &db,
        sealer,
        device_id,
        data_dir: data_dir.to_path_buf(),
        known_hosts_path: Some(data_dir.join("known_hosts")),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        onedrive_endpoints: super::onedrive_endpoints(),
    };
    handle
        .block_on(context.change_master_password(old, new))
        .map_err(|err| err.to_string())
}

impl SyncDemo {
    /// 展开「修改主密码」表单。
    pub(super) fn open_change_password(&mut self, cx: &mut Context<Self>) {
        self.change_pw.open = true;
        self.change_pw.error = None;
        cx.notify();
    }

    /// 收起并清空表单（密码不留在输入框里）。
    pub(super) fn close_change_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.change_pw.busy {
            return;
        }
        self.change_pw.open = false;
        self.change_pw.error = None;
        for input in [
            &self.change_pw.old,
            &self.change_pw.new,
            &self.change_pw.confirm,
        ] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }

    /// 「修改并上传」。
    pub(super) fn submit_change_password(&mut self, cx: &mut Context<Self>) {
        if self.change_pw.busy || self.testing || self.syncing || self.signing_in {
            return;
        }
        let old = self.change_pw.old.read(cx).value().to_string();
        let new = self.change_pw.new.read(cx).value().to_string();
        let confirm = self.change_pw.confirm.read(cx).value().to_string();
        if let Err(message) = check_change_password(&old, &new, &confirm) {
            self.change_pw.error = Some(message);
            cx.notify();
            return;
        }
        let guard = match SyncInFlight::acquire(ExclusiveKind::Sync) {
            Ok(guard) => guard,
            Err(holder) => {
                self.change_pw.error =
                    Some(exclusive_busy_message(ExclusiveKind::Sync, holder).to_owned());
                cx.notify();
                return;
            }
        };
        let data_dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.change_pw.busy = true;
                self.change_pw.error = None;
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    let _guard = guard;
                    let event =
                        match change_password_with(&handle, &data_dir, &DpapiSealer, &old, &new) {
                            Ok(()) => SyncEvent::PasswordChanged { new },
                            Err(message) => SyncEvent::PasswordChangeFailed(message),
                        };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.change_pw.error = Some(format!("启动任务失败：{err}")),
        }
        cx.notify();
    }

    /// 改成功：输入框换成新密码；本机记住了主密码的话把记住的那份也换掉（0.2.14 的
    /// `replace_password`），不然自动同步下一拍会拿旧密码去解新包。
    pub(super) fn finish_change_password(
        &mut self,
        new: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.change_pw.busy = false;
        self.master_password
            .update(cx, |state, cx| state.set_value(new, window, cx));
        let mut note =
            "已修改主密码：云端数据已用新密码重新加密，其他电脑之后要输入新的主密码".to_owned();
        if self.remember.saved
            && let Err(message) = save_master_password_at(&crate::settings::data_dir(), new)
        {
            note = format!("已修改主密码，但更新本机记住的主密码失败：{message}");
        }
        self.status = note;
        self.close_change_password(window, cx);
    }

    /// 防误删的第三个选项：放弃合并结果，改用云端版本覆盖本地（0.2.14 的「用云端版本覆盖本地」）。
    pub(super) fn confirm_deletion_use_cloud(&mut self, cx: &mut Context<Self>) {
        if self.testing || self.syncing {
            return;
        }
        let Some(password) = self.sync_state.pending_password().map(str::to_owned) else {
            return;
        };
        self.spawn_sync(password, Some(DeletionDecision::UseCloud), cx);
    }

    pub(super) fn toggle_understood(&mut self, cx: &mut Context<Self>) {
        self.understood = !self.understood;
        cx.notify();
    }

    pub(super) fn toggle_onedrive_advanced(&mut self, cx: &mut Context<Self>) {
        self.onedrive_advanced = !self.onedrive_advanced;
        cx.notify();
    }

    /// 「在浏览器中打开『应用和服务』页面」：撤销 ZeeTerm 在微软账户里的授权。
    /// 网址固定，同样先过 core 的白名单校验。
    pub(super) fn open_revoke_page(&mut self, cx: &mut Context<Self>) {
        let opener = super::onedrive_opener();
        if let Err(message) = opener.open(REVOKE_PAGE_URL) {
            self.status = message;
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::check_change_password;

    #[test]
    fn the_change_form_checks_old_new_and_confirm() {
        assert!(check_change_password("", "newpass-123", "newpass-123").is_err());
        assert!(
            check_change_password("old", "short", "short")
                .unwrap_err()
                .contains("至少")
        );
        assert!(
            check_change_password("oldpass-1", "newpass-123", "newpass-124")
                .unwrap_err()
                .contains("不一致")
        );
        assert!(
            check_change_password("samepass-1", "samepass-1", "samepass-1")
                .unwrap_err()
                .contains("一样")
        );
        assert!(check_change_password("oldpass-1", "newpass-123", "newpass-123").is_ok());
    }
}
