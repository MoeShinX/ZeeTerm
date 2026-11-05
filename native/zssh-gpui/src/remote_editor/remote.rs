//! 远端读写：读文件、保存（带版本核对和进程级写闸）、冲突处理、另存为、外部修改监视。
//!
//! 保存的原子性不在这里实现，全交给核心的 [`EditBackend::overwrite`]（同目录临时文件 + 核对大小 + 改名，
//! 保留权限 / 属主，目标是符号链接时写到真实文件，服务器没有 `posix-rename` 时核心自己退回先删后改名）。
//! 这一层只管三件事：
//! - 保存前重读一遍当前内容比版本，远端被别人改过（或删了）就**一次都不写**；
//! - 同一个服务器上的同一个文件，进程内一次只允许一个保存在「比版本 → 写」这一段里（[`write_gate`]）：
//!   两个编辑器同时拿着同一个旧版本保存，后到的一个会看到先到的写入、报冲突，不会互相覆盖；
//! - 把各种结果整理成界面要的 [`SaveOutcome`]。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use zssh_core::sftp::SftpChannel;
use zssh_core::sftp::SftpError;
use zssh_core::sftp::edit::{
    EditBackend, OverwriteError, OwnerMode, sibling_remote_path, validate_remote_name,
};
use zssh_core::sftp::path::{is_absolute, join, normalize, parent};

use super::RemoteEditEvent;
use super::contract::{MAX_BYTES, Opened, RemoteVersion, open_bytes, too_large, version_of};
use crate::error_text::error_text_in;
use crate::i18n::{Lang, current};
use crate::ssh_backend::{factory_for_host, runtime};

/// 远端编辑的错误文案。
#[derive(Clone, Copy, Debug)]
enum RemoteMsg {
    OpenFailedPrefix,
    OpenFailedMid,
    OpenFailedClose,
    NotRegular,
    ReadFailedPrefix,
    ChannelFailedPrefix,
    TooLargePrefix,
    TooLargeMid,
    TooLargeSuffix,
    SaveFailedPrefix,
    ReadRemoteFailedPrefix,
    ExistsSuffix,
    CheckFailedPrefix,
    SaveAsFailedPrefix,
    SaveAsExists,
    SaveAsEmpty,
    ByteSuffix,
}

impl crate::i18n::Msg for RemoteMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::OpenFailedPrefix => "打开失败：",
            Self::OpenFailedMid => "（",
            Self::OpenFailedClose => "）",
            Self::NotRegular => "这不是普通文件，不能编辑",
            Self::ReadFailedPrefix => "读取失败：",
            Self::ChannelFailedPrefix => "开 SFTP 通道失败：",
            Self::TooLargePrefix => "内容太大（",
            Self::TooLargeMid => "，上限 ",
            Self::TooLargeSuffix => "），拒绝保存",
            Self::SaveFailedPrefix => "保存失败：",
            Self::ReadRemoteFailedPrefix => "保存失败：读取远端当前内容失败：",
            Self::ExistsSuffix => " 已经存在，换个名字",
            Self::CheckFailedPrefix => "检查目标失败：",
            Self::SaveAsFailedPrefix => "另存为失败：",
            Self::SaveAsExists => "目标已存在，另存为不会覆盖",
            Self::SaveAsEmpty => "另存为路径不能为空",
            Self::ByteSuffix => " 字节",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::OpenFailedPrefix => "Could not open: ",
            Self::OpenFailedMid => " (",
            Self::OpenFailedClose => ")",
            Self::NotRegular => "This is not a regular file and cannot be edited",
            Self::ReadFailedPrefix => "Read failed: ",
            Self::ChannelFailedPrefix => "Could not open the SFTP channel: ",
            Self::TooLargePrefix => "The content is too large (",
            Self::TooLargeMid => ", limit ",
            Self::TooLargeSuffix => "); saving refused",
            Self::SaveFailedPrefix => "Save failed: ",
            Self::ReadRemoteFailedPrefix => {
                "Save failed: could not read the current remote content: "
            }
            Self::ExistsSuffix => " already exists — choose another name",
            Self::CheckFailedPrefix => "Could not check the target: ",
            Self::SaveAsFailedPrefix => "Save as failed: ",
            Self::SaveAsExists => "The target already exists; Save as does not overwrite",
            Self::SaveAsEmpty => "The save-as path cannot be empty",
            Self::ByteSuffix => "bytes",
        }
    }
}

/// 外部修改轮询间隔：只发一次 `stat`，不读内容；2 秒一次够用，也不会压服务器。
pub(super) const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// 远端文件的轻量标记：只用来发现「别人动过」，不替代内容版本。
///
/// 只取 `stat` 里最便宜的几个字段：大小和秒级 mtime 已经能覆盖绝大多数写入；
/// uid/gid 在文件被换属主或整体覆盖时再补一层。内容是否真的变了仍由保存时的
/// 整份哈希判断，这里只负责提醒。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RemoteStamp {
    pub(super) size: Option<u64>,
    pub(super) mtime: Option<u32>,
    pub(super) uid: Option<u32>,
    pub(super) gid: Option<u32>,
}

/// 界面和后台监视共享的状态：加载/保存成功时更新标记，保存期间让监视器闭嘴。
#[derive(Default)]
pub(super) struct RemoteWatch {
    stamp: Mutex<Option<RemoteStamp>>,
    saving: AtomicBool,
    stop: AtomicBool,
}

impl RemoteWatch {
    pub(super) fn stamp(&self) -> Option<RemoteStamp> {
        self.stamp.lock().ok().and_then(|guard| *guard)
    }

    pub(super) fn set_stamp(&self, stamp: Option<RemoteStamp>) {
        if let Ok(mut guard) = self.stamp.lock() {
            *guard = stamp;
        }
    }

    pub(super) fn saving(&self) -> bool {
        self.saving.load(Ordering::SeqCst)
    }

