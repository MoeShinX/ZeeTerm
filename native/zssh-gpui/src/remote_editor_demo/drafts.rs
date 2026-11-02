//! 编辑草稿：磁盘格式、文件名、原子写、后台写线程、导出，以及启动时扫描留下的草稿（恢复用）。
//!
//! 从 F3 的远端编辑器里原样搬出来的部分（草稿键 = 主机 id + 规范化路径、旧格式草稿只读不删、
//! 同一路径在队列里只留最后一条）都没改语义；G09 加了两样：
//! - **revision**：草稿里记着写它时编辑器内容的修订号，保存成功只删「修订号不晚于这次保存」的草稿
//!   （[`DraftJob::DeleteUpTo`]），保存期间继续输入产生的更新草稿不会被误删；
//! - **扫描**：[`list_drafts`] 把数据目录下所有草稿列出来，启动时的恢复提示用它。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zssh_core::sftp::edit::sanitize_local_name;
use zssh_core::sftp::path::{file_name, normalize};

use super::contract::{MAX_BYTES, RemoteVersion};

/// 草稿目录：和 `ui-settings.json` 一起放在数据目录下。
pub(super) const DRAFT_DIR: &str = "edit-drafts";
/// 用户把旧版本草稿「另存为本地文件」时落的目录（同样在数据目录下，只增不覆盖）。
pub(super) const RECOVERED_DIR: &str = "edit-recovered";
/// 草稿文件上限：编辑上限的文本再加 JSON 转义余量；再大就不会是正常草稿，直接删掉。
pub(super) const DRAFT_FILE_LIMIT: u64 = MAX_BYTES as u64 * 2;

/// 一份未保存的编辑草稿；字段名就是磁盘上的 JSON 格式，改名要先想好旧草稿怎么办。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EditDraft {
    pub(super) path: String,
    /// 草稿属于哪台主机（主机库 id）。旧格式没有这个字段：读出来是 `None`，认不出主人。
    #[serde(default)]
    pub(super) host_id: Option<i64>,
    pub(super) base_version: u64,
    pub(super) text: String,
    pub(super) saved_at_unix: u64,
    /// 写这份草稿时编辑器内容的修订号（每次用户改动 +1，只增不减）。旧草稿没有这个字段，读出来是 0。
    /// 保存成功只删修订号不晚于那次保存的草稿，保存期间继续输入产生的新草稿留着。
    #[serde(default)]
    pub(super) revision: u64,
}

/// 加载完成后草稿能怎么处理：基准版本和这次读到的一致才能恢复；基准不一致（远端后来变了）
/// 或认不出主人的旧格式，不自动恢复，但正文都留着，用户可以复制出来或另存成本地文件。
pub(super) enum DraftNotice {
    Restorable(EditDraft),
    /// 基准版本和现在的远端不一致。
    Stale(EditDraft),
    /// 旧格式（只按路径命名、没有主机 id）：不知道是哪台主机的，原文件原样保留。
    Legacy(EditDraft),
}

impl DraftNotice {
    pub(super) fn draft(&self) -> &EditDraft {
        match self {
            Self::Restorable(draft) | Self::Stale(draft) | Self::Legacy(draft) => draft,
        }
    }
}

/// FNV-1a 64 位。不用 `DefaultHasher`：它不保证跨 Rust 版本稳定，升级之后草稿就对不上了。
pub(super) fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 草稿文件名：`h<主机id>-<哈希>`，哈希 = 主机 id + 规范化后的远端路径。
///
/// 主机进键：不同主机上同一个路径（`/etc/hosts`、`~/.bashrc`）不再共用一份草稿。
/// 只用哈希、不用远端路径本身：远端路径里的 `/`、`..`、`\`、盘符都进不了本地文件名，
/// 也就没有路径穿越。路径先规范化，`/a//b/../c` 和 `/a/c` 是同一个文件。
pub(super) fn draft_key(host_id: i64, remote_path: &str) -> String {
    let normalized = normalize(remote_path);
    let hash = fnv1a(
        host_id
            .to_le_bytes()
            .into_iter()
            .chain(std::iter::once(0u8))
            .chain(normalized.bytes()),
    );
    format!("h{host_id}-{hash:016x}")
}

