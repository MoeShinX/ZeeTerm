//! 异常退出后的恢复：启动时把上次没处理完的编辑草稿列出来，每一项可以 打开并恢复 / 重新上传 / 另存到本地 / 放弃
//! （0.2.14 的 `EditRecoveryDialog` 语义）。
//!
//! 数据来源是编辑草稿（`<数据目录>/edit-drafts/`）：编辑器里没保存的内容，每 2 秒落一次，程序被结束 / 崩溃时就
//! 留在那儿；用户的真实文件一个都不碰。
//!
//! 「重新上传」先核对：远端文件在这期间被改过（或删了）就不传、报冲突，用户看过之后可以选「覆盖服务器上的文件」。
//! 属主保不住时也不传（原因写明白，让用户先用编辑器打开重新保存）。纯逻辑（[`act_with`]）接 [`EditBackend`]，
//! 内存远端就能测；真实入口（[`act`]）自己拨号开通道。

use std::path::{Path, PathBuf};

use zssh_core::sftp::edit::{EditBackend, OwnerMode};
use zssh_core::sftp::path::file_name;

use super::contract::{RemoteVersion, encode, open_bytes};
use super::drafts::{
    DbDraftCipher, DraftCipher, EditDraft, draft_age_label, export_recovered, list_drafts_with,
    now_unix,
};
use super::remote::{self, SaveOutcome};

/// 一项待恢复的修改（一份编辑草稿）。
#[derive(Clone, Debug)]
pub(crate) struct RecoveryItem {
    /// 列表里和动作里用它指这一项：`draft:<草稿文件名>`。
    pub(crate) id: String,
    /// 草稿文件。
    pub(crate) file: PathBuf,
    pub(crate) draft: Box<EditDraft>,
    /// 来自哪台主机（主机库 id）；旧格式草稿认不出主人，是 `None`。
    pub(crate) host_id: Option<i64>,
    /// 主机名：草稿里没有，由界面按主机库补。
    pub(crate) host_label: Option<String>,
    /// 对应的服务器文件。
    pub(crate) path: Option<String>,
    pub(crate) file_name: String,
    pub(crate) size: u64,
    /// 「几分钟前」。
    pub(crate) age: String,
    /// 没法确认这份草稿属于哪台主机（旧格式），所以保险起见留下了。
    pub(crate) uncertain: bool,
}

impl RecoveryItem {
    /// 能不能在编辑器标签里打开恢复：要认得出主机和路径（草稿提示条会接着给 恢复 / 丢弃）。
    pub(crate) fn can_open_in_editor(&self) -> bool {
        self.host_id.is_some() && self.path.is_some()
    }

    /// 能不能「重新上传」：要认得出主机和文件。
    pub(crate) fn can_reupload(&self) -> bool {
        self.host_id.is_some() && self.path.is_some()
    }
}

/// 列表里一行的说明：哪台主机的哪个文件；认不出来源就只说文件名（同 0.2.14 `recoveryLine`）。
pub(crate) fn recovery_line(item: &RecoveryItem) -> String {
    match &item.path {
        Some(path) => match item.host_label.as_deref().filter(|name| !name.is_empty()) {
            Some(host) => crate::i18n::colon(host, path),
            None => path.clone(),
        },
        None => format!(
            "{}{}",
            item.file_name,
            crate::i18n::t(RecoveryMsg::UnknownFileLineSuffix)
        ),
    }
}

/// 标题：上次有几个文件的修改没有上传。
pub(crate) fn recovery_title(count: usize) -> String {
    format!(
        "{}{}",
        crate::i18n::t(RecoveryMsg::TitlePrefix),
        crate::i18n::count(
            count as u64,
            crate::i18n::t(RecoveryMsg::UnsavedCountSuffix),
            "file with unsaved edits",
            "files with unsaved edits"
        )
    )
}

/// 扫一遍草稿目录（只读不改）。
pub(crate) fn scan(data_dir: &Path) -> Vec<RecoveryItem> {
    let Some(account) = crate::host_db::open_database()
        .ok()
        .map(|db| db.vault_account().to_owned())
    else {
        tracing::warn!("没有资料库身份，无法列出草稿");
        return Vec::new();
    };
    scan_with(&DbDraftCipher::new(account), data_dir)
}

