//! 调试驱动（`ZSSH_MAIN_SCRIPT` 里的 `act:sync_*`）操作同步设置用的入口。只在 debug 构建里存在
//! （`mod dev` 带 `cfg(debug_assertions)`），release 里没有这条路径。
//!
//! 动作走的都是界面按钮同一批方法；输入框用 `set_value` 填，和用户打字之后 `capture` 读到的是
//! 同一份内容。查询只返回状态文字 / 布尔 / 数字，不返回密码、令牌这类秘密。

use ::gpui::{Context, Window};

use super::{ProviderChoice, StorageConfig, SyncDemo};

impl SyncDemo {
    /// 没有后台任务在跑（测试 / 同步 / 登录 / 改密码）。
    pub(crate) fn dev_idle(&self) -> bool {
        !self.testing && !self.syncing && !self.signing_in && !self.change_pw.busy
    }

    /// 往输入框里填内容。字段名见匹配分支。
    pub(crate) fn dev_set(
        &mut self,
        field: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let input = match field {
            "url" => &self.url,
            "directory" => &self.directory,
            "username" => &self.username,
            "password" => &self.password,
            "s3_endpoint" => &self.s3_endpoint,
            "s3_region" => &self.s3_region,
            "s3_bucket" => &self.s3_bucket,
            "s3_access" => &self.s3_access_key,
            "s3_secret" => &self.s3_secret,
            "s3_prefix" => &self.s3_prefix,
            "gh_api" => &self.github_api_base,
            "gh_token" => &self.github_token,
            "gh_gist" => &self.github_gist_id,
            "od_client" => &self.onedrive_client_id,
            "master" => &self.master_password,
            "master2" => &self.master_password_confirm,
            "interval" => &self.auto_interval,
            "cp_old" => &self.change_pw.old,
            "cp_new" => &self.change_pw.new,
            "cp_confirm" => &self.change_pw.confirm,
            _ => return Err("不认识的字段".to_owned()),
        };
        let value = value.to_owned();
        input.update(cx, |state, cx| state.set_value(value, window, cx));
        cx.notify();
        Ok(())
    }

    /// 点一个按钮 / 开关。
    pub(crate) fn dev_do(
        &mut self,
        action: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match action {
            "provider" => {
                let choice = match arg {
                    "webdav" => ProviderChoice::WebDav,
                    "s3" => ProviderChoice::S3,
                    "github" => ProviderChoice::Github,
                    "onedrive" => ProviderChoice::OneDrive,
                    _ => return Err("存储只能是 webdav / s3 / github / onedrive".to_owned()),
                };
                self.switch_provider(choice, cx);
            }
            "save" => self.save(window, cx),
            "test" => self.start_test(cx),
            "sync" => self.start_sync(window, cx),
            "clear" => self.clear(window, cx),
            "remember" => self.toggle_remember(cx),
            "auto" => self.toggle_auto(window, cx),
            "understood" => self.toggle_understood(cx),
            "advanced" => self.toggle_onedrive_advanced(cx),
            "login" => self.start_onedrive_login(cx),
            "login_cancel" => self.cancel_onedrive_login(cx),
            "logout" => self.onedrive_logout(window, cx),
            "revoke" => self.open_revoke_page(cx),
            "delete_continue" => self.confirm_deletion(cx),
            "delete_cancel" => self.cancel_deletion(cx),
            "delete_cloud" => self.confirm_deletion_use_cloud(cx),
            "reset_baseline" => self.reset_baseline(cx),
            "cp_open" => self.open_change_password(cx),
            "cp_submit" => self.submit_change_password(cx),
            "cp_cancel" => self.close_change_password(window, cx),
            "allow_http" => {
                self.allow_insecure_http = !self.allow_insecure_http;
                cx.notify();
            }
            "gh_allow_http" => {
                self.github_allow_insecure_http = !self.github_allow_insecure_http;
                cx.notify();
            }
            "s3_path_style" => {
                self.drafts.s3.path_style = !self.drafts.s3.path_style;
                cx.notify();
            }
            "refresh_conflicts" => self.refresh_conflicts(cx),
            "conflict_use" | "conflict_dismiss" => {
                let index: usize = arg.parse().map_err(|_| "冲突下标不是数字".to_owned())?;
                let id = self
                    .conflicts
                    .items
                    .get(index)
                    .map(|item| item.id)
                    .ok_or("没有这条冲突")?;
                if action == "conflict_use" {
                    self.click_use_other(id, cx);
                } else {
                    self.dismiss_conflict(id, cx);
                }
            }
            _ => return Err("还没有实现".to_owned()),
        }
        Ok(())
    }