    pub(super) fn set_saving(&self, saving: bool) {
        self.saving.store(saving, Ordering::SeqCst);
    }

    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(super) fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// 这次 `stat` 结果要不要提醒用户：没有基线、正在保存、和基线一致、或者同一个
/// 标记已经报过，都不重复报。保存失败后基线保持旧值，下一轮会重新提醒。
pub(super) fn should_report_change(
    current: &RemoteStamp,
    baseline: Option<&RemoteStamp>,
    last_reported: Option<&RemoteStamp>,
    saving: bool,
) -> bool {
    if saving {
        return false;
    }
    let Some(baseline) = baseline else {
        return false;
    };
    current != baseline && last_reported != Some(current)
}

/// 一次成功读取的结果。
pub(super) struct Loaded {
    pub(super) opened: Opened,
    /// 读取时顺带记下的远端标记；`stat` 失败时为 `None`，只影响外部修改提醒。
    pub(super) stamp: Option<RemoteStamp>,
}

/// 读取失败：给用户看的话，加上"是不是连接没了"（界面据此出「重新连接」）。
///
/// 断线与否在拿到 `SftpError` 的那一刻就判断（[`SftpError::is_disconnect`]），不靠事后在已经
/// 翻译成当前语言的文案里找关键词——文案换语言就认不出了。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LoadFailure {
    pub(super) message: String,
    pub(super) disconnected: bool,
}

impl LoadFailure {
    /// 不是断线的失败（太大、不是普通文件、内容格式不对……）。
    fn other(message: String) -> Self {
        Self {
            message,
            disconnected: false,
        }
    }

    /// 来自 SFTP 错误的失败：`prefix` + 指定语言的错误文案 + `suffix`。
    fn from_sftp(lang: Lang, prefix: &str, err: &SftpError, suffix: &str) -> Self {
        Self {
            message: format!("{prefix}{}{suffix}", error_text_in(lang, err)),
            disconnected: err.is_disconnect(),
        }
    }
}

/// 读远端字节：不是普通文件、超过上限都在这里拒绝（报的是人话）。
pub(super) async fn fetch_bytes<B: EditBackend + ?Sized>(
    backend: &B,
    path: &str,
) -> Result<Vec<u8>, String> {
    fetch_classified(backend, path)
        .await
        .map_err(|failure| failure.message)
}

/// 同 [`fetch_bytes`]，失败时带着"是不是断线"的判断。
async fn fetch_classified<B: EditBackend + ?Sized>(
    backend: &B,
    path: &str,
) -> Result<Vec<u8>, LoadFailure> {
    let lang = current();
    let (snapshot, is_dir) = backend.stat(path).await.map_err(|err| {
        let prefix = format!(
            "{}{path}{}",
            crate::i18n::t(RemoteMsg::OpenFailedPrefix),
            crate::i18n::t(RemoteMsg::OpenFailedMid),
        );
        LoadFailure::from_sftp(
            lang,
            &prefix,
            &err,
            crate::i18n::t(RemoteMsg::OpenFailedClose),
        )
    })?;
    if is_dir {
        return Err(LoadFailure::other(
            crate::i18n::t(RemoteMsg::NotRegular).to_owned(),
        ));
    }
    if snapshot.size > MAX_BYTES as u64 {
        return Err(LoadFailure::other(too_large(snapshot.size as usize)));
    }
    backend
        .read_all(path, MAX_BYTES as u64)
        .await
        .map_err(|err| match err {
            SftpError::TooLarge { size, .. } => LoadFailure::other(too_large(size as usize)),
            other => LoadFailure::from_sftp(
                lang,
                crate::i18n::t(RemoteMsg::ReadFailedPrefix),
                &other,
                "",
            ),
        })
}

/// 读远端文本：大小、二进制、编码、换行都按内置编辑器的契约检查。
pub(super) async fn load_remote(channel: &SftpChannel, path: &str) -> Result<Loaded, LoadFailure> {
    let bytes = fetch_classified(channel, path).await?;
    let opened = open_bytes(&bytes).map_err(LoadFailure::other)?;
    // `stat` 失败不影响打开：只是这次没有基线，外部修改提醒暂时不启用。
    let stamp = read_stamp(channel, path).await.ok();
    Ok(Loaded { opened, stamp })
}

/// 读远端标记：`stat` 跟随链接；悬空链接再 `lstat` 一次（和 `target_exists` 一致）。
pub(super) async fn read_stamp(
    channel: &SftpChannel,
    path: &str,
) -> Result<RemoteStamp, SftpError> {
    let attrs = match channel.stat(path).await {
        Ok(attrs) => attrs,
        Err(SftpError::NotFound { .. }) => channel.lstat(path).await?,
        Err(err) => return Err(err),
    };
    Ok(RemoteStamp {
        size: attrs.size,
        mtime: attrs.mtime,
        uid: attrs.uid,
        gid: attrs.gid,
    })
}

/// 开一条新的 SFTP 通道（连接池里有同配置的活连接就复用，没有就重新握手）：
/// 编辑器从恢复提示打开时、断线后「重新连接」时用。
pub(crate) async fn dial_channel(host_id: i64) -> Result<Arc<SftpChannel>, String> {
    let factory = factory_for_host(host_id).await?;
    let channel = SftpChannel::open(&factory.connection())
        .await
        .map_err(|err| format!("{}{err}", crate::i18n::t(RemoteMsg::ChannelFailedPrefix)))?;
    Ok(Arc::new(channel))
}

// ---------------------------------------------------------------------------
// 保存
// ---------------------------------------------------------------------------

/// 冲突时远端现在的样子（给冲突提示用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RemoteGlimpse {
    /// 远端文件被删了。
    pub(super) gone: bool,
    /// 远端现在的字节数（读不到时 `None`）。
    pub(super) size: Option<u64>,
}

/// 保存的结果，还没转成界面事件（读回写入后的属性是调用方的事，这里只管远端内容）。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SaveOutcome {
    Saved {
        version: RemoteVersion,
        size: usize,
        owner_changed: Option<(String, String)>,
    },
    /// 打开之后远端被改过（或删了）：一次都没写。
    Conflict(RemoteGlimpse),
    /// 属主保持不了，**什么都没替换**。
    OwnerWouldChange {
        from: String,
        to: String,
    },
    Failed(String),
    /// 连接没了（保存没有发生）：界面给「重新连接后再保存」，不当成普通失败。
    Disconnected(String),
}

