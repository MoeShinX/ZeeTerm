//! 调试驱动里 `act:backup_*` 操作备份设置用的入口。只在 debug 构建里存在。
//! 动作走的都是界面按钮同一批方法；查询只返回状态文字 / 布尔 / 数字。

use ::gpui::{Context, Window};
use zssh_core::sync::offline::OfflineImportMode;

use super::BackupDemo;

impl BackupDemo {
    /// 没有后台任务在跑。
    pub(crate) fn dev_idle(&self) -> bool {
        self.busy.is_none()
    }

    pub(crate) fn dev_set(
        &mut self,
        field: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let input = match field {
            "path" => &self.package_path,
            "password" => &self.password,
            "password2" => &self.password_confirm,
            _ => return Err("字段只能是 path / password / password2".to_owned()),
        };
        let value = value.to_owned();
        input.update(cx, |state, cx| state.set_value(value, window, cx));
        cx.notify();
        Ok(())
    }

    pub(crate) fn dev_do(
        &mut self,
        action: &str,
        arg: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match action {
            "refresh" => self.start_load(cx),
            "create" => self.start_create(cx),
            "keep" => {
                let keep: usize = arg.parse().map_err(|_| "份数不是数字".to_owned())?;
                self.set_keep(keep, cx);
            }
            "arm" | "restore_arm" => {
                let index: usize = arg.parse().map_err(|_| "下标不是数字".to_owned())?;
                let detail = self.backups.get(index).ok_or("没有这份备份")?;
                let (path, name) = (detail.info.path.clone(), detail.info.name.clone());
                self.arm_restore(path, name, cx);
            }
            "confirm" => self.confirm_restore(cx),
            "cancel" => self.cancel_restore(cx),
            "export" => self.start_export(cx),
            "import" => self.start_import(cx),
            "overwrite" => self.confirm_overwrite(cx),
            // 恢复 / 导入时备份里带着同步账号设置、本机已配置了同步：替换或保留
            "accounts_replace" => self.replace_accounts(cx),
            "accounts_keep" => self.keep_accounts(cx),
            "overwrite_cancel" => self.cancel_overwrite(cx),
            "mode" => {
                let mode = match arg {
                    "merge" => OfflineImportMode::Merge,
                    "replace" => OfflineImportMode::Replace,
                    _ => return Err("导入方式只能是 merge / replace".to_owned()),
                };
                self.set_import_mode(mode, cx);
            }
            _ => return Err("还没有实现".to_owned()),
        }
        Ok(())
    }

    pub(crate) fn dev_query(&self, key: &str, cx: &gpui::App) -> Result<String, String> {
        let flag = |value: bool| value.to_string();
        Ok(match key {
            "status" => self.status.clone(),
            "count" => self.backups.len().to_string(),
            "accounts_pending" => flag(self.pending_accounts.is_some()),
            "keep" => self.keep.to_string(),
            "armed" => flag(self.confirm.armed().is_some()),
            "mode" => match self.import_mode {
                OfflineImportMode::Merge => "merge".to_owned(),
                OfflineImportMode::Replace => "replace".to_owned(),
            },
            "overwrite_armed" => {
                let path = std::path::PathBuf::from(self.package_path.read(cx).value().trim());
                flag(self.overwrite.is_armed_for(&path))
            }
            "replace_armed" => {
                let path = std::path::PathBuf::from(self.package_path.read(cx).value().trim());
                flag(self.replace_confirm.is_armed_for(&path))
            }
            "unreadable" => flag(self.backups.iter().any(|detail| !detail.readable)),
            _ => return Err("不认识的查询".to_owned()),
        })
    }
}
