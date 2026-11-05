//! 调试驱动里 `act:migrate_*` 操作迁移块用的入口。只在 debug 构建里存在。动作走的都是界面按钮同一批方法；
//! 查询只返回状态文字，不返回任何密码。

use ::gpui::{App, Context, Window};
use zssh_core::sync::offline::OfflineImportMode;

use super::{Busy, FailureKind, Found, MigrateDemo, Stage, view};

impl MigrateDemo {
    pub(crate) fn dev_do(
        &mut self,
        action: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match action {
            "detect" => self.start_detect(cx),
            "direct" => self.start_direct(cx),
            // file:<路径>|<密码>：填好两个输入框再点「读取并预览」。
            "file" => {
                let (path, password) = arg
                    .split_once('|')
                    .ok_or("格式：file:<路径>|<密码>".to_owned())?;
                let (path, password) = (path.to_owned(), password.to_owned());
                self.file_path
                    .update(cx, |state, cx| state.set_value(path, window, cx));
                self.file_password
                    .update(cx, |state, cx| state.set_value(password, window, cx));
                self.start_file(cx);
            }
            "folder" => self.use_folder(arg.into(), cx),
            "mode" => {
                let mode = match arg {
                    "merge" => OfflineImportMode::Merge,
                    "replace" => OfflineImportMode::Replace,
                    _ => return Err("方式只能是 merge / replace".to_owned()),
                };
                self.set_mode(mode, cx);
            }
            "import" => self.start_import(cx),
            "cancel" => self.cancel(cx),
            "open_hosts" => self.open_hosts(window, cx),
            _ => return Err(format!("不认识的迁移动作：{action}")),
        }
        Ok(())
    }

    pub(crate) fn dev_query(&self, name: &str, _cx: &App) -> Result<String, String> {
        let state = &self.state;
        let text = |value: &str| value.to_owned();
        Ok(match name {
            "found" => text(match state.found {
                Found::Checking => "checking",
                Found::Yes(_) => "yes",
                Found::No => "no",
            }),
            "busy" => text(match state.busy {
                None => "no",
                Some(Busy::Detecting) => "detecting",
                Some(_) => "yes",
            }),
            "stage" => text(match &state.stage {
                Stage::Idle => "idle",
                Stage::Preview(_) => "preview",
                Stage::Done(_) => "done",
                Stage::Failed(failure) if failure.kind == FailureKind::InUse => "in_use",
                Stage::Failed(_) => "failed",
            }),
            "mode" => text(match state.mode {
                OfflineImportMode::Merge => "merge",
                OfflineImportMode::Replace => "replace",
            }),
            "armed" => state.replace_armed.to_string(),
            // 屏幕上这一块的文字（只做"包含"检查用）
            "text" => {
                let mut parts = vec![view::found_text(
                    &state.found,
                    state.folder_error.as_deref(),
                )];
                match &state.stage {
                    Stage::Failed(failure) => parts.push(failure.message.clone()),
                    Stage::Preview(loaded) => {
                        parts.extend(
                            view::preview_rows(&loaded.preview)
                                .into_iter()
                                .map(|(label, text)| format!("{label} {text}")),
                        );
                        parts.extend(view::preview_warnings(&loaded.preview, &loaded.origin));
                    }
                    Stage::Done(done) => parts.extend(
                        view::done_rows(&done.preview, &done.report)
                            .into_iter()
                            .map(|(label, text)| format!("{label} {text}")),
                    ),
                    Stage::Idle => {}
                }
                parts.join("\n")
            }
            "preview_hosts" => match &state.stage {
                Stage::Preview(loaded) => loaded.preview.hosts.inserted.to_string(),
                _ => "-".to_owned(),
            },
            "preview_conflicts" => match &state.stage {
                Stage::Preview(loaded) => loaded.preview.conflicts.to_string(),
                _ => "-".to_owned(),
            },
            "done_hosts" => match &state.stage {
                Stage::Done(done) => done.report.applied.hosts.inserted.to_string(),
                _ => "-".to_owned(),
            },
            _ => return Err(format!("不认识的迁移查询：{name}")),
        })
    }
}