/// 进程级的「同一服务器同一文件一次一个保存」闸。键 = 服务器身份 + 真实路径（符号链接解析后）。
///
/// 核心的 `overwrite` 自己也会拿远端目标锁，但那把锁只管「写」这一小段，管不到前面的
/// 「重读比版本」：两个编辑器可以同时读到同一个旧版本、同时通过检查，然后先后覆盖。
/// 这个闸把「比版本 + 写」整段罩住，后到的一个读到的就是先到的写入。
async fn write_gate<B: EditBackend + ?Sized>(
    backend: &B,
    path: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let real = backend
        .real_path(path)
        .await
        .unwrap_or_else(|| path.to_owned());
    let key = format!(
        "{}\0{}",
        backend.server_identity().unwrap_or_default(),
        normalize(&real)
    );
    let gate = {
        let mut map = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 没人用的闸顺手清掉，免得编辑过很多文件之后一直占着。
        if map.len() > 256 {
            map.retain(|_, gate| Arc::strong_count(gate) > 1);
        }
        Arc::clone(map.entry(key).or_default())
    };
    gate.lock_owned().await
}

/// 保存：重读当前内容 → 比版本 → 一致才交给核心的 [`EditBackend::overwrite`]。
///
/// `expected` 为 `None` 是用户在冲突提示里选了「覆盖远端」：不比版本，直接写（远端被删了也照写，等于重建）。
/// 任何一步失败，远端原文件还是完整的旧内容（核心已经测过这一点）；这里不自己动远端文件，
/// 也就不会再有「截断到一半」的状态。
pub(super) async fn save_remote<B: EditBackend + ?Sized>(
    backend: &B,
    path: &str,
    bytes: &[u8],
    expected: Option<RemoteVersion>,
    owner: OwnerMode,
) -> SaveOutcome {
    if bytes.len() > MAX_BYTES {
        return SaveOutcome::Failed(format!(
            "{}{}{}{}{}",
            crate::i18n::t(RemoteMsg::TooLargePrefix),
            crate::i18n::count(
                bytes.len() as u64,
                crate::i18n::t(RemoteMsg::ByteSuffix),
                "byte",
                "bytes"
            ),
            crate::i18n::t(RemoteMsg::TooLargeMid),
            crate::i18n::count(
                MAX_BYTES as u64,
                crate::i18n::t(RemoteMsg::ByteSuffix),
                "byte",
                "bytes"
            ),
            crate::i18n::t(RemoteMsg::TooLargeSuffix)
        ));
    }
    let _gate = write_gate(backend, path).await;
    if let Some(expected) = expected {
        match backend.read_all(path, MAX_BYTES as u64).await {
            Ok(current) => {
                if version_of(&current) != expected {
                    return SaveOutcome::Conflict(RemoteGlimpse {
                        gone: false,
                        size: Some(current.len() as u64),
                    });
                }
            }
            // 远端涨过上限时不可能是打开时那份内容（那份在上限以内），按冲突处理。
            Err(SftpError::TooLarge { size, .. }) => {
                return SaveOutcome::Conflict(RemoteGlimpse {
                    gone: false,
                    size: Some(size),
                });
            }
            Err(SftpError::NotFound { .. }) => {
                return SaveOutcome::Conflict(RemoteGlimpse {
                    gone: true,
                    size: None,
                });
            }
            Err(err) => {
                return SaveOutcome::Failed(format!(
                    "{}{err}",
                    crate::i18n::t(RemoteMsg::ReadRemoteFailedPrefix)
                ));
            }
        }
    }
    match backend.overwrite(path, bytes, owner).await {
        Ok(report) => SaveOutcome::Saved {
            version: version_of(bytes),
            size: bytes.len(),
            owner_changed: report.owner_change.map(|change| (change.from, change.to)),
        },
        Err(OverwriteError::OwnerWouldChange(change)) => SaveOutcome::OwnerWouldChange {
            from: change.from,
            to: change.to,
        },
        Err(OverwriteError::Failed(err)) => {
            let message = format!("{}{err}", crate::i18n::t(RemoteMsg::SaveFailedPrefix));
            if matches!(err, SftpError::Disconnected(_)) {
                SaveOutcome::Disconnected(message)
            } else {
                SaveOutcome::Failed(message)
            }
        }
    }
}

/// 在真实通道上保存并转成界面事件。写成功后重新 `stat` 一次，把基线挪到自己这次写入的结果上，
/// 避免监视器下一轮把这次保存当成外部修改；`stat` 失败就清空基线，宁可不提醒也不误报。
pub(super) async fn save_event(
    channel: &SftpChannel,
    path: &str,
    bytes: &[u8],
    expected: Option<RemoteVersion>,
    owner: OwnerMode,
    save_id: u64,
) -> RemoteEditEvent {
    match save_remote(channel, path, bytes, expected, owner).await {
        SaveOutcome::Saved {
            version,
            size,
            owner_changed,
        } => RemoteEditEvent::Saved {
            save_id,
            version,
            size,
            stamp: read_stamp(channel, path).await.ok(),
            owner_changed,
        },
        SaveOutcome::Conflict(remote) => RemoteEditEvent::Conflict { save_id, remote },
        SaveOutcome::OwnerWouldChange { from, to } => {
            RemoteEditEvent::OwnerWouldChange { save_id, from, to }
        }
        SaveOutcome::Failed(message) => RemoteEditEvent::SaveFailed {
            save_id,
            message,
            disconnected: false,
        },
        SaveOutcome::Disconnected(message) => RemoteEditEvent::SaveFailed {
            save_id,
            message,
            disconnected: true,
        },
    }
}

// ---------------------------------------------------------------------------
// 冲突处理
// ---------------------------------------------------------------------------

