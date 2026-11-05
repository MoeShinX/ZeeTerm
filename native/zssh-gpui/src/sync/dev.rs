//! 调试驱动（`ZSSH_MAIN_SCRIPT` 里的 `act:sync_*`）操作同步设置用的入口。只在 debug 构建里存在
//! （`mod dev` 带 `cfg(debug_assertions)`），release 里没有这条路径。
//!
//! 动作走的都是界面按钮同一批方法；输入框用 `set_value` 填，和用户打字之后 `capture` 读到的是
//! 同一份内容。查询只返回状态文字 / 布尔 / 数字，不返回密码、令牌这类秘密。

use ::gpui::{Context, Window};

use super::{ProviderChoice, StorageConfig, SyncDemo, cloud_backup};

impl SyncDemo {
    /// 没有后台任务在跑（测试 / 同步 / 登录 / 改密码）。
    pub(crate) fn dev_idle(&self) -> bool {
        !self.testing
            && !self.syncing
            && !self.signing_in
            && !self.change_pw.busy
            && !self.cloud.is_busy()
            && !self
                .cloud
                .form
                .as_ref()
                .is_some_and(|form| form.is_signing_in())
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
            // 云端备份的添加 / 编辑表单（要先 `target_add` / `target_edit`）
            other if other.starts_with("t_") => {
                let form = self.cloud.form.as_ref().ok_or("云端备份的表单没有打开")?;
                form.dev_input(other)?
            }
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
            // ---- 云端备份（U33） ----
            "target_seed" => self.dev_seed_targets(cx)?,
            "target_add" => self.open_target_form(None, window, cx),
            "target_edit" => {
                let id = self.dev_target_id(arg)?;
                self.open_target_form(Some(id), window, cx);
            }
            "target_provider" => {
                let choice = match arg {
                    "webdav" => ProviderChoice::WebDav,
                    "s3" => ProviderChoice::S3,
                    "github" => ProviderChoice::Github,
                    "onedrive" => ProviderChoice::OneDrive,
                    _ => return Err("存储只能是 webdav / s3 / github / onedrive".to_owned()),
                };
                self.set_target_provider(choice, cx);
            }
            // target_fill:webdav|s3：把桩的地址和固定凭据填进添加表单（端口不出现在脚本里）
            "target_fill" => self.dev_target_fill(arg, window, cx)?,
            "target_save" => self.save_target_form(cx),
            "target_cancel" => self.close_target_form(cx),
            "target_login" => self.start_target_login(cx),
            "target_allow_http" => self.toggle_target_http(false, cx),
            "target_toggle" => {
                let id = self.dev_target_id(arg)?;
                self.toggle_target(&id, cx);
            }
            "target_backup" => {
                let id = self.dev_target_id(arg)?;
                self.start_target_backup(&id, cx);
            }
            "target_restore" => {
                let id = self.dev_target_id(arg)?;
                self.arm_target_restore(&id, cx);
            }
            "target_restore_confirm" => self.confirm_target_restore(cx),
            "target_restore_cancel" => self.cancel_target_restore(cx),
            "target_delete" => {
                let id = self.dev_target_id(arg)?;
                self.delete_target(&id, cx);
            }
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
            "targets" => self.cloud.targets.len().to_string(),
            "target_form" => flag(self.cloud.form.is_some()),
            "target_form_error" => self
                .cloud
                .form
                .as_ref()
                .and_then(|form| form.error.clone())
                .unwrap_or_default(),
            "target_notice" => self
                .cloud
                .notice
                .as_ref()
                .map(|notice| notice.text.clone())
                .unwrap_or_default(),
            "target_notice_ok" => flag(self.cloud.notice.as_ref().is_some_and(|n| n.ok)),
            "target_restore_armed" => flag(self.cloud.restore_armed.is_some()),
            "target_delete_armed" => flag(self.cloud.delete_armed.is_some()),
            "target_enabled_count" => self
                .cloud
                .targets
                .iter()
                .filter(|target| target.enabled)
                .count()
                .to_string(),
            "target_busy" => flag(self.cloud.is_busy()),
            other if other.starts_with("target_last_") => {
                let index: usize = other["target_last_".len()..]
                    .parse()
                    .map_err(|_| "目标下标不是数字".to_owned())?;
                let target = self.cloud.targets.get(index).ok_or("没有这个目标")?;
                cloud_backup::last_text(target).1
            }
            other if other.starts_with("target_desc_") => {
                let index: usize = other["target_desc_".len()..]
                    .parse()
                    .map_err(|_| "目标下标不是数字".to_owned())?;
                self.cloud
                    .targets
                    .get(index)
                    .ok_or("没有这个目标")?
                    .describe()
            }
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

impl SyncDemo {
    /// 把本机桩的地址和固定凭据填进云端备份的添加表单。
    fn dev_target_fill(
        &mut self,
        kind: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        use crate::sync_stub as stub;
        let (choice, fields): (ProviderChoice, Vec<(&str, String)>) = match kind {
            "webdav" => (
                ProviderChoice::WebDav,
                vec![
                    ("t_url", stub::webdav_url()?),
                    ("t_dir", "zssh".to_owned()),
                    ("t_user", stub::DAV_USER.to_owned()),
                    ("t_pass", stub::DAV_PASS.to_owned()),
                ],
            ),
            "s3" => (
                ProviderChoice::S3,
                vec![
                    ("t_s3_endpoint", stub::s3_endpoint()?),
                    ("t_s3_region", "us-east-1".to_owned()),
                    ("t_s3_bucket", stub::S3_BUCKET.to_owned()),
                    ("t_s3_access", stub::S3_ACCESS.to_owned()),
                    ("t_s3_secret", stub::S3_SECRET.to_owned()),
                    ("t_s3_prefix", "z-ssh".to_owned()),
                ],
            ),
            _ => return Err("目标只能是 webdav / s3".to_owned()),
        };
        self.set_target_provider(choice, cx);
        for (field, value) in &fields {
            self.dev_set(field, value, window, cx)?;
        }
        Ok(())
    }

    /// 第 `index` 个备份目标的 id。
    fn dev_target_id(&self, index: &str) -> Result<String, String> {
        let index: usize = index.parse().map_err(|_| "目标下标不是数字".to_owned())?;
        self.cloud
            .targets
            .get(index)
            .map(|target| target.id.clone())
            .ok_or_else(|| "没有这个目标".to_owned())
    }

    /// 截图用：造两个目标（一个上次成功、一个上次失败），地址用 example.com / demo-bucket 这类占位。
    fn dev_seed_targets(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        use zssh_core::sync::backup_targets::{self, TargetResult};
        use zssh_core::sync::s3::S3Config;
        use zssh_core::sync::sealer::DpapiSealer;
        use zssh_core::sync::webdav::WebDavConfig;

        let data_dir = crate::settings::data_dir();
        let db = super::open_db_at(&data_dir)?;
        let now = super::now_secs();
        let dav = backup_targets::add_target(
            &db,
            &DpapiSealer,
            None,
            StorageConfig::Webdav(WebDavConfig {
                url: "https://dav.example.com/remote.php/dav/files/me/".to_owned(),
                directory: "zeeterm".to_owned(),
                username: "demo".to_owned(),
                password: "demo-password".to_owned(),
                allow_insecure_http: false,
            }),
            true,
        )
        .map_err(|err| err.to_string())?;
        let s3 = backup_targets::add_target(
            &db,
            &DpapiSealer,
            None,
            StorageConfig::S3(S3Config {
                endpoint: "https://s3.example.com".to_owned(),
                region: "us-east-1".to_owned(),
                bucket: "demo-bucket".to_owned(),
                access_key: "DEMOACCESSKEY".to_owned(),
                secret_key: "demo-secret".to_owned(),
                prefix: "backups".to_owned(),
                path_style: true,
            }),
            true,
        )
        .map_err(|err| err.to_string())?;
        let record = |id: &str, ok: bool, ago: i64, message: &str| {
            backup_targets::record_result(
                &db,
                &DpapiSealer,
                id,
                TargetResult {
                    at: now - ago,
                    ok,
                    message: message.to_owned(),
                    digest: None,
                },
            )
            .map_err(|err| err.to_string())
        };
        record(&dav.id, true, 25 * 60, "已上传")?;
        record(
            &s3.id,
            false,
            25 * 60,
            "S3 写入失败：Access Key 或 Secret 不对",
        )?;
        self.reload_targets_now();
        cx.notify();
        Ok(())
    }
}