/// 旧版本（只按路径命名）的草稿文件名，只用来找到并保留旧文件。
pub(super) fn legacy_draft_key(remote_path: &str) -> String {
    format!("{:016x}", fnv1a(remote_path.bytes()))
}

pub(super) fn draft_dir_in(base: &Path) -> PathBuf {
    base.join(DRAFT_DIR)
}

/// 草稿路径按数据目录拼；`base` 单独传是为了测试能用临时目录。
pub(super) fn draft_file_in(base: &Path, host_id: i64, remote_path: &str) -> PathBuf {
    draft_dir_in(base).join(format!("{}.json", draft_key(host_id, remote_path)))
}

pub(super) fn legacy_draft_file_in(base: &Path, remote_path: &str) -> PathBuf {
    draft_dir_in(base).join(format!("{}.json", legacy_draft_key(remote_path)))
}

pub(super) fn draft_notice(
    draft: Option<EditDraft>,
    version: RemoteVersion,
) -> Option<DraftNotice> {
    draft.map(|draft| {
        if draft.base_version == version.0 {
            DraftNotice::Restorable(draft)
        } else {
            DraftNotice::Stale(draft)
        }
    })
}

/// 写草稿：同目录临时文件 + `sync_all` + 改名，崩溃时不会留下半截 JSON。
pub(super) fn write_draft(path: &Path, draft: &EditDraft) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec(draft)?;
    // 临时名带进程号和自增序号：多个窗口同时编辑不同文件也不会互相截断。
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("draft");
    let temp = path.with_file_name(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// 解析一份草稿文件：读不了、太大、JSON 坏了、文本超限都是 `None`。
///
/// `delete_broken` 为真时，确定坏掉的文件（超限、坏 JSON、文本超上限）顺手删掉，免得每次
/// 打开都重试；旧格式文件不删（认不出主人的东西宁可留着）。单纯读文件失败（比如被别的
/// 程序占着）一律先留着，下次再说。
pub(super) fn parse_draft_file(path: &Path, delete_broken: bool) -> Option<EditDraft> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > DRAFT_FILE_LIMIT {
        if delete_broken {
            remove_draft(path);
        }
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let draft: EditDraft = match serde_json::from_slice(&bytes) {
        Ok(draft) => draft,
        Err(_) => {
            if delete_broken {
                remove_draft(path);
            }
            return None;
        }
    };
    if draft.text.len() > MAX_BYTES {
        if delete_broken {
            remove_draft(path);
        }
        return None;
    }
    Some(draft)
}

/// 读这台主机、这个路径的草稿：主机 id 和路径都要对上，对不上的当没有（也不删别人的）。
pub(super) fn read_draft(path: &Path, host_id: i64, remote_path: &str) -> Option<EditDraft> {
    let draft = parse_draft_file(path, true)?;
    (draft.host_id == Some(host_id) && normalize(&draft.path) == normalize(remote_path))
        .then_some(draft)
}

/// 找同路径的旧格式草稿（没有主机 id）。只读不删：认不出主人，不自动恢复。
pub(super) fn read_legacy_draft(path: &Path, remote_path: &str) -> Option<EditDraft> {
    let draft = parse_draft_file(path, false)?;
    (draft.host_id.is_none() && draft.path == remote_path).then_some(draft)
}

/// 把一份草稿存成本地文件（只增不覆盖）：用户从提示条选「另存为本地文件」时用。
///
/// 放在数据目录的 `edit-recovered` 下，文件名取远端文件名（去掉 Windows 不认的字符）加保存时间，
/// 重名就加序号。
pub(super) fn export_recovered(base: &Path, draft: &EditDraft) -> std::io::Result<PathBuf> {
    let dir = base.join(RECOVERED_DIR);
    std::fs::create_dir_all(&dir)?;
    let name = sanitize_local_name(&file_name(&draft.path));
    for attempt in 0..1000u32 {
        let candidate = if attempt == 0 {
            format!("{name}.{}.txt", draft.saved_at_unix)
        } else {
            format!("{name}.{}-{attempt}.txt", draft.saved_at_unix)
        };
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let written = file
                    .write_all(draft.text.as_bytes())
                    .and_then(|()| file.sync_all());
                if let Err(err) = written {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(err);
                }
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "同名的导出文件太多",
    ))
}