/// 冲突提示里的「另存为」：同目录下用新名字存一份，之后这个编辑器改跟踪新文件。
/// 返回新文件的完整路径；目标已存在就拒绝（绝不覆盖别人的文件）。
pub(super) async fn save_beside<B: EditBackend + ?Sized>(
    backend: &B,
    original: &str,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    validate_remote_name(name).map_err(|reason| crate::error_text::reason_text(&reason))?;
    let target = sibling_remote_path(original, name.trim());
    let _gate = write_gate(backend, &target).await;
    match backend.stat(&target).await {
        Ok(_) => {
            return Err(format!(
                "{target}{}",
                crate::i18n::t(RemoteMsg::ExistsSuffix)
            ));
        }
        Err(SftpError::NotFound { .. }) => {}
        Err(err) => {
            return Err(format!(
                "{}{err}",
                crate::i18n::t(RemoteMsg::CheckFailedPrefix)
            ));
        }
    }
    backend
        .write_all(&target, bytes)
        .await
        .map_err(|err| format!("{}{err}", crate::i18n::t(RemoteMsg::SaveAsFailedPrefix)))?;
    Ok(target)
}

/// 另存为（副本）：先确认目标不存在，再用核心的「临时文件 + 改名」新建；不改当前文件和版本。
pub(super) async fn save_as_remote(
    channel: &SftpChannel,
    path: &str,
    bytes: &[u8],
) -> RemoteEditEvent {
    match target_exists(channel, path).await {
        Ok(true) => {
            return RemoteEditEvent::SaveAsFailed(
                crate::i18n::t(RemoteMsg::SaveAsExists).to_owned(),
            );
        }
        Ok(false) => {}
        Err(message) => {
            return RemoteEditEvent::SaveAsFailed(format!(
                "{}{message}",
                crate::i18n::t(RemoteMsg::SaveAsFailedPrefix)
            ));
        }
    }
    // 目标原来不存在：核心在改名前会再确认一次它没有被别人刚刚建出来，建出来了就放弃。
    match channel.write_all(path, bytes).await {
        Ok(()) => RemoteEditEvent::SavedAs {
            path: path.to_owned(),
        },
        Err(err) => RemoteEditEvent::SaveAsFailed(format!(
            "{}{err}",
            crate::i18n::t(RemoteMsg::SaveAsFailedPrefix)
        )),
    }
}

/// 目标名字是不是已经被占用：`stat` 跟随链接，悬空链接再由 `lstat` 兜底。
async fn target_exists(channel: &SftpChannel, path: &str) -> Result<bool, String> {
    match channel.stat(path).await {
        Ok(_) => Ok(true),
        // 悬空符号链接：`stat` 跟随之后报不存在，但名字本身已经被占了。
        Err(SftpError::NotFound { .. }) => match channel.lstat(path).await {
            Ok(_) => Ok(true),
            Err(SftpError::NotFound { .. }) => Ok(false),
            Err(err) => Err(format!(
                "{}{err}",
                crate::i18n::t(RemoteMsg::CheckFailedPrefix)
            )),
        },
        Err(err) => Err(format!(
            "{}{err}",
            crate::i18n::t(RemoteMsg::CheckFailedPrefix)
        )),
    }
}

/// 另存为的目标路径：空串报错；相对路径按原文件所在目录拼，绝对路径只规范化。
///
/// 只是文本处理，不查服务器：`..` 会不会穿过符号链接由服务器自己解释。
pub(super) fn resolve_save_as(original: &str, typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err(crate::i18n::t(RemoteMsg::SaveAsEmpty).to_owned());
    }
    if is_absolute(typed) {
        return Ok(normalize(typed));
    }
    Ok(normalize(&join(&parent(original), typed)))
}

// ---------------------------------------------------------------------------
// 外部修改监视
// ---------------------------------------------------------------------------

/// 外部修改监视：每 2 秒 `stat` 一次同一个路径，只比大小 / mtime / 属主，不读内容。
///
/// 通道用 `Weak`：标签关掉后 `RemoteEditor` 里的 `Arc` 被清空，下一轮升级失败就退出。
/// 停止信号由 `RemoteWatch::stop` 提供，最多多跑一个轮询间隔。
pub(super) fn start_watcher(
    channel: Weak<SftpChannel>,
    path: String,
    sender: Sender<RemoteEditEvent>,
    watch: Arc<RemoteWatch>,
) {
    let Ok(runtime) = runtime() else {
        // runtime 起不来时读取本身会报错；监视器没有也无所谓。
        return;
    };
    runtime.spawn(async move {
        let mut last_reported: Option<RemoteStamp> = None;
        let mut seen_baseline: Option<RemoteStamp> = None;
        loop {
            tokio::time::sleep(WATCH_INTERVAL).await;
            if watch.stopped() {
                break;
            }
            let baseline = watch.stamp();
            // 加载成功或保存成功后基线会变：清掉「已经报过」，让新内容还能再报。
            if baseline != seen_baseline {
                last_reported = None;
                seen_baseline = baseline;
            }
            let Some(baseline) = baseline else {
                continue;
            };
            if watch.saving() {
                continue;
            }
            let Some(channel) = channel.upgrade() else {
                break;
            };
            let current = match read_stamp(&channel, &path).await {
                Ok(stamp) => stamp,
                // 没权限、连接抖动、文件被删掉：这一轮不下结论，下一轮再看。
                Err(_) => continue,
            };
            if watch.stopped() {
                break;
            }
            // 远端回到基线（比如外部改动被撤销）：允许同一个标记下次再报。
            if current == baseline {
                last_reported = None;
                continue;
            }
            if !should_report_change(
                &current,
                Some(&baseline),
                last_reported.as_ref(),
                watch.saving(),
            ) {
                continue;
            }
            if sender.send(RemoteEditEvent::ExternalChange).is_err() {
                break;
            }
            last_reported = Some(current);
        }
    });
}

#[cfg(test)]
pub(super) mod testing {
    //! 内存里的远端：只有文件，没有目录结构，保存走和生产一样的 `upload_via_fs`。
    //! 编辑器、恢复、外部编辑的测试共用。

    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use zssh_core::ai::files::{FsEntry, FsKind, FsStat, RemoteFs, RenameFailure};
    use zssh_core::sftp::SftpError;
    use zssh_core::sftp::edit::{
        EditBackend, OverwriteError, OverwriteReport, OwnerMode, RemoteSnapshot,
    };
    use zssh_core::sftp::error::ReplaceState;
    use zssh_core::ssh::BoxFuture;

