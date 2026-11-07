//! 编辑草稿：磁盘格式、文件名、原子写、后台写线程、导出，以及启动时扫描留下的草稿（恢复用）。
//!
//! 从 F3 的远端编辑器里原样搬出来的部分（草稿键 = 主机 id + 规范化路径、旧格式草稿只读不删、
//! 同一路径在队列里只留最后一条）都没改语义；G09 加了两样：
//! - **revision**：草稿里记着写它时编辑器内容的修订号，保存成功只删「修订号不晚于这次保存」的草稿
//!   （[`DraftJob::DeleteUpTo`]），保存期间继续输入产生的更新草稿不会被误删；
//! - **扫描**：[`list_drafts`] 把数据目录下所有草稿列出来，启动时的恢复提示用它。
//!
//! S01：草稿落盘前整份用 Windows DPAPI（当前登录用户）加密，磁盘上找不到正文。读到旧版本留下的明文草稿
//! 时照常读出来，并就地改写成加密格式。「另存为本地文件」导出的 `edit-recovered/` 是用户主动要的文件，
//! 仍是明文（界面上会说明）。别的用户 / 别的电脑保存的草稿解不开：报清楚的原因，文件原样保留。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zssh_core::sftp::edit::sanitize_local_name;
use zssh_core::sftp::path::{file_name, normalize};
use zssh_core::store::secrets;

use super::contract::{MAX_BYTES, RemoteVersion};

/// 草稿目录：和 `ui-settings.json` 一起放在数据目录下。
pub(super) const DRAFT_DIR: &str = "edit-drafts";
/// 用户把旧版本草稿「另存为本地文件」时落的目录（同样在数据目录下，只增不覆盖）。
pub(super) const RECOVERED_DIR: &str = "edit-recovered";
/// 草稿文件上限：编辑上限的文本再加 JSON 转义余量；再大就不会是正常草稿，直接删掉。
pub(super) const DRAFT_FILE_LIMIT: u64 = MAX_BYTES as u64 * 2;
/// 加密格式比明文 JSON 多出来的余量（标记 + DPAPI 的头和校验），算上限时要留着。
const SEALED_OVERHEAD: u64 = 4096;
/// 加密草稿文件的开头标记。明文草稿是 JSON，以 `{` 开头，不会撞上。
const SEALED_MAGIC: &[u8; 8] = b"ZTDRAFT\x01";

/// 草稿读写与恢复列表的界面文案。
#[derive(Clone, Copy, Debug)]
enum DraftsMsg {
    ExportPlaintextNote,
    LockedPrefix,
    SealFailedPrefix,
    TooManyLockedFiles,
    TooManyExports,
    JustNow,
    MinutesAgoSuffix,
    HoursAgoSuffix,
    DaysAgoSuffix,
}

impl crate::i18n::Msg for DraftsMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::LockedPrefix if !cfg!(windows) => {
                "本地有一份草稿无法解密（可能是另一个用户或另一台电脑保存的、本机的资料库密钥不可用，或文件已损坏）："
            }
            Self::LockedPrefix => {
                "本地有一份草稿无法解密（可能是另一个 Windows 用户或另一台电脑保存的，或文件已损坏）："
            }
            Self::SealFailedPrefix => "草稿加密失败：",
            Self::TooManyLockedFiles => "同名的无法解密草稿太多",
            Self::TooManyExports => "同名的导出文件太多",
            Self::JustNow => "刚刚",
            Self::MinutesAgoSuffix => " 分钟前",
            Self::HoursAgoSuffix => " 小时前",
            Self::DaysAgoSuffix => " 天前",
            Self::ExportPlaintextNote => "这是明文文件，没有加密，用完请自行删除",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::LockedPrefix if !cfg!(windows) => {
                "A local draft cannot be decrypted (it may belong to another user or another computer, this computer's data key may be unavailable, or the file is damaged): "
            }
            Self::LockedPrefix => {
                "A local draft cannot be decrypted (it may belong to another Windows user or another computer, or the file is damaged): "
            }
            Self::SealFailedPrefix => "Could not encrypt the draft: ",
            Self::TooManyLockedFiles => "Too many undecryptable drafts with the same name",
            Self::TooManyExports => "Too many exported files with the same name",
            Self::JustNow => "just now",
            Self::MinutesAgoSuffix => " minutes ago",
            Self::HoursAgoSuffix => " hours ago",
            Self::DaysAgoSuffix => " days ago",
            Self::ExportPlaintextNote => {
                "This file is plain text and is not encrypted; delete it when you no longer need it"
            }
        }
    }
}