/// 删草稿：文件本来就不在不算错误（保存、丢弃、清理都会走这里）。
pub(super) fn remove_draft(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(%err, path = %path.display(), "删除编辑器草稿失败"),
    }
}

/// 草稿写线程的任务。落盘位置由界面算好带过来（和界面用同一个 `draft_file_in`），
/// 线程不用再猜主机和路径对应哪个文件。
pub(super) enum DraftJob {
    Save(PathBuf, EditDraft),
    /// 无条件删（用户主动丢弃、明确选了「不保存」）。
    Delete(PathBuf),
    /// 保存成功之后删：只删修订号不晚于 `revision` 的草稿。保存期间继续输入产生的更新草稿
    /// （修订号更大）留着，下次还能恢复出来。
    DeleteUpTo(PathBuf, u64),
    /// 排在前面的任务都处理完再回一条。退出前用它等草稿落盘；测试也靠它，免得用 sleep 猜磁盘进度。
    Flush(Sender<()>),
}

/// 草稿写线程：界面只发任务，`serde_json`、临时文件、`sync_all` 和改名都在后台做。
///
/// 同一个路径在队列里只留最后一条任务：磁盘慢的时候积压的是「最新状态」，不是历史；
/// 先 Save 后 Delete 也一定按入队顺序落到磁盘。
pub(super) struct DraftWriter {
    sender: Option<Sender<DraftJob>>,
}

impl DraftWriter {
    pub(super) fn spawn() -> Self {
        let (sender, jobs) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("zssh-draft-writer".to_owned())
            .spawn(move || run_draft_writer(jobs));
        match spawned {
            Ok(_) => Self {
                sender: Some(sender),
            },
            Err(err) => {
                // 线程起不来时宁可不写草稿，也不能退回界面线程同步写。
                tracing::warn!(%err, "启动草稿写线程失败，这次编辑不会保存草稿");
                Self { sender: None }
            }
        }
    }

    /// 发送任务；线程已经退出（窗口正在关）时安静丢掉，界面不报错也不等待。
    pub(super) fn send(&self, job: DraftJob) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(job);
        }
    }

    /// 关标签：只丢发送端。线程会先把已经排队的任务处理完再退出，界面不等它。
    pub(super) fn close(&mut self) {
        self.sender = None;
    }

    /// 等前面排队的任务全部落盘，最多等 `wait`；返回是否真的处理完了（线程不在了算没完成）。
    pub(super) fn flush(&self, wait: Duration) -> bool {
        let (ack_sender, ack) = std::sync::mpsc::channel();
        let Some(sender) = self.sender.as_ref() else {
            return false;
        };
        if sender.send(DraftJob::Flush(ack_sender)).is_err() {
            return false;
        }
        ack.recv_timeout(wait).is_ok()
    }
}

/// 同一路径的待办任务合并：只留最新的，但「保存成功之后删」不能盖掉修订号更大的待写草稿。
pub(super) fn queue_draft_job(
    pending: &mut HashMap<PathBuf, DraftJob>,
    flushes: &mut Vec<Sender<()>>,
    job: DraftJob,
) {
    match job {
        DraftJob::Save(path, draft) => {
            pending.insert(path.clone(), DraftJob::Save(path, draft));
        }
        DraftJob::Delete(path) => {
            pending.insert(path.clone(), DraftJob::Delete(path));
        }
        DraftJob::DeleteUpTo(path, revision) => {
            // 队列里已经有一份更新的草稿等着写：这次删除只针对旧的，不能把它顶掉。
            let newer_pending = matches!(
                pending.get(&path),
                Some(DraftJob::Save(_, draft)) if draft.revision > revision
            );
            if !newer_pending {
                pending.insert(path.clone(), DraftJob::DeleteUpTo(path, revision));
            }
        }
        DraftJob::Flush(ack) => flushes.push(ack),
    }
}