fn scan_with(cipher: &dyn DraftCipher, data_dir: &Path) -> Vec<RecoveryItem> {
    let now = now_unix();
    list_drafts_with(cipher, data_dir)
        .into_iter()
        .map(|found| {
            let draft = found.draft;
            let stem = found
                .file
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            RecoveryItem {
                id: format!("draft:{stem}"),
                host_id: draft.host_id,
                host_label: None,
                path: Some(draft.path.clone()),
                file_name: file_name(&draft.path),
                size: draft.text.len() as u64,
                age: draft_age_label(draft.saved_at_unix, now),
                uncertain: draft.host_id.is_none(),
                file: found.file,
                draft: Box::new(draft),
            }
        })
        .collect()
}

/// 对一项能做的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Discard,
    /// 另存到本地（数据目录下的 `edit-recovered/`，只增不覆盖；不弹文件对话框，路径写在结果里）。
    SaveLocal,
    Reupload,
    /// 看过冲突之后选了「覆盖服务器上的文件」。
    ReuploadOverwrite,
}

/// 一次动作的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// 处理完了，这一项可以从列表里去掉；带一句话（另存到哪了、上传了多少字节）。
    Done(Option<String>),
    /// 服务器上的文件变过（或被删了），没传：用户可以看过之后选覆盖。
    Conflict(String),
    Failed(String),
}

/// 真实入口：重新上传需要开一条 SFTP 通道（连接池里有这台主机的活连接就复用）。
pub(crate) async fn act(item: &RecoveryItem, action: Action, data_dir: &Path) -> Outcome {
    match action {
        Action::Discard | Action::SaveLocal => act_local(item, action, data_dir),
        Action::Reupload | Action::ReuploadOverwrite => {
            let Some(host_id) = item.host_id else {
                return Outcome::Failed(crate::i18n::t(RecoveryMsg::UnknownServerFile).to_owned());
            };
            let channel = match remote::dial_channel(host_id).await {
                Ok(channel) => channel,
                Err(message) => {
                    return Outcome::Failed(format!(
                        "{}{message}",
                        crate::i18n::t(RecoveryMsg::ConnectFailedPrefix)
                    ));
                }
            };
            act_with(&*channel, item, action).await
        }
    }
}

/// 不需要网络的动作：放弃、另存到本地。
fn act_local(item: &RecoveryItem, action: Action, data_dir: &Path) -> Outcome {
    match action {
        Action::Discard => {
            let _ = std::fs::remove_file(&item.file);
            Outcome::Done(None)
        }
        Action::SaveLocal => match export_recovered(data_dir, &item.draft) {
            // 已经另存出来了：草稿本身不动，用户确认没问题再选放弃。
            Ok(path) => Outcome::Done(Some(path.display().to_string())),
            Err(err) => Outcome::Failed(format!(
                "{}{err}",
                crate::i18n::t(RecoveryMsg::SaveFailedPrefix)
            )),
        },
        _ => Outcome::Failed(crate::i18n::t(RecoveryMsg::NeedsServer).to_owned()),
    }
}

/// 重新上传 / 覆盖上传（接任意 [`EditBackend`]，单测用内存远端）。
pub(crate) async fn act_with<B: EditBackend>(
    backend: &B,
    item: &RecoveryItem,
    action: Action,
) -> Outcome {
    let overwrite = action == Action::ReuploadOverwrite;
    reupload_draft(backend, &item.file, &item.draft, overwrite).await
}