    /// 查一项状态，返回文字（布尔是 `true` / `false`）。
    pub(crate) fn dev_query(&self, key: &str, cx: &gpui::App) -> Result<String, String> {
        let flag = |value: bool| value.to_string();
        Ok(match key {
            "status" => self.status.clone(),
            "error" => self.sync_state.error.clone().unwrap_or_default(),
            "report" => self
                .sync_state
                .report
                .as_ref()
                .map(|report| format!("{:?} {}", report.status, report.summary))
                .unwrap_or_default(),
            "pending" => flag(self.sync_state.needs_confirmation()),
            "ready" => flag(self.ready),
            "remember_saved" => flag(self.remember.saved),
            "remember_on" => flag(self.remember.toggled),
            "baseline_broken" => flag(self.baseline_broken),
            "conflicts" => self.conflicts.items.len().to_string(),
            "conflict_error" => self.conflicts.error.clone().unwrap_or_default(),
            "conflict_armed" => flag(self.conflicts.armed.is_some()),
            "provider" => self.drafts.provider.key().to_owned(),
            "saved" => self
                .saved
                .as_ref()
                .map_or_else(String::new, |config| config.provider().to_owned()),
            "saved_gist" => match &self.saved {
                Some(StorageConfig::Github(config)) => flag(!config.gist_id.is_empty()),
                _ => flag(false),
            },
            "probe" => self
                .probe
                .as_ref()
                .map(super::probe_text)
                .unwrap_or_default(),
            "probe_ok" => flag(
                self.probe
                    .as_ref()
                    .is_some_and(|probe| probe.read.ok && probe.write.ok),
            ),
            "probe_existing" => flag(
                self.probe
                    .as_ref()
                    .is_some_and(|probe| probe.has_existing_data),
            ),
            "concurrency" => self
                .concurrency
                .as_ref()
                .map(|record| format!("{:?}", record.mode))
                .unwrap_or_default(),
            "needs_confirm_password" => flag(self.needs_password_confirm()),
            "auto_enabled" => flag(self.auto.settings.enabled),
            "auto_interval" => self.auto.settings.interval_minutes.to_string(),
            "auto_error" => self.auto.error.clone().unwrap_or_default(),
            "last" => self
                .auto
                .last
                .as_ref()
                .map(|last| format!("{} {}", last.status, last.summary))
                .unwrap_or_default(),
            "cp_open" => flag(self.change_pw.open),
            "cp_error" => self.change_pw.error.clone().unwrap_or_default(),
            "master_len" => self
                .master_password
                .read(cx)
                .value()
                .chars()
                .count()
                .to_string(),
            "understood" => flag(self.understood),
            "advanced" => flag(self.onedrive_advanced),
            "signing_in" => flag(self.signing_in),
            "logout_armed" => flag(self.logout_confirm.is_armed()),
            "clear_armed" => flag(self.clear_confirm.is_armed()),
            "baseline_armed" => flag(self.baseline_reset_armed),
            "master_confirm_len" => self
                .master_password_confirm
                .read(cx)
                .value()
                .chars()
                .count()
                .to_string(),
            _ => return Err("不认识的查询".to_owned()),
        })
    }
}