    /// 用来模拟「临时文件已经写好、提交那一步断了」的 `fail_rename`，以及慢服务器的 `read_delay`。
    #[derive(Default)]
    pub(in crate::remote_editor) struct MemFs {
        pub(in crate::remote_editor) files: Mutex<HashMap<String, Vec<u8>>>,
        pub(in crate::remote_editor) fail_rename: Mutex<bool>,
        pub(in crate::remote_editor) ops: Mutex<Vec<String>>,
        pub(in crate::remote_editor) read_delay: Mutex<Duration>,
        /// 属主保不住：覆盖时 `set_owner` 报错（普通用户没法把属主设回服务账号）。
        pub(in crate::remote_editor) deny_owner: Mutex<bool>,
        /// 文件的属主 uid（没记的算 1000）。`deny_owner` 打开时，新写出的文件属主是 0（登录用户自己）。
        pub(in crate::remote_editor) uids: Mutex<HashMap<String, u32>>,
    }

    impl MemFs {
        pub(in crate::remote_editor) fn with(path: &str, data: &[u8]) -> Self {
            let fs = Self::default();
            fs.files
                .lock()
                .expect("锁")
                .insert(path.to_owned(), data.to_vec());
            fs
        }

        pub(in crate::remote_editor) fn get(&self, path: &str) -> Option<Vec<u8>> {
            self.files.lock().expect("锁").get(path).cloned()
        }

        pub(in crate::remote_editor) fn put(&self, path: &str, data: &[u8]) {
            self.files
                .lock()
                .expect("锁")
                .insert(path.to_owned(), data.to_vec());
        }