/// 草稿正文的加解密。生产里是 [`Dpapi`]；测试换成假的来模拟「换了个 Windows 用户」解不开、加密失败。
pub(super) trait DraftCipher {
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String>;
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String>;
}

/// Windows DPAPI，绑定当前登录用户（和主机密码、私钥口令用同一套）。
pub(super) struct Dpapi;

impl DraftCipher for Dpapi {
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        secrets::protect(plain).map_err(|err| crate::error_text::error_text(&err))
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        secrets::unprotect(sealed).map_err(|err| crate::error_text::error_text(&err))
    }
}

/// 一份草稿文件存在，但这个用户解不开（别的 Windows 用户 / 别的电脑保存的，或文件损坏）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftLocked {
    pub(super) file: PathBuf,
    reason: String,
}

impl std::fmt::Display for DraftLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}",
            crate::i18n::t(DraftsMsg::LockedPrefix),
            self.reason
        )
    }
}

impl std::error::Error for DraftLocked {}

/// 读一份草稿文件的结果。
pub(super) enum Loaded {
    Draft(EditDraft),
    /// 读不了 / 太大 / 内容坏了：当作没有。
    Unusable,
    Locked(DraftLocked),
}

/// 读草稿时的两个开关。
#[derive(Clone, Copy)]
pub(super) struct LoadOpts {
    /// 确定坏掉的文件（超限、坏 JSON、文本超上限）顺手删掉。
    pub(super) delete_broken: bool,
    /// 读到旧版本的明文草稿，就地改写成加密格式。
    pub(super) migrate: bool,
}

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

/// 写草稿：序列化后整份加密，再同目录临时文件 + `sync_all` + 改名，崩溃时不会留下半截文件。
/// 加密失败就不写（不退回明文）：宁可这一轮没有草稿，也不把正文落到磁盘上。
pub(super) fn write_draft(path: &Path, draft: &EditDraft) -> std::io::Result<()> {
    write_draft_with(&Dpapi, path, draft)
}