/// 写线程主循环：每次醒来把通道里已经排队的任务全取走、按路径合并，再统一落盘。
///
/// 发送端全丢掉之后再 `recv` 会返回 `Err`，这时队列已经处理完，线程自己退出。
pub(super) fn run_draft_writer(jobs: Receiver<DraftJob>) {
    while let Ok(first) = jobs.recv() {
        let mut pending = HashMap::new();
        let mut flushes = Vec::new();
        queue_draft_job(&mut pending, &mut flushes, first);
        while let Ok(job) = jobs.try_recv() {
            queue_draft_job(&mut pending, &mut flushes, job);
        }
        for (path, job) in pending {
            match job {
                DraftJob::Save(_, draft) => {
                    if let Err(err) = write_draft(&path, &draft) {
                        tracing::warn!(%err, path = %path.display(), "写编辑器草稿失败");
                    }
                }
                // 用任务里带的路径删，和入队时界面算的是同一个文件。
                DraftJob::Delete(path) => remove_draft(&path),
                DraftJob::DeleteUpTo(path, revision) => remove_draft_up_to(&path, revision),
                DraftJob::Flush(_) => {}
            }
        }
        for flush in flushes {
            let _ = flush.send(());
        }
    }
}

/// 磁盘上的草稿修订号不晚于 `revision` 才删；读不出来（文件坏了 / 不在）按「没有可保护的新草稿」处理。
pub(super) fn remove_draft_up_to(path: &Path, revision: u64) {
    if let Some(draft) = parse_draft_file(path, false)
        && draft.revision > revision
    {
        return;
    }
    remove_draft(path);
}

/// 当前 Unix 秒；时钟早于纪元时按 0 处理（这个值只用来显示）。
pub(super) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// 草稿时间只显示相对时间：够判断是不是刚才那次编辑，也不用为本地时区引入依赖。
pub(super) fn draft_age_label(saved_at_unix: u64, now_unix: u64) -> String {
    let age = now_unix.saturating_sub(saved_at_unix);
    if age < 60 {
        "刚刚".to_owned()
    } else if age < 3_600 {
        format!("{} 分钟前", age / 60)
    } else if age < 86_400 {
        format!("{} 小时前", age / 3_600)
    } else {
        format!("{} 天前", age / 86_400)
    }
}

/// 启动时扫出来的一份草稿（恢复提示用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FoundDraft {
    /// 草稿文件。
    pub(super) file: PathBuf,
    pub(super) draft: EditDraft,
}