/// 编辑草稿重新上传：远端内容的版本和草稿的基准一致才传（草稿是在那个版本上改的）；
/// 换行 / BOM 按远端现在的样子还原，和编辑器保存同一套。
pub(crate) async fn reupload_draft<B: EditBackend>(
    backend: &B,
    file: &Path,
    draft: &EditDraft,
    overwrite: bool,
) -> Outcome {
    if draft.host_id.is_none() {
        return Outcome::Failed(crate::i18n::t(RecoveryMsg::UnknownHostDraft).to_owned());
    }
    // 先读远端当前内容：既要它的版本，也要它的 BOM / 换行样子。
    let current = match remote::fetch_bytes(backend, &draft.path).await {
        Ok(bytes) => Some(bytes),
        Err(message) if is_open_failed(&message) && !overwrite => {
            // 文件不见了（被删 / 被改名）：不替用户重建。
            return Outcome::Conflict(crate::i18n::t(RecoveryMsg::RemoteDeleted).to_owned());
        }
        Err(_) if overwrite => None,
        Err(message) => return Outcome::Failed(message),
    };
    let format = current
        .as_deref()
        .and_then(|bytes| open_bytes(bytes).ok())
        .map(|opened| opened.format)
        .unwrap_or_default();
    let bytes = encode(&draft.text, &format);
    let expected = (!overwrite).then_some(RemoteVersion(draft.base_version));
    match remote::save_remote(backend, &draft.path, &bytes, expected, OwnerMode::Require).await {
        SaveOutcome::Saved { size, .. } => {
            let _ = std::fs::remove_file(file);
            Outcome::Done(Some(crate::i18n::count(
                size as u64,
                crate::i18n::t(RecoveryMsg::ByteSuffix),
                "byte",
                "bytes",
            )))
        }
        SaveOutcome::Conflict(glimpse) => Outcome::Conflict(if glimpse.gone {
            crate::i18n::t(RecoveryMsg::RemoteDeleted).to_owned()
        } else {
            crate::i18n::t(RecoveryMsg::RemoteChanged).to_owned()
        }),
        SaveOutcome::OwnerWouldChange { from, to } => Outcome::Failed(format!(
            "{}{from}{}{to}{}",
            crate::i18n::t(RecoveryMsg::OwnerChangePrefix),
            crate::i18n::t(RecoveryMsg::OwnerChangeMid),
            crate::i18n::t(RecoveryMsg::OwnerChangeSuffix)
        )),
        SaveOutcome::Failed(message) | SaveOutcome::Disconnected(message) => {
            Outcome::Failed(message)
        }
    }
}

/// 远端读取失败（文件不见了、不是普通文件……）：前缀由 `remote` 模块生成，
/// 中英文两种都认，切过语言之后留下的旧消息也认得出。
fn is_open_failed(message: &str) -> bool {
    let key = RecoveryMsg::OpenFailedPrefix;
    message.starts_with(crate::i18n::Msg::zh(&key))
        || message.starts_with(crate::i18n::Msg::en(&key))
}

/// 恢复提示的界面文案。
#[derive(Clone, Copy, Debug)]
enum RecoveryMsg {
    UnknownFileLineSuffix,
    TitlePrefix,
    UnsavedCountSuffix,
    UnknownServerFile,
    ConnectFailedPrefix,
    SaveFailedPrefix,
    NeedsServer,
    UnknownHostDraft,
    OpenFailedPrefix,
    RemoteDeleted,
    RemoteChanged,
    OwnerChangePrefix,
    OwnerChangeMid,
    OwnerChangeSuffix,
    ByteSuffix,
}