pub(super) fn write_draft_with(
    cipher: &dyn DraftCipher,
    path: &Path,
    draft: &EditDraft,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(draft)?;
    let sealed = cipher.seal(&json).map_err(|reason| {
        std::io::Error::other(format!(
            "{}{reason}",
            crate::i18n::t(DraftsMsg::SealFailedPrefix)
        ))
    })?;
    let mut bytes = Vec::with_capacity(SEALED_MAGIC.len() + sealed.len());
    bytes.extend_from_slice(SEALED_MAGIC);
    bytes.extend_from_slice(&sealed);
    write_atomically(path, &bytes)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
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
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// 解析一份草稿文件。加密格式用 `cipher` 解开；旧版本的明文 JSON 照常读，`opts.migrate` 为真时顺手改写成加密格式。
///
/// 确定坏掉的文件（超限、坏 JSON、文本超上限）在 `opts.delete_broken` 为真时删掉，免得每次打开都重试；
/// 单纯读文件失败（比如被别的程序占着）一律先留着，下次再说。**解不开的加密草稿永远不删**——
/// 它可能是另一个用户的，也可能换回原用户就能读。
pub(super) fn load_draft_file(cipher: &dyn DraftCipher, path: &Path, opts: LoadOpts) -> Loaded {
    let discard = |path: &Path| {
        if opts.delete_broken {
            remove_draft(path);
        }
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return Loaded::Unusable;
    };
    if metadata.len() > DRAFT_FILE_LIMIT + SEALED_OVERHEAD {
        discard(path);
        return Loaded::Unusable;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Loaded::Unusable;
    };
    let sealed = bytes.starts_with(SEALED_MAGIC);
    let json = if sealed {
        match cipher.open(&bytes[SEALED_MAGIC.len()..]) {
            Ok(json) => json,
            Err(reason) => {
                return Loaded::Locked(DraftLocked {
                    file: path.to_owned(),
                    reason,
                });
            }
        }
    } else {
        if metadata.len() > DRAFT_FILE_LIMIT {
            discard(path);
            return Loaded::Unusable;
        }
        bytes.clone()
    };
    let draft: EditDraft = match serde_json::from_slice(&json) {
        Ok(draft) => draft,
        Err(_) => {
            discard(path);
            return Loaded::Unusable;
        }
    };
    if draft.text.len() > MAX_BYTES {
        discard(path);
        return Loaded::Unusable;
    }
    if !sealed && opts.migrate {
        migrate_to_sealed(cipher, path, &bytes, &draft);
    }
    Loaded::Draft(draft)
}

/// 旧版本的明文草稿：改写成加密格式。改写前再确认一次文件还是刚才读到的那份（别把别处刚写的新草稿盖掉）；
/// 失败不影响读出来的结果，下次读到再试。
fn migrate_to_sealed(cipher: &dyn DraftCipher, path: &Path, original: &[u8], draft: &EditDraft) {
    match std::fs::read(path) {
        Ok(current) if current == original => {}
        _ => return,
    }
    if let Err(err) = write_draft_with(cipher, path, draft) {
        tracing::warn!(%err, path = %path.display(), "把旧的明文草稿改写成加密格式失败");
    }
}

/// 读这台主机、这个路径的草稿：主机 id 和路径都要对上，对不上的当没有（也不删别人的）。
/// 解不开返回 [`DraftLocked`]，调用方要告诉用户并把那个文件挪开（见 [`set_aside_locked`]），
/// 否则下一次自动保存会把它盖掉。
pub(super) fn read_draft(
    path: &Path,
    host_id: i64,
    remote_path: &str,
) -> Result<Option<EditDraft>, DraftLocked> {
    read_draft_with(&Dpapi, path, host_id, remote_path)
}

pub(super) fn read_draft_with(
    cipher: &dyn DraftCipher,
    path: &Path,
    host_id: i64,
    remote_path: &str,
) -> Result<Option<EditDraft>, DraftLocked> {
    let opts = LoadOpts {
        delete_broken: true,
        migrate: true,
    };
    match load_draft_file(cipher, path, opts) {
        Loaded::Draft(draft) => Ok((draft.host_id == Some(host_id)
            && normalize(&draft.path) == normalize(remote_path))
        .then_some(draft)),
        Loaded::Unusable => Ok(None),
        Loaded::Locked(locked) => Err(locked),
    }
}

/// 找同路径的旧格式草稿（没有主机 id）。不删：认不出主人，不自动恢复；明文的会改写成加密格式（内容不变）。
pub(super) fn read_legacy_draft(path: &Path, remote_path: &str) -> Option<EditDraft> {
    let opts = LoadOpts {
        delete_broken: false,
        migrate: true,
    };
    match load_draft_file(&Dpapi, path, opts) {
        Loaded::Draft(draft) => {
            (draft.host_id.is_none() && draft.path == remote_path).then_some(draft)
        }
        Loaded::Unusable => None,
        Loaded::Locked(locked) => {
            tracing::warn!(path = %locked.file.display(), "旧格式草稿无法解密，已跳过");
            None
        }
    }
}

/// 解不开的草稿挪到一边（改名，数据原样保留），让出原来的文件名给新草稿。
/// 新名字不是 `.json`，扫描恢复列表时不会再碰它。
pub(super) fn set_aside_locked(locked: &DraftLocked) -> std::io::Result<PathBuf> {
    let name = locked
        .file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("draft");
    let stamp = now_unix();
    for attempt in 0..1000u32 {
        let candidate = if attempt == 0 {
            format!("{name}.undecryptable-{stamp}")
        } else {
            format!("{name}.undecryptable-{stamp}-{attempt}")
        };
        let target = locked.file.with_file_name(candidate);
        if !target.exists() {
            std::fs::rename(&locked.file, &target)?;
            return Ok(target);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        crate::i18n::t(DraftsMsg::TooManyLockedFiles),
    ))
}

/// 导出的草稿文件是**明文**（用户主动要的、要拿去用的文件），界面上要说明：见 [`EXPORT_PLAINTEXT_NOTE`]。
pub(crate) fn export_plaintext_note() -> &'static str {
    crate::i18n::t(DraftsMsg::ExportPlaintextNote)
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
        crate::i18n::t(DraftsMsg::TooManyExports),
    ))
}