/// 把数据目录下所有能读出来的草稿列出来，最近改的在前。
///
/// 只读不改：坏的、太大的、读不了的文件一概跳过（不删——这里是「列一列」，删不删是各自的事）；
/// 临时文件（`*.tmp`）是写到一半的残留，不算草稿。文本为空的草稿也列（用户可能确实清空了文件）。
pub(super) fn list_drafts(base: &Path) -> Vec<FoundDraft> {
    let Ok(entries) = std::fs::read_dir(draft_dir_in(base)) else {
        return Vec::new();
    };
    let mut found: Vec<FoundDraft> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|file| {
            let draft = parse_draft_file(&file, false)?;
            Some(FoundDraft { file, draft })
        })
        .collect();
    found.sort_by(|a, b| {
        b.draft
            .saved_at_unix
            .cmp(&a.draft.saved_at_unix)
            .then_with(|| a.file.cmp(&b.file))
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zssh-edit-drafts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 样例草稿属于主机 7。
    const HOST: i64 = 7;

    fn sample_draft(text: &str) -> EditDraft {
        EditDraft {
            path: "/srv/app/main.rs".to_owned(),
            host_id: Some(HOST),
            base_version: 0x1234_5678,
            text: text.to_owned(),
            saved_at_unix: 1_700_000_000,
            revision: 0,
        }
    }

    #[test]
    fn draft_key_is_a_stable_hex_name() {
        let key = draft_key(HOST, "/srv/app/main.rs");
        let (prefix, hash) = key.split_once('-').expect("h<id>-<哈希>");
        assert_eq!(prefix, "h7");
        assert_eq!(hash.len(), 16);
        assert!(
            hash.chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        );
        assert_eq!(key, draft_key(HOST, "/srv/app/main.rs"));
        assert_ne!(key, draft_key(HOST, "/srv/app/main.rs "));
        assert_ne!(key, draft_key(HOST, "/srv/app/other.rs"));
    }

    /// E2：不同主机上同一个路径不共用草稿；写法不同但指向同一文件的路径共用。
    #[test]
    fn draft_key_separates_hosts_and_normalizes_paths() {
        assert_ne!(
            draft_key(1, "/etc/hosts"),
            draft_key(2, "/etc/hosts"),
            "同路径、不同主机必须是两份草稿"
        );
        assert_eq!(
            draft_key(1, "/srv//app/../app/main.rs"),
            draft_key(1, "/srv/app/main.rs")
        );
        // 新格式文件名不会和旧格式（纯 16 位十六进制）撞名。
        assert_ne!(draft_key(1, "/etc/hosts"), legacy_draft_key("/etc/hosts"));
    }

    /// E2：A 主机的草稿，B 主机打开同一个路径时读不到，也不会被删。
    #[test]
    fn a_draft_is_only_read_back_for_its_own_host() {
        let dir = temp_dir("host-isolation");
        let path_a = draft_file_in(&dir, 1, "/etc/hosts");
        let path_b = draft_file_in(&dir, 2, "/etc/hosts");
        assert_ne!(path_a, path_b);
        let mut draft = sample_draft("A 主机的改动");
        draft.path = "/etc/hosts".to_owned();
        draft.host_id = Some(1);
        write_draft(&path_a, &draft).expect("写草稿");

        assert_eq!(read_draft(&path_a, 1, "/etc/hosts"), Some(draft.clone()));
        // B 主机根本不会去读 A 的文件；就算有人把文件塞到 B 的名字下，主机 id 对不上也不认。
        assert!(read_draft(&path_b, 2, "/etc/hosts").is_none());
        std::fs::copy(&path_a, &path_b).expect("复制成 B 的文件名");
        assert!(read_draft(&path_b, 2, "/etc/hosts").is_none());
        assert!(path_b.is_file(), "认不出的草稿不能被删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E2：旧格式（没有主机 id）的草稿能被认出来，但不会当作任何主机的草稿，更不会被删。
    #[test]
    fn legacy_drafts_are_found_but_never_adopted_or_deleted() {
        let dir = temp_dir("legacy");
        let legacy_path = legacy_draft_file_in(&dir, "/srv/app/main.rs");
        std::fs::create_dir_all(draft_dir_in(&dir)).expect("建草稿目录");
        let json = r#"{"path":"/srv/app/main.rs","base_version":42,"text":"旧草稿","saved_at_unix":1700000000}"#;
        std::fs::write(&legacy_path, json).expect("写旧格式草稿");

        let found = read_legacy_draft(&legacy_path, "/srv/app/main.rs").expect("认出旧草稿");
        assert_eq!(found.host_id, None);
        assert_eq!(found.text, "旧草稿");
        // 新格式的读取不认它。
        assert!(read_draft(&legacy_path, HOST, "/srv/app/main.rs").is_none());
        assert!(legacy_path.is_file(), "旧格式草稿读完还在");
        assert_eq!(std::fs::read_to_string(&legacy_path).expect("读回"), json);
        // 路径对不上的不当成它。
        assert!(read_legacy_draft(&legacy_path, "/srv/app/other.rs").is_none());
        // 坏掉的旧文件也不删。
        std::fs::write(&legacy_path, b"{not json").expect("写坏旧草稿");
        assert!(read_legacy_draft(&legacy_path, "/srv/app/main.rs").is_none());
        assert!(legacy_path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E3：旧版本草稿能存成本地文件，内容原样，重名不覆盖。
    #[test]
    fn stale_drafts_export_to_new_local_files_without_overwriting() {
        let dir = temp_dir("export");
        let mut draft = sample_draft("旧版本里的改动\n第二行");
        draft.path = "/srv/app/CON.txt".to_owned();
        let first = export_recovered(&dir, &draft).expect("第一次导出");
        let second = export_recovered(&dir, &draft).expect("同名再导出一次");
        assert_ne!(first, second, "重名不能覆盖上一份");
        assert_eq!(
            std::fs::read_to_string(&first).expect("读回"),
            "旧版本里的改动\n第二行"
        );
        let name = first
            .file_name()
            .and_then(|name| name.to_str())
            .expect("文件名");
        assert!(!name.to_ascii_uppercase().starts_with("CON."), "{name}");
        assert_eq!(first.parent(), Some(dir.join("edit-recovered").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_path_never_leaves_the_draft_directory() {
        let base = Path::new("drafts");
        let dir = draft_dir_in(base);
        for remote in [
            "/srv/app/main.rs",
            "../../../windows/system32/config",
            "C:\\Users\\..\\x",
            "",
            "/",
        ] {
            let path = draft_file_in(base, HOST, remote);
            assert_eq!(path.parent(), Some(dir.as_path()), "{remote}");
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("草稿文件名");
            assert!(name.ends_with(".json"), "{name}");
            assert!(
                !name.contains("..") && !name.contains('/') && !name.contains('\\'),
                "{name}"
            );
        }
    }

    #[test]
    fn draft_json_round_trips() {
        let draft = sample_draft("第一行\nsecond line");
        let json = serde_json::to_string(&draft).expect("序列化草稿");
        for field in [
            "\"path\"",
            "\"host_id\"",
            "\"base_version\"",
            "\"text\"",
            "\"saved_at_unix\"",
        ] {
            assert!(json.contains(field), "缺少字段 {field}：{json}");
        }
        let parsed: EditDraft = serde_json::from_str(&json).expect("反序列化草稿");
        assert_eq!(parsed, draft);
    }

    #[test]
    fn draft_notice_depends_on_the_base_version() {
        let draft = sample_draft("草稿");
        let version = RemoteVersion(draft.base_version);
        assert!(matches!(
            draft_notice(Some(draft.clone()), version),
            Some(DraftNotice::Restorable(_))
        ));
        assert!(matches!(
            draft_notice(Some(draft.clone()), RemoteVersion(draft.base_version + 1)),
            Some(DraftNotice::Stale(_))
        ));
        assert!(draft_notice(None, version).is_none());
    }

    #[test]
    fn draft_round_trips_through_an_atomic_write() {
        let dir = temp_dir("roundtrip");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        let draft = sample_draft("未保存的内容\n");

        write_draft(&path, &draft).expect("写草稿");
        assert!(path.is_file());
        // 原子写用的是同目录临时文件，成功后不该留下。
        let leftovers: Vec<PathBuf> = std::fs::read_dir(draft_dir_in(&dir))
            .expect("列草稿目录")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|entry| entry != &path)
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");

        // 换个路径读同一份文件：路径对不上，不用也不删。
        assert!(read_draft(&path, HOST, "/srv/app/other.rs").is_none());
        assert!(path.is_file());
        assert_eq!(read_draft(&path, HOST, "/srv/app/main.rs"), Some(draft));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_draft_text_is_still_a_draft() {
        let dir = temp_dir("empty");
        let path = draft_file_in(&dir, HOST, "/srv/app/empty.txt");
        let draft = EditDraft {
            path: "/srv/app/empty.txt".to_owned(),
            host_id: Some(HOST),
            base_version: 7,
            text: String::new(),
            saved_at_unix: 10,
            revision: 0,
        };
        write_draft(&path, &draft).expect("写空草稿");
        assert_eq!(read_draft(&path, HOST, "/srv/app/empty.txt"), Some(draft));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_drafts_are_ignored_and_removed() {
        let dir = temp_dir("broken");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        std::fs::create_dir_all(draft_dir_in(&dir)).expect("建草稿目录");

        // 坏 JSON：删掉，读成没有。
        std::fs::write(&path, b"{not json").expect("写坏草稿");
        assert!(read_draft(&path, HOST, "/srv/app/main.rs").is_none());
        assert!(!path.exists(), "坏 JSON 应该被删掉");

        // 文本超过 4 MiB：删掉。
        let mut oversized = sample_draft("");
        oversized.text = "a".repeat(MAX_BYTES + 1);
        std::fs::write(&path, serde_json::to_vec(&oversized).expect("序列化")).expect("写超大草稿");
        assert!(read_draft(&path, HOST, "/srv/app/main.rs").is_none());
        assert!(!path.exists(), "文本超限的草稿应该被删掉");

        // 整个文件就超过上限：连解析都不做，直接删。
        std::fs::write(&path, vec![b'x'; DRAFT_FILE_LIMIT as usize + 1]).expect("写更大的文件");
        assert!(read_draft(&path, HOST, "/srv/app/main.rs").is_none());
        assert!(!path.exists(), "超过文件上限的草稿应该被删掉");

        // 文件不存在：安静地当作没有。
        assert!(read_draft(&path, HOST, "/srv/app/main.rs").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_writer_delete_after_save_leaves_no_file() {
        let base = temp_dir("writer-delete");
        let path = draft_file_in(&base, HOST, "/srv/app/main.rs");
        let writer = DraftWriter::spawn();

        writer.send(DraftJob::Save(path.clone(), sample_draft("未保存的内容")));
        writer.send(DraftJob::Delete(path.clone()));
        assert!(
            writer.flush(Duration::from_secs(5)),
            "草稿写线程没有在超时前处理完"
        );

        assert!(!path.exists(), "Save 之后入队的 Delete 必须生效");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn draft_writer_keeps_only_the_latest_save() {
        let base = temp_dir("writer-latest");
        let path = draft_file_in(&base, HOST, "/srv/app/main.rs");
        let writer = DraftWriter::spawn();

        let mut older = sample_draft("旧内容");
        older.saved_at_unix = 1_700_000_000;
        let mut newer = sample_draft("新内容");
        newer.saved_at_unix = 1_700_000_001;
        writer.send(DraftJob::Save(path.clone(), older));
        writer.send(DraftJob::Save(path.clone(), newer.clone()));
        assert!(
            writer.flush(Duration::from_secs(5)),
            "草稿写线程没有在超时前处理完"
        );

        assert_eq!(read_draft(&path, HOST, "/srv/app/main.rs"), Some(newer));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn draft_age_label_is_relative() {
        let now = 1_700_000_000;
        assert_eq!(draft_age_label(now, now), "刚刚");
        // 时钟倒退（保存时间比现在还晚）也按刚刚处理。
        assert_eq!(draft_age_label(now + 30, now), "刚刚");
        assert_eq!(draft_age_label(now - 120, now), "2 分钟前");
        assert_eq!(draft_age_label(now - 3 * 3_600, now), "3 小时前");
        assert_eq!(draft_age_label(now - 2 * 86_400, now), "2 天前");
    }

    fn at_revision(text: &str, revision: u64) -> EditDraft {
        EditDraft {
            revision,
            ..sample_draft(text)
        }
    }

    #[test]
    fn old_drafts_without_a_revision_read_as_revision_zero() {
        let json = r#"{"path":"/a","host_id":3,"base_version":1,"text":"t","saved_at_unix":5}"#;
        let draft: EditDraft = serde_json::from_str(json).expect("旧格式能读");
        assert_eq!(draft.revision, 0);
    }

    /// 保存成功只删「不晚于那次保存」的草稿：保存期间继续输入写下的更新草稿必须留着。
    #[test]
    fn saving_deletes_only_drafts_that_are_not_newer_than_the_save() {
        let dir = temp_dir("rev-delete");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        write_draft(&path, &at_revision("typed during the save", 9)).expect("写");
        remove_draft_up_to(&path, 5);
        assert!(path.exists(), "修订号 9 的草稿晚于第 5 次保存，不能删");
        remove_draft_up_to(&path, 9);
        assert!(!path.exists(), "不晚于保存的草稿要删掉");
        // 文件本来就不在：安静。
        remove_draft_up_to(&path, 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_queued_newer_draft_survives_a_delete_for_an_older_save() {
        let path = PathBuf::from("/x/draft.json");
        let mut pending = HashMap::new();
        let mut flushes = Vec::new();
        queue_draft_job(
            &mut pending,
            &mut flushes,
            DraftJob::Save(path.clone(), at_revision("new", 8)),
        );
        queue_draft_job(
            &mut pending,
            &mut flushes,
            DraftJob::DeleteUpTo(path.clone(), 7),
        );
        assert!(
            matches!(pending.get(&path), Some(DraftJob::Save(_, draft)) if draft.revision == 8),
            "更新的草稿不能被旧保存的删除顶掉"
        );
        // 反过来，待写的草稿不比保存新：被这次删除替换。
        queue_draft_job(
            &mut pending,
            &mut flushes,
            DraftJob::DeleteUpTo(path.clone(), 8),
        );
        assert!(matches!(
            pending.get(&path),
            Some(DraftJob::DeleteUpTo(_, 8))
        ));
        // 删除之后又来了新草稿：新草稿赢。
        queue_draft_job(
            &mut pending,
            &mut flushes,
            DraftJob::Save(path.clone(), at_revision("later", 10)),
        );
        assert!(
            matches!(pending.get(&path), Some(DraftJob::Save(_, draft)) if draft.revision == 10)
        );
    }

    #[test]
    fn the_writer_applies_delete_up_to_in_order_with_saves() {
        let base = temp_dir("rev-writer");
        let path = draft_file_in(&base, HOST, "/srv/app/main.rs");
        let writer = DraftWriter::spawn();
        // 第 5 次保存成功前后的一串任务：写草稿 5、保存成功（删到 5）、保存期间又打字写草稿 6。
        writer.send(DraftJob::Save(path.clone(), at_revision("v5", 5)));
        writer.send(DraftJob::DeleteUpTo(path.clone(), 5));
        writer.send(DraftJob::Save(path.clone(), at_revision("v6", 6)));
        assert!(writer.flush(Duration::from_secs(5)));
        assert_eq!(
            read_draft(&path, HOST, "/srv/app/main.rs").map(|draft| draft.text),
            Some("v6".to_owned())
        );
        // 只有保存成功，没有更新的草稿：删光。
        writer.send(DraftJob::DeleteUpTo(path.clone(), 6));
        assert!(writer.flush(Duration::from_secs(5)));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn listing_finds_every_readable_draft_newest_first_and_skips_junk() {
        let base = temp_dir("list");
        let first = draft_file_in(&base, HOST, "/srv/a.txt");
        let second = draft_file_in(&base, HOST + 1, "/srv/b.txt");
        let mut older = sample_draft("a");
        older.path = "/srv/a.txt".to_owned();
        older.saved_at_unix = 100;
        let mut newer = sample_draft("b");
        newer.path = "/srv/b.txt".to_owned();
        newer.host_id = Some(HOST + 1);
        newer.saved_at_unix = 200;
        write_draft(&first, &older).expect("写");
        write_draft(&second, &newer).expect("写");
        std::fs::write(draft_dir_in(&base).join("junk.json"), b"not json").expect("坏文件");
        std::fs::write(draft_dir_in(&base).join("half.json.123.0.tmp"), b"{").expect("残留");
        let found = list_drafts(&base);
        assert_eq!(
            found
                .iter()
                .map(|f| f.draft.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/srv/b.txt", "/srv/a.txt"]
        );
        // 只读：坏文件没有被删。
        assert!(draft_dir_in(&base).join("junk.json").exists());
        assert!(list_drafts(&base.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