        pub(in crate::remote_editor) fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = self.files.lock().expect("锁").keys().cloned().collect();
            names.sort();
            names
        }
    }

    impl RemoteFs for MemFs {
        fn home<'a>(&'a self) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Ok("/".to_owned()) })
        }
        fn stat<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Option<FsStat>, String>> {
            Box::pin(async move {
                Ok(self.get(path).map(|data| FsStat {
                    kind: FsKind::File,
                    size: data.len() as u64,
                    mtime: None,
                    mode: Some(0o644),
                    uid: Some(
                        self.uids
                            .lock()
                            .expect("锁")
                            .get(path)
                            .copied()
                            .unwrap_or(1000),
                    ),
                    gid: Some(1000),
                }))
            })
        }
        fn realpath<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move { Ok(path.to_owned()) })
        }
        fn list<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, Result<Vec<FsEntry>, String>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn read_range<'a>(
            &'a self,
            path: &'a str,
            offset: u64,
            len: usize,
        ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            Box::pin(async move {
                let data = self.get(path).ok_or_else(|| "不存在".to_owned())?;
                let start = (offset as usize).min(data.len());
                let end = (start + len).min(data.len());
                Ok(data[start..end].to_vec())
            })
        }
        fn read_all<'a>(
            &'a self,
            path: &'a str,
            _limit: u64,
        ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            Box::pin(async move { self.get(path).ok_or_else(|| "不存在".to_owned()) })
        }
        fn write_file<'a>(
            &'a self,
            path: &'a str,
            data: &'a [u8],
            _mode: Option<u32>,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.ops.lock().expect("锁").push(format!("write {path}"));
                self.put(path, data);
                if *self.deny_owner.lock().expect("锁") {
                    self.uids.lock().expect("锁").insert(path.to_owned(), 0);
                }
                Ok(())
            })
        }
        fn rename_over<'a>(
            &'a self,
            from: &'a str,
            to: &'a str,
        ) -> BoxFuture<'a, Result<(), RenameFailure>> {
            Box::pin(async move {
                if *self.fail_rename.lock().expect("锁") {
                    return Err(RenameFailure {
                        state: ReplaceState::Untouched,
                        message: "连接断了".to_owned(),
                    });
                }
                self.ops
                    .lock()
                    .expect("锁")
                    .push(format!("rename {from} -> {to}"));
                let mut files = self.files.lock().expect("锁");
                let data = files.remove(from).ok_or_else(|| RenameFailure {
                    state: ReplaceState::Untouched,
                    message: "临时文件不见了".to_owned(),
                })?;
                let mut uids = self.uids.lock().expect("锁");
                if let Some(uid) = uids.remove(from) {
                    uids.insert(to.to_owned(), uid);
                }
                files.insert(to.to_owned(), data);
                Ok(())
            })
        }
        fn remove_file<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.files.lock().expect("锁").remove(path);
                Ok(())
            })
        }
        fn set_owner<'a>(
            &'a self,
            _path: &'a str,
            _uid: Option<u32>,
            _gid: Option<u32>,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if *self.deny_owner.lock().expect("锁") {
                    Err("Operation not permitted".to_owned())
                } else {
                    Ok(())
                }
            })
        }
    }

    /// 把内存远端接成 `EditBackend`，覆盖走和生产一样的 `upload_via_fs`。
    pub(in crate::remote_editor) struct MemBackend(pub(in crate::remote_editor) MemFs);

    impl EditBackend for MemBackend {
        fn stat<'a>(
            &'a self,
            path: &'a str,
        ) -> BoxFuture<'a, Result<(RemoteSnapshot, bool), SftpError>> {
            Box::pin(async move {
                match self.0.get(path) {
                    Some(data) => Ok((
                        RemoteSnapshot {
                            size: data.len() as u64,
                            mtime: None,
                        },
                        false,
                    )),
                    None => Err(SftpError::NotFound {
                        path: path.to_owned(),
                    }),
                }
            })
        }
        fn read_all<'a>(
            &'a self,
            path: &'a str,
            limit: u64,
        ) -> BoxFuture<'a, Result<Vec<u8>, SftpError>> {
            Box::pin(async move {
                let delay = *self.0.read_delay.lock().expect("锁");
                let data = self.0.get(path).ok_or_else(|| SftpError::NotFound {
                    path: path.to_owned(),
                })?;
                // 慢服务器：读到内容之后才「传输」，这时另一个保存可能已经写完了。
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                if data.len() as u64 > limit {
                    return Err(SftpError::TooLarge {
                        path: path.to_owned(),
                        size: data.len() as u64,
                        limit,
                    });
                }
                Ok(data)
            })
        }
        fn write_all<'a>(
            &'a self,
            path: &'a str,
            data: &'a [u8],
        ) -> BoxFuture<'a, Result<(), SftpError>> {
            Box::pin(async move {
                self.0.put(path, data);
                Ok(())
            })
        }
        fn overwrite<'a>(
            &'a self,
            path: &'a str,
            data: &'a [u8],
            mode: OwnerMode,
        ) -> BoxFuture<'a, Result<OverwriteReport, OverwriteError>> {
            Box::pin(async move {
                zssh_core::sftp::edit::upload_via_fs(&self.0, path, data, "abc123def456", mode)
                    .await
            })
        }
        fn server_identity(&self) -> Option<String> {
            Some("mem-server".to_owned())
        }
    }

    pub(in crate::remote_editor) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{MemBackend, MemFs, block_on};
    use super::*;
    use zssh_core::sftp::edit::OwnerMode;

    fn stamp(size: u64, mtime: u32) -> RemoteStamp {
        RemoteStamp {
            size: Some(size),
            mtime: Some(mtime),
            uid: Some(1000),
            gid: Some(1000),
        }
    }

    #[test]
    fn stamp_comparison_ignores_unchanged_size_and_mtime() {
        let before = stamp(12, 1_700_000_000);
        let after = stamp(12, 1_700_000_000);
        assert_eq!(before, after);
        assert!(!should_report_change(&after, Some(&before), None, false));
    }

    #[test]
    fn stamp_comparison_detects_size_or_mtime_changes() {
        let before = stamp(12, 1_700_000_000);
        assert_ne!(before, stamp(13, 1_700_000_000));
        assert_ne!(before, stamp(12, 1_700_000_001));
        assert!(should_report_change(
            &stamp(13, 1_700_000_000),
            Some(&before),
            None,
            false
        ));
        assert!(should_report_change(
            &stamp(12, 1_700_000_001),
            Some(&before),
            None,
            false
        ));

        // 属主变了也算外部改动：可能是别的文件覆盖了上来。
        let mut changed_owner = before;
        changed_owner.uid = Some(1001);
        assert_ne!(before, changed_owner);
    }

    #[test]
    fn external_change_is_ignored_while_saving() {
        let before = stamp(12, 100);
        let after = stamp(12, 101);
        assert!(!should_report_change(&after, Some(&before), None, true));
        assert!(should_report_change(&after, Some(&before), None, false));
    }

    #[test]
    fn external_change_needs_a_baseline_and_is_reported_once() {
        let before = stamp(12, 100);
        let after = stamp(12, 101);
        // 还没加载成功时没有基线，不能凭空提醒。
        assert!(!should_report_change(&after, None, None, false));
        // 同一个标记报过一次就不再重复。
        assert!(!should_report_change(
            &after,
            Some(&before),
            Some(&after),
            false
        ));
        // 再变一次还是新的外部改动。
        assert!(should_report_change(
            &stamp(12, 102),
            Some(&before),
            Some(&after),
            false
        ));
    }

    #[test]
    fn save_as_resolves_relative_paths_against_the_original_parent() {
        assert_eq!(
            resolve_save_as("/srv/app/notes.txt", "copy.txt").expect("相对路径"),
            "/srv/app/copy.txt"
        );
        assert_eq!(
            resolve_save_as("/srv/app/notes.txt", "./copy.txt").expect("当前目录"),
            "/srv/app/copy.txt"
        );
        assert_eq!(
            resolve_save_as("/srv/app/notes.txt", "../backup/notes.bak").expect("上一级"),
            "/srv/backup/notes.bak"
        );
        // 原文件在根目录下：父目录是 `/`，不会拼出 `//`。
        assert_eq!(
            resolve_save_as("/notes.txt", "copy.txt").expect("根目录"),
            "/copy.txt"
        );
    }

    #[test]
    fn save_as_rejects_empty_paths() {
        for typed in ["", " ", "\t\n  "] {
            let message = resolve_save_as("/srv/app/notes.txt", typed).expect_err("空路径要报错");
            assert!(message.contains("不能为空"), "{message}");
        }
    }

    #[test]
    fn save_as_keeps_absolute_paths() {
        assert_eq!(
            resolve_save_as("/srv/app/notes.txt", "/tmp/out.txt").expect("绝对路径"),
            "/tmp/out.txt"
        );
        // 绝对路径只做规范化，不会再按原文件目录拼一次。
        assert_eq!(
            resolve_save_as("/srv/app/notes.txt", "/tmp//a/../b.txt").expect("规范化"),
            "/tmp/b.txt"
        );
        assert_eq!(
            resolve_save_as("/srv/app/notes.txt", "/backup.txt").expect("根下的绝对路径"),
            "/backup.txt"
        );
    }

    fn glimpse_size(outcome: &SaveOutcome) -> Option<Option<u64>> {
        match outcome {
            SaveOutcome::Conflict(glimpse) => Some(glimpse.size),
            _ => None,
        }
    }

    #[test]
    fn save_goes_through_a_temp_file_and_rename() {
        let original = b"old text\n";
        let backend = MemBackend(MemFs::with("/srv/app/conf.txt", original));
        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/conf.txt",
            b"new text\n",
            Some(version_of(original)),
            OwnerMode::Require,
        ));
        assert_eq!(
            outcome,
            SaveOutcome::Saved {
                version: version_of(b"new text\n"),
                size: 9,
                owner_changed: None
            }
        );
        assert_eq!(
            backend.0.get("/srv/app/conf.txt").as_deref(),
            Some(&b"new text\n"[..])
        );
        let ops = backend.0.ops.lock().expect("锁").clone();
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(ops[0].starts_with("write /srv/app/") && ops[0] != "write /srv/app/conf.txt");
        assert!(ops[1].starts_with("rename ") && ops[1].ends_with("-> /srv/app/conf.txt"));
        assert_eq!(
            backend.0.names(),
            vec!["/srv/app/conf.txt".to_owned()],
            "不留临时文件"
        );
    }

    /// 卡里要求的「保存中断」：临时文件已经写好、提交（改名）那一步失败，原文件逐字节不变。
    #[test]
    fn interrupted_save_leaves_the_original_intact_and_cleans_up() {
        let original = "第一行\nsecond line\n".as_bytes();
        let backend = MemBackend(MemFs::with("/srv/app/conf.txt", original));
        *backend.0.fail_rename.lock().expect("锁") = true;

        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/conf.txt",
            "半截的新内容".as_bytes(),
            Some(version_of(original)),
            OwnerMode::Require,
        ));
        match outcome {
            SaveOutcome::Failed(message) => assert!(message.contains("保存失败"), "{message}"),
            other => panic!("应该失败：{other:?}"),
        }
        assert_eq!(
            backend.0.get("/srv/app/conf.txt").as_deref(),
            Some(original),
            "原文件必须完好"
        );
        assert_eq!(
            backend.0.names(),
            vec!["/srv/app/conf.txt".to_owned()],
            "临时文件要清掉"
        );
        let ops = backend.0.ops.lock().expect("锁").clone();
        assert!(
            ops.iter().any(|op| op.starts_with("write ")),
            "确实写过临时文件：{ops:?}"
        );
    }

    #[test]
    fn save_is_refused_when_the_remote_changed_underneath() {
        let backend = MemBackend(MemFs::with("/srv/app/conf.txt", b"someone else wrote this"));
        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/conf.txt",
            b"my edit",
            Some(version_of(b"what I opened")),
            OwnerMode::Require,
        ));
        assert_eq!(glimpse_size(&outcome), Some(Some(23)));
        assert_eq!(
            backend.0.get("/srv/app/conf.txt").as_deref(),
            Some(&b"someone else wrote this"[..])
        );
        assert!(
            backend.0.ops.lock().expect("锁").is_empty(),
            "冲突时一次写都不该有"
        );
    }

    #[test]
    fn a_deleted_remote_is_a_conflict_and_force_recreates_it() {
        let backend = MemBackend(MemFs::default());
        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/gone.txt",
            b"mine",
            Some(version_of(b"old")),
            OwnerMode::Require,
        ));
        assert_eq!(
            outcome,
            SaveOutcome::Conflict(RemoteGlimpse {
                gone: true,
                size: None
            })
        );
        assert!(backend.0.get("/srv/app/gone.txt").is_none(), "冲突不写");
        // 用户选了「覆盖远端」：不比版本，重新建出来。
        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/gone.txt",
            b"mine",
            None,
            OwnerMode::Require,
        ));
        assert!(matches!(outcome, SaveOutcome::Saved { .. }), "{outcome:?}");
        assert_eq!(
            backend.0.get("/srv/app/gone.txt").as_deref(),
            Some(&b"mine"[..])
        );
    }

    #[test]
    fn save_treats_a_remote_that_outgrew_the_limit_as_a_conflict() {
        let big = vec![b'a'; MAX_BYTES + 1];
        let backend = MemBackend(MemFs::with("/srv/app/big.txt", &big));
        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/big.txt",
            b"x",
            Some(version_of(b"x")),
            OwnerMode::Require,
        ));
        assert!(matches!(outcome, SaveOutcome::Conflict(_)), "{outcome:?}");
    }

    #[test]
    fn oversized_content_is_refused_before_touching_the_remote() {
        let backend = MemBackend(MemFs::with("/srv/app/a.txt", b"a"));
        let outcome = block_on(save_remote(
            &backend,
            "/srv/app/a.txt",
            &vec![b'x'; MAX_BYTES + 1],
            Some(version_of(b"a")),
            OwnerMode::Require,
        ));
        assert!(matches!(outcome, SaveOutcome::Failed(_)), "{outcome:?}");
        assert!(backend.0.ops.lock().expect("锁").is_empty());
    }

    /// 卡里要求的：两个编辑器拿着同一个旧版本同时保存，只有一个能写成，另一个报冲突，
    /// 远端是先写成的那份——不会互相覆盖。服务器慢（读要等一会儿）时没有闸一定撞车。
    #[test]
    fn two_editors_saving_the_same_file_do_not_overwrite_each_other() {
        let original = b"base\n";
        let backend = Arc::new(MemBackend(MemFs::with("/srv/app/shared.txt", original)));
        *backend.0.read_delay.lock().expect("锁") = Duration::from_millis(80);
        let (first, second) = block_on(async {
            let a = Arc::clone(&backend);
            let b = Arc::clone(&backend);
            let version = version_of(original);
            let one = tokio::spawn(async move {
                save_remote(
                    a.as_ref(),
                    "/srv/app/shared.txt",
                    b"from tab one\n",
                    Some(version),
                    OwnerMode::Require,
                )
                .await
            });
            let two = tokio::spawn(async move {
                save_remote(
                    b.as_ref(),
                    "/srv/app/shared.txt",
                    b"from tab two\n",
                    Some(version),
                    OwnerMode::Require,
                )
                .await
            });
            (one.await.expect("任务一"), two.await.expect("任务二"))
        });
        let saved: Vec<&SaveOutcome> = [&first, &second]
            .into_iter()
            .filter(|outcome| matches!(outcome, SaveOutcome::Saved { .. }))
            .collect();
        let conflicts = [&first, &second]
            .into_iter()
            .filter(|outcome| matches!(outcome, SaveOutcome::Conflict(_)))
            .count();
        assert_eq!(saved.len(), 1, "{first:?} / {second:?}");
        assert_eq!(conflicts, 1, "{first:?} / {second:?}");
        let winner = if matches!(first, SaveOutcome::Saved { .. }) {
            &b"from tab one\n"[..]
        } else {
            &b"from tab two\n"[..]
        };
        assert_eq!(
            backend.0.get("/srv/app/shared.txt").as_deref(),
            Some(winner),
            "远端必须是成功的那一份，不是被后来者盖掉的"
        );
    }

    #[test]
    fn a_stale_second_tab_cannot_overwrite_after_the_first_saved() {
        let original = b"base\n";
        let backend = MemBackend(MemFs::with("/srv/app/shared.txt", original));
        let version = version_of(original);
        let first = block_on(save_remote(
            &backend,
            "/srv/app/shared.txt",
            b"one\n",
            Some(version),
            OwnerMode::Require,
        ));
        assert!(matches!(first, SaveOutcome::Saved { .. }));
        let second = block_on(save_remote(
            &backend,
            "/srv/app/shared.txt",
            b"two\n",
            Some(version),
            OwnerMode::Require,
        ));
        assert_eq!(glimpse_size(&second), Some(Some(4)));
        assert_eq!(
            backend.0.get("/srv/app/shared.txt").as_deref(),
            Some(&b"one\n"[..])
        );
    }

    #[test]
    fn owner_that_cannot_be_kept_blocks_the_save_until_the_user_decides() {
        let original = b"base\n";
        let backend = MemBackend(MemFs::with("/etc/app.conf", original));
        *backend.0.deny_owner.lock().expect("锁") = true;
        let version = version_of(original);
        // Require：属主保不住就什么都不替换。
        let outcome = block_on(save_remote(
            &backend,
            "/etc/app.conf",
            b"new\n",
            Some(version),
            OwnerMode::Require,
        ));
        assert!(
            matches!(outcome, SaveOutcome::OwnerWouldChange { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            backend.0.get("/etc/app.conf").as_deref(),
            Some(&original[..]),
            "什么都没替换"
        );
        assert_eq!(backend.0.names(), vec!["/etc/app.conf".to_owned()]);
        // 用户同意属主改变：替换，并如实报告。
        let outcome = block_on(save_remote(
            &backend,
            "/etc/app.conf",
            b"new\n",
            Some(version),
            OwnerMode::Allow,
        ));
        match outcome {
            SaveOutcome::Saved { owner_changed, .. } => assert!(owner_changed.is_some()),
            other => panic!("应该保存成功：{other:?}"),
        }
        assert_eq!(
            backend.0.get("/etc/app.conf").as_deref(),
            Some(&b"new\n"[..])
        );
    }

    /// 属主提示里的第三个选项「直接写入原文件」：版本核对照样做（远端变了仍然报冲突），通过才写。
    #[test]
    fn writing_in_place_still_checks_the_version_first() {
        let original = b"base\n";
        let backend = MemBackend(MemFs::with("/etc/app.conf", original));
        let version = version_of(original);
        let outcome = block_on(save_remote(
            &backend,
            "/etc/app.conf",
            b"in place\n",
            Some(version),
            OwnerMode::InPlace,
        ));
        assert!(matches!(outcome, SaveOutcome::Saved { .. }), "{outcome:?}");
        assert_eq!(
            backend.0.get("/etc/app.conf").as_deref(),
            Some(&b"in place\n"[..])
        );
        // 旧版本再来一次：远端已经变了，拒绝。
        let outcome = block_on(save_remote(
            &backend,
            "/etc/app.conf",
            b"stale\n",
            Some(version),
            OwnerMode::InPlace,
        ));
        assert!(matches!(outcome, SaveOutcome::Conflict(_)), "{outcome:?}");
        assert_eq!(
            backend.0.get("/etc/app.conf").as_deref(),
            Some(&b"in place\n"[..])
        );
    }

    #[test]
    fn save_beside_never_overwrites_an_existing_file() {
        let backend = MemBackend(MemFs::with("/srv/app/a.txt", b"mine"));
        backend.0.put("/srv/app/a (副本).txt", b"someone else");
        let refused = block_on(save_beside(
            &backend,
            "/srv/app/a.txt",
            "a (副本).txt",
            b"x",
        ))
        .expect_err("目标已存在");
        assert!(refused.contains("已经存在"), "{refused}");
        assert_eq!(
            backend.0.get("/srv/app/a (副本).txt").as_deref(),
            Some(&b"someone else"[..])
        );
        let made =
            block_on(save_beside(&backend, "/srv/app/a.txt", "b.txt", b"new")).expect("成功");
        assert_eq!(made, "/srv/app/b.txt");
        assert_eq!(
            backend.0.get("/srv/app/b.txt").as_deref(),
            Some(&b"new"[..])
        );
        // 名字不合法：不带路径、不能为空。
        assert!(block_on(save_beside(&backend, "/srv/app/a.txt", "x/y", b"")).is_err());
        assert!(block_on(save_beside(&backend, "/srv/app/a.txt", "  ", b"")).is_err());
    }

    #[test]
    fn fetch_refuses_directories_and_oversized_files_with_a_clear_reason() {
        let backend = MemBackend(MemFs::with("/srv/app/ok.txt", b"hello"));
        assert_eq!(
            block_on(fetch_bytes(&backend, "/srv/app/ok.txt")).expect("普通文件"),
            b"hello"
        );
        let missing = block_on(fetch_bytes(&backend, "/srv/app/none.txt")).expect_err("不存在");
        assert!(missing.contains("打开失败"), "{missing}");
        let big = vec![b'a'; MAX_BYTES + 1];
        backend.0.put("/srv/app/big.txt", &big);
        let too_big = block_on(fetch_bytes(&backend, "/srv/app/big.txt")).expect_err("超限");
        assert!(too_big.contains("超过内置编辑器的上限"), "{too_big}");
    }

    #[test]
    fn disconnects_are_recognised_from_the_error_not_its_wording() {
        // 断线在拿到错误的那一刻判断，中英文界面一样；文案本身随语言变。
        let lost = SftpError::Disconnected("channel closed".to_owned());
        let reset = SftpError::Local {
            path: "/srv/app/a.txt".to_owned(),
            message: "Connection reset by peer".to_owned(),
        };
        let denied = SftpError::PermissionDenied {
            path: "/srv/app/a.txt".to_owned(),
        };
        for lang in [Lang::Zh, Lang::En] {
            assert!(LoadFailure::from_sftp(lang, "p: ", &lost, "").disconnected);
            assert!(LoadFailure::from_sftp(lang, "p: ", &reset, "").disconnected);
            let failure = LoadFailure::from_sftp(lang, "p: ", &denied, "");
            assert!(!failure.disconnected, "{lang:?}");
            assert!(failure.message.starts_with("p: "), "{}", failure.message);
        }
        let english = LoadFailure::from_sftp(Lang::En, "", &lost, "").message;
        assert!(
            !english
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "英文界面的断线文案里不能有中文：{english}"
        );
        assert!(!LoadFailure::other("太大".to_owned()).disconnected);
    }
}