/// 删草稿：文件本来就不在不算错误（保存、丢弃、清理都会走这里）。
pub(super) fn remove_draft(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "删除编辑器草稿失败");
        }
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
    let opts = LoadOpts {
        delete_broken: false,
        migrate: false,
    };
    match load_draft_file(&Dpapi, path, opts) {
        Loaded::Draft(draft) if draft.revision > revision => {}
        // 解不开的不知道修订号，也不是我们刚保存的那一份：不删。
        Loaded::Locked(_) => {}
        _ => remove_draft(path),
    }
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
        crate::i18n::t(DraftsMsg::JustNow).to_owned()
    } else if age < 3_600 {
        crate::i18n::count(
            age / 60,
            crate::i18n::t(DraftsMsg::MinutesAgoSuffix),
            "minute ago",
            "minutes ago",
        )
    } else if age < 86_400 {
        crate::i18n::count(
            age / 3_600,
            crate::i18n::t(DraftsMsg::HoursAgoSuffix),
            "hour ago",
            "hours ago",
        )
    } else {
        crate::i18n::count(
            age / 86_400,
            crate::i18n::t(DraftsMsg::DaysAgoSuffix),
            "day ago",
            "days ago",
        )
    }
}

/// 启动时扫出来的一份草稿（恢复提示用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FoundDraft {
    /// 草稿文件。
    pub(super) file: PathBuf,
    pub(super) draft: EditDraft,
}

/// 扫描结果：能读的草稿，和这个用户解不开的加密草稿。
pub(super) struct DraftScan {
    pub(super) found: Vec<FoundDraft>,
    pub(super) locked: Vec<DraftLocked>,
}

/// 把数据目录下所有能读出来的草稿列出来，最近改的在前。
///
/// 坏的、太大的、读不了的文件一概跳过（不删——这里是「列一列」，删不删是各自的事）；
/// 旧版本的明文草稿读出来之后顺手改写成加密格式；解不开的另外报出来（文件不动）。
/// 临时文件（`*.tmp`）是写到一半的残留，不算草稿。文本为空的草稿也列（用户可能确实清空了文件）。
pub(super) fn list_drafts(base: &Path) -> Vec<FoundDraft> {
    let scan = scan_drafts_with(&Dpapi, base);
    if !scan.locked.is_empty() {
        tracing::warn!(
            count = scan.locked.len(),
            "有无法解密的编辑草稿（可能是另一个 Windows 用户或另一台电脑保存的），已保留、没有列出" // i18n-allow: 仅日志
        );
    }
    scan.found
}