impl crate::i18n::Msg for RecoveryMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::UnknownFileLineSuffix => "（认不出对应的服务器文件）",
            Self::TitlePrefix => "上次有 ",
            Self::UnsavedCountSuffix => " 个文件的修改没有上传",
            Self::UnknownServerFile => "这份修改认不出对应的服务器文件，只能另存到本地或放弃",
            Self::ConnectFailedPrefix => "连不上这台主机，没有上传（修改还保留着）：",
            Self::SaveFailedPrefix => "保存失败（修改还在，可以再试）：",
            Self::NeedsServer => "这个动作需要连接服务器",
            Self::UnknownHostDraft => "这份草稿认不出是哪台主机的，只能另存到本地或放弃",
            Self::OpenFailedPrefix => "打开失败",
            Self::RemoteDeleted => "服务器上的文件已经被删掉了，没有上传",
            Self::RemoteChanged => "服务器上的文件在你离开之后又被改过，没有上传",
            Self::OwnerChangePrefix => "重新上传后文件的属主会从 ",
            Self::OwnerChangeMid => " 变成 ",
            Self::OwnerChangeSuffix => {
                "，没有上传。请先打开这个文件，用编辑器重新保存一次再选处理方式。"
            }
            Self::ByteSuffix => " 字节",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::UnknownFileLineSuffix => " (the matching server file could not be identified)",
            Self::TitlePrefix => "Last session left ",
            Self::UnsavedCountSuffix => " file with unsaved edits",
            Self::UnknownServerFile => {
                "This change cannot be matched to a server file; save it locally or discard it"
            }
            Self::ConnectFailedPrefix => {
                "Could not connect to this host; nothing was uploaded (the change is still kept): "
            }
            Self::SaveFailedPrefix => "Save failed (the change is still here, you can try again): ",
            Self::NeedsServer => "This action needs a server connection",
            Self::UnknownHostDraft => {
                "This draft cannot be matched to a host; save it locally or discard it"
            }
            Self::OpenFailedPrefix => "Could not open",
            Self::RemoteDeleted => "The file on the server has been deleted; nothing was uploaded",
            Self::RemoteChanged => {
                "The file on the server changed while you were away; nothing was uploaded"
            }
            Self::OwnerChangePrefix => "Re-uploading would change the file owner from ",
            Self::OwnerChangeMid => " to ",
            Self::OwnerChangeSuffix => {
                "; nothing was uploaded. Open the file, save it again in the editor, then choose how to proceed."
            }
            Self::ByteSuffix => " bytes",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::contract::version_of;
    use super::super::drafts::{Dpapi, draft_file_in, write_draft};
    use super::super::remote::testing::{MemBackend, MemFs, block_on};
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-recovery-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn draft(path: &str, base: &[u8], text: &str) -> EditDraft {
        EditDraft {
            path: path.to_owned(),
            host_id: Some(3),
            base_version: version_of(base).0,
            text: text.to_owned(),
            saved_at_unix: now_unix(),
            revision: 4,
        }
    }

    fn written(dir: &Path, draft: &EditDraft) -> RecoveryItem {
        let file = draft_file_in(dir, 3, &draft.path);
        write_draft(&file, draft).expect("写草稿");
        scan_with(&Dpapi, dir)
            .into_iter()
            .find(|item| item.file == file)
            .expect("扫得到")
    }

    #[test]
    fn scan_lists_drafts_with_their_host_and_path() {
        let dir = temp_dir("scan");
        let item = written(&dir, &draft("/srv/app/a.conf", b"old", "new"));
        assert_eq!(item.host_id, Some(3));
        assert_eq!(item.path.as_deref(), Some("/srv/app/a.conf"));
        assert_eq!(item.file_name, "a.conf");
        assert!(item.can_open_in_editor() && item.can_reupload());
        assert!(!item.uncertain);
        assert_eq!(recovery_line(&item), "/srv/app/a.conf");
        assert_eq!(recovery_title(2), "上次有 2 个文件的修改没有上传");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_drafts_are_listed_but_cannot_be_reuploaded() {
        let dir = temp_dir("legacy");
        let mut old = draft("/srv/app/a.conf", b"old", "new");
        old.host_id = None;
        let file = super::super::drafts::legacy_draft_file_in(&dir, &old.path);
        write_draft(&file, &old).expect("写旧格式草稿");
        let item = scan_with(&Dpapi, &dir).into_iter().next().expect("扫得到");
        assert!(item.uncertain && !item.can_reupload() && !item.can_open_in_editor());
        // 不能重传，但可以另存、可以放弃。
        let backend = MemBackend(MemFs::default());
        let (file, draft) = (&item.file, &item.draft);
        let outcome = block_on(reupload_draft(&backend, file, draft, false));
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 异常退出后的恢复链：远端没变 → 重新上传成功，远端是草稿内容，草稿文件没了。
    #[test]
    fn reupload_sends_the_draft_when_the_remote_is_unchanged() {
        let dir = temp_dir("reupload");
        let base = b"server {\n  listen 80;\n}\n";
        let backend = MemBackend(MemFs::with("/etc/nginx/site.conf", base));
        let item = written(
            &dir,
            &draft(
                "/etc/nginx/site.conf",
                base,
                "server {\n  listen 8080;\n}\n",
            ),
        );
        let outcome = block_on(act_with(&backend, &item, Action::Reupload));
        assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
        assert_eq!(
            backend.0.get("/etc/nginx/site.conf").as_deref(),
            Some(&b"server {\n  listen 8080;\n}\n"[..])
        );
        let file = &item.file;
        assert!(!file.exists(), "上传成功后草稿删掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reupload_keeps_the_crlf_and_bom_of_the_remote_file() {
        let dir = temp_dir("crlf");
        let base = b"\xEF\xBB\xBFa\r\nb\r\n";
        let backend = MemBackend(MemFs::with("/srv/win.txt", base));
        let item = written(&dir, &draft("/srv/win.txt", base, "a\nB\n"));
        let outcome = block_on(act_with(&backend, &item, Action::Reupload));
        assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
        assert_eq!(
            backend.0.get("/srv/win.txt").as_deref(),
            Some(&b"\xEF\xBB\xBFa\r\nB\r\n"[..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 远端在这期间被改过：不传，草稿原样留着；用户看过后选覆盖才传。
    #[test]
    fn reupload_reports_a_conflict_and_overwrite_then_goes_through() {
        let dir = temp_dir("conflict");
        let base = b"v1\n";
        let backend = MemBackend(MemFs::with("/srv/a.txt", b"edited by someone else\n"));
        let item = written(&dir, &draft("/srv/a.txt", base, "my draft\n"));
        let outcome = block_on(act_with(&backend, &item, Action::Reupload));
        assert!(matches!(outcome, Outcome::Conflict(_)), "{outcome:?}");
        assert_eq!(
            backend.0.get("/srv/a.txt").as_deref(),
            Some(&b"edited by someone else\n"[..])
        );
        let file = &item.file;
        assert!(file.exists(), "冲突时草稿必须留着");
        let outcome = block_on(act_with(&backend, &item, Action::ReuploadOverwrite));
        assert!(matches!(outcome, Outcome::Done(_)), "{outcome:?}");
        assert_eq!(
            backend.0.get("/srv/a.txt").as_deref(),
            Some(&b"my draft\n"[..])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_remote_file_is_a_conflict_not_a_silent_recreate() {
        let dir = temp_dir("gone");
        let backend = MemBackend(MemFs::default());
        let item = written(&dir, &draft("/srv/gone.txt", b"v1", "mine"));
        let outcome = block_on(act_with(&backend, &item, Action::Reupload));
        assert!(matches!(outcome, Outcome::Conflict(_)), "{outcome:?}");
        assert!(backend.0.get("/srv/gone.txt").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_local_exports_without_touching_the_draft_and_discard_removes_it() {
        let dir = temp_dir("local");
        let item = written(&dir, &draft("/srv/app/a.conf", b"old", "unsaved text"));
        let outcome = act_local(&item, Action::SaveLocal, &dir);
        let Outcome::Done(Some(saved)) = outcome else {
            panic!("应该另存成功：{outcome:?}");
        };
        assert_eq!(
            std::fs::read_to_string(&saved).expect("读回"),
            "unsaved text"
        );
        assert!(saved.contains("edit-recovered"), "{saved}");
        let file = &item.file;
        assert!(file.exists(), "另存之后草稿还在，等用户确认");
        assert_eq!(act_local(&item, Action::Discard, &dir), Outcome::Done(None));
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn owner_that_cannot_be_kept_blocks_the_reupload_with_a_clear_reason() {
        let dir = temp_dir("owner");
        let base = b"v1\n";
        let mem = MemFs::with("/etc/app.conf", base);
        *mem.deny_owner.lock().expect("锁") = true;
        let backend = MemBackend(mem);
        let item = written(&dir, &draft("/etc/app.conf", base, "v2\n"));
        let outcome = block_on(act_with(&backend, &item, Action::Reupload));
        let Outcome::Failed(message) = outcome else {
            panic!("应该失败");
        };
        assert!(message.contains("属主"), "{message}");
        assert_eq!(backend.0.get("/etc/app.conf").as_deref(), Some(&base[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