pub(super) fn scan_drafts_with(cipher: &dyn DraftCipher, base: &Path) -> DraftScan {
    let mut scan = DraftScan {
        found: Vec::new(),
        locked: Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir(draft_dir_in(base)) else {
        return scan;
    };
    let opts = LoadOpts {
        delete_broken: false,
        migrate: true,
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    for file in files {
        match load_draft_file(cipher, &file, opts) {
            Loaded::Draft(draft) => scan.found.push(FoundDraft { file, draft }),
            Loaded::Locked(locked) => scan.locked.push(locked),
            Loaded::Unusable => {}
        }
    }
    scan.found.sort_by(|a, b| {
        b.draft
            .saved_at_unix
            .cmp(&a.draft.saved_at_unix)
            .then_with(|| a.file.cmp(&b.file))
    });
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老测试按「读不出来 = None」写的，沿用这个形状；解不开的单独有测试。
    fn read_draft(path: &Path, host_id: i64, remote_path: &str) -> Option<EditDraft> {
        super::read_draft(path, host_id, remote_path).ok().flatten()
    }

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
        // S01：文件和内容都留着，只是明文改写成了加密格式（读出来的草稿不变）。
        let raw = std::fs::read(&legacy_path).expect("读回");
        assert!(raw.starts_with(SEALED_MAGIC), "旧格式明文也要改写成加密");
        assert!(!contains(&raw, "旧草稿"));
        assert_eq!(
            read_legacy_draft(&legacy_path, "/srv/app/main.rs"),
            Some(found.clone())
        );
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
        if cfg!(windows) {
            // `CON` 是 Windows 的保留设备名：不能照原样当文件名。
            assert!(!name.to_ascii_uppercase().starts_with("CON."), "{name}");
        } else {
            // POSIX 上它是个普通名字，按下载同一套规则原样保留。
            assert!(name.starts_with("CON.txt."), "{name}");
        }
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

    // ---- S01：草稿正文加密落盘 ----

    /// 模拟「换了个 Windows 用户 / 换了台电脑」：加密照旧（用 DPAPI），解密一律被拒。
    struct OtherUser;

    impl DraftCipher for OtherUser {
        fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
            Dpapi.seal(plain)
        }

        fn open(&self, _sealed: &[u8]) -> Result<Vec<u8>, String> {
            Err("拒绝访问".to_owned())
        }
    }

    /// 加密本身失败（系统加密不可用）。
    struct SealFails;

    impl DraftCipher for SealFails {
        fn seal(&self, _plain: &[u8]) -> Result<Vec<u8>, String> {
            Err("系统加密不可用".to_owned())
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
            Dpapi.open(sealed)
        }
    }

    const SECRET_BODY: &str = "db_password=hunter2-0xDEADBEEF\napi_token=tok-中文-123";

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    /// S01：新写的草稿文件里找不到正文（连远端路径也不露），读回来一字不差。
    #[test]
    fn a_new_draft_file_holds_no_plaintext() {
        let dir = temp_dir("sealed-write");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        let draft = sample_draft(SECRET_BODY);
        write_draft(&path, &draft).expect("写草稿");

        let raw = std::fs::read(&path).expect("读原始字节");
        assert!(raw.starts_with(SEALED_MAGIC), "应该是加密格式");
        assert!(!contains(&raw, "hunter2"), "磁盘上不能有正文");
        assert!(!contains(&raw, "tok-"), "磁盘上不能有正文");
        assert!(!contains(&raw, "/srv/app/main.rs"), "远端路径也不露");
        assert!(!contains(&raw, "db_password"));
        // 不留 JSON 明文的任何痕迹。
        assert!(!contains(&raw, "\"text\""));
        assert_eq!(read_draft(&path, HOST, "/srv/app/main.rs"), Some(draft));
        // 目录里没有残留的临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(draft_dir_in(&dir))
            .expect("读目录")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：旧版本留下的明文草稿照常读出来，读到之后就地改写成加密格式，再读还是同样的内容。
    #[test]
    fn a_plaintext_draft_from_an_older_version_is_read_then_rewritten_encrypted() {
        let dir = temp_dir("sealed-migrate");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        std::fs::create_dir_all(draft_dir_in(&dir)).expect("建草稿目录");
        let draft = sample_draft(SECRET_BODY);
        let json = serde_json::to_vec(&draft).expect("序列化");
        std::fs::write(&path, &json).expect("写旧版明文草稿");
        assert!(contains(&std::fs::read(&path).unwrap(), "hunter2"));

        assert_eq!(
            read_draft(&path, HOST, "/srv/app/main.rs"),
            Some(draft.clone()),
            "明文草稿要能读"
        );
        let raw = std::fs::read(&path).expect("读回");
        assert!(raw.starts_with(SEALED_MAGIC), "读到之后改写成加密格式");
        assert!(!contains(&raw, "hunter2"), "改写之后磁盘上不再有正文");
        assert_eq!(
            read_draft(&path, HOST, "/srv/app/main.rs"),
            Some(draft),
            "改写后内容不变"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：启动时的恢复扫描也会把明文草稿改写成加密格式，并且列得出来。
    #[test]
    fn the_startup_scan_rewrites_plaintext_drafts_too() {
        let dir = temp_dir("sealed-scan");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        std::fs::create_dir_all(draft_dir_in(&dir)).expect("建草稿目录");
        let draft = sample_draft(SECRET_BODY);
        std::fs::write(&path, serde_json::to_vec(&draft).unwrap()).expect("写旧版明文草稿");

        let found = list_drafts(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].draft, draft);
        let raw = std::fs::read(&path).expect("读回");
        assert!(raw.starts_with(SEALED_MAGIC));
        assert!(!contains(&raw, "hunter2"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：别的用户 / 别的电脑保存的草稿解不开：给清楚的错误，不崩，不删，不改；
    /// 恢复扫描把它单独报出来；挪到一边之后，新草稿能写在原来的位置，旧的原样还在。
    #[test]
    fn a_draft_another_user_cannot_decrypt_is_reported_clearly_and_kept() {
        let dir = temp_dir("sealed-other-user");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        write_draft(&path, &sample_draft(SECRET_BODY)).expect("写草稿");
        let before = std::fs::read(&path).expect("读原始字节");

        let error = read_draft_with(&OtherUser, &path, HOST, "/srv/app/main.rs")
            .expect_err("解不开必须是错误，不是 None 也不是崩溃");
        let message = error.to_string();
        assert!(message.contains("无法解密"), "{message}");
        let other_user = if cfg!(windows) {
            "另一个 Windows 用户"
        } else {
            "另一个用户"
        };
        assert!(message.contains(other_user), "{message}");
        assert!(!message.contains("hunter2"), "错误信息里不能带正文");
        assert_eq!(std::fs::read(&path).unwrap(), before, "文件不能被删或改");

        let scan = scan_drafts_with(&OtherUser, &dir);
        assert!(scan.found.is_empty());
        assert_eq!(scan.locked.len(), 1, "恢复扫描要把它报出来");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // 挪到一边：数据一字节不动，名字不再是 .json（扫描不会再碰它）。
        let kept = set_aside_locked(&error).expect("挪开");
        assert!(!path.exists());
        assert_eq!(std::fs::read(&kept).unwrap(), before);
        assert!(kept.extension().is_none_or(|ext| ext != "json"), "{kept:?}");
        // 原来的位置现在能写新草稿，旧的不受影响。
        write_draft(&path, &sample_draft("新的内容")).expect("写新草稿");
        assert_eq!(std::fs::read(&kept).unwrap(), before);
        assert_eq!(
            read_draft(&path, HOST, "/srv/app/main.rs").map(|draft| draft.text),
            Some("新的内容".to_owned())
        );
        // 换回原来的用户（真实 DPAPI）就能读出挪开的那份。
        let loaded = load_draft_file(
            &Dpapi,
            &kept,
            LoadOpts {
                delete_broken: false,
                migrate: false,
            },
        );
        assert!(matches!(loaded, Loaded::Draft(draft) if draft.text == SECRET_BODY));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：加密文件被改坏：也是「解不开」的清楚错误，不是崩溃，也不会被当成坏文件删掉。
    #[test]
    fn a_damaged_encrypted_draft_is_reported_and_never_deleted() {
        let dir = temp_dir("sealed-damaged");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        write_draft(&path, &sample_draft(SECRET_BODY)).expect("写草稿");
        let mut raw = std::fs::read(&path).expect("读");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        let middle = SEALED_MAGIC.len() + 40;
        raw[middle] ^= 0xff;
        std::fs::write(&path, &raw).expect("写坏");

        let error =
            super::read_draft(&path, HOST, "/srv/app/main.rs").expect_err("改坏的加密草稿读不了");
        assert!(error.to_string().contains("无法解密"));
        assert_eq!(std::fs::read(&path).unwrap(), raw, "解不开的不能删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：加密失败就不写——不退回明文，也不留临时文件。
    #[test]
    fn when_encryption_fails_nothing_is_written() {
        let dir = temp_dir("sealed-seal-fails");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        let error = write_draft_with(&SealFails, &path, &sample_draft(SECRET_BODY))
            .expect_err("加密失败要报错");
        assert!(error.to_string().contains("加密失败"), "{error}");
        assert!(!path.exists());
        let leftovers = std::fs::read_dir(draft_dir_in(&dir))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "连临时文件都不能有");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：保存成功后的清理删不掉「读不出修订号」的加密草稿（可能是别人的），也不会因此崩。
    #[test]
    fn cleanup_after_a_save_leaves_a_draft_it_cannot_decrypt() {
        let dir = temp_dir("sealed-cleanup");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        std::fs::create_dir_all(draft_dir_in(&dir)).expect("建草稿目录");
        let mut foreign = SEALED_MAGIC.to_vec();
        foreign.extend_from_slice(b"not-a-valid-dpapi-blob");
        std::fs::write(&path, &foreign).expect("写别人的草稿");
        remove_draft_up_to(&path, 5);
        assert_eq!(std::fs::read(&path).unwrap(), foreign);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S01：加密格式比明文 JSON 稍大；贴着上限、全是要转义的字符的草稿也不能被当成"太大"删掉。
    #[test]
    fn a_maximum_size_draft_with_heavy_escaping_still_reads_back() {
        let dir = temp_dir("sealed-max");
        let path = draft_file_in(&dir, HOST, "/srv/app/main.rs");
        let draft = sample_draft(&"\n".repeat(MAX_BYTES));
        write_draft(&path, &draft).expect("写草稿");
        assert_eq!(read_draft(&path, HOST, "/srv/app/main.rs"), Some(draft));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
