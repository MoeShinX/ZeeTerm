//! 底部同步状态条的数据（0.2.14 `SyncStatusBar.tsx` + `state/syncModel.ts` 的 `statusBarModel`）。
//!
//! 连接了存储才出现；失败原因只在这里和设置页的同步一节显示，不弹窗；点一下打开设置页的
//! 同步一节。数据从本机设置表和进程级的同步保护状态里读：读库放在阻塞线程里每 2 秒做一次
//! （同步、恢复、改设置之后也会马上补一次），界面只读内存里的最新一份。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ::gpui::App;
use zssh_core::sync::service::{self, LastResult};

use super::{
    AutoPause, ERROR_STATUS, ExclusiveKind, SYNC_IN_FLIGHT, baseline_broken_at, load_conflicts_at,
    open_existing_db_at, with_sync_guard,
};
use crate::datetime::{civil_from_days, local_utc_offset};
use crate::i18n::{Msg, t};

/// 底部同步状态条（`status.rs`）自己的界面文案：各状态的整句、前缀 / 后缀和凭据叫法。
///
/// 只放应用自己的固定文案；失败原因（`last.summary`）和存储名属于远端 / 用户数据，原样拼在
/// 本地化前缀后面，不经过这里。`webdav` / `s3` / `github` / `onedrive` 这些协议键不进这张表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncStatusMsg {
    Syncing,
    AutoPausedPrefix,
    PendingDeletionDefault,
    PendingDeletionSuffix,
    BaselineBroken,
    NoMasterPassword,
    NeedMasterPassword,
    OneDriveExpired,
    AuthExpiredPrefix,
    AuthExpiredSuffix,
    NotSyncedYet,
    SyncFailedPrefix,
    LocalChanged,
    Cancelled,
    SyncedPrefix,
    ConflictsMiddle,
    ConflictsSuffix,
    CredentialToken,
    CredentialAccessKey,
    CredentialAccount,
}

impl Msg for SyncStatusMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::Syncing => "同步中…",
            Self::AutoPausedPrefix => "自动同步已暂停：",
            Self::PendingDeletionDefault => "同步会删除很多记录",
            Self::PendingDeletionSuffix => "，等待你确认",
            Self::BaselineBroken => "同步基线损坏，需要重建后才能同步",
            Self::NoMasterPassword => "同步：还没有设置主密码",
            Self::NeedMasterPassword => "同步：需要输入主密码",
            Self::OneDriveExpired => "同步已暂停：OneDrive 登录已失效，请重新登录",
            Self::AuthExpiredPrefix => "同步已暂停：",
            Self::AuthExpiredSuffix => "已失效，请重新填写",
            Self::NotSyncedYet => "同步：尚未同步",
            Self::SyncFailedPrefix => "同步失败：",
            Self::LocalChanged => "同步期间有改动，稍后自动重新同步",
            Self::Cancelled => "上次同步已取消",
            Self::SyncedPrefix => "已同步 ",
            Self::ConflictsMiddle => "，",
            Self::ConflictsSuffix => " 条冲突待处理",
            Self::CredentialToken => "令牌",
            Self::CredentialAccessKey => "访问密钥",
            Self::CredentialAccount => "账号或密码",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::Syncing => "Syncing…",
            Self::AutoPausedPrefix => "Auto sync paused: ",
            Self::PendingDeletionDefault => "The sync would delete many records",
            Self::PendingDeletionSuffix => ", waiting for your confirmation",
            Self::BaselineBroken => "The sync baseline is broken; rebuild it before syncing",
            Self::NoMasterPassword => "Sync: no master password set yet",
            Self::NeedMasterPassword => "Sync: the master password is required",
            Self::OneDriveExpired => {
                "Sync paused: the OneDrive sign-in has expired; please sign in again"
            }
            Self::AuthExpiredPrefix => "Sync paused: ",
            Self::AuthExpiredSuffix => " has expired; please enter it again",
            Self::NotSyncedYet => "Sync: not synced yet",
            Self::SyncFailedPrefix => "Sync failed: ",
            Self::LocalChanged => "Changed during the sync; it retries automatically later",
            Self::Cancelled => "The last sync was cancelled",
            Self::SyncedPrefix => "Synced ",
            Self::ConflictsMiddle => ", ",
            Self::ConflictsSuffix => " conflicts to resolve",
            Self::CredentialToken => "the token",
            Self::CredentialAccessKey => "the access key",
            Self::CredentialAccount => "the username or password",
        }
    }
}

/// 状态条的颜色档位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Idle,
    Ok,
    Busy,
    Warn,
    Error,
}

/// 状态条要显示什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyncBarModel {
    pub visible: bool,
    pub tone: Tone,
    pub text: String,
}

impl SyncBarModel {
    fn hidden() -> Self {
        Self {
            visible: false,
            tone: Tone::Idle,
            text: String::new(),
        }
    }

    fn shown(tone: Tone, text: impl Into<String>) -> Self {
        Self {
            visible: true,
            tone,
            text: text.into(),
        }
    }
}

/// 算状态条要用的全部输入。纯数据，方便单测。
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct BarInput {
    /// 本机保存了存储设置。
    pub configured: bool,
    /// 存储种类的键（`webdav` / `s3` / `github` / `onedrive`）。
    pub provider: Option<&'static str>,
    /// 同步正在跑（手动或自动）。
    pub running: bool,
    /// 有停在防误删确认上的同步。
    pub pending: bool,
    pub baseline_broken: bool,
    /// 这台电脑已经和云端对上过（设置过主密码）。
    pub ready: bool,
    /// 主密码现在可用：本机记住了，或者设置页的输入框里有。
    pub password_available: bool,
    /// 存储拒绝了凭据，自动同步暂停。
    pub auth_paused: bool,
    pub last: Option<LastResult>,
    pub conflict_count: usize,
}

/// 被拒绝的是什么凭据（各家叫法不同）。
fn credential_name(provider: Option<&str>) -> &'static str {
    match provider {
        Some("github") => t(SyncStatusMsg::CredentialToken),
        Some("s3") => t(SyncStatusMsg::CredentialAccessKey),
        _ => t(SyncStatusMsg::CredentialAccount),
    }
}

/// 本地时间 `YYYY-MM-DD HH:mm`（同 0.2.14 的 `formatStamp`）。
pub(crate) fn format_stamp(secs: i64) -> String {
    let local = secs + local_utc_offset();
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    let in_day = local.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        in_day / 3_600,
        in_day % 3_600 / 60
    )
}

/// 状态条的文案和颜色；优先级同 0.2.14：没连存储就隐藏 → 同步中 → 待确认 → 基线损坏 → 还没设置主密码
/// → 需要输入主密码 → 凭据失效 → 上次结果。
pub(super) fn model_for(input: &BarInput) -> SyncBarModel {
    if !input.configured {
        return SyncBarModel::hidden();
    }
    if input.running {
        return SyncBarModel::shown(Tone::Busy, t(SyncStatusMsg::Syncing));
    }
    if input.pending {
        let auto = input.last.as_ref().is_some_and(|last| last.auto);
        let prefix = if auto { t(SyncStatusMsg::AutoPausedPrefix) } else { "" };
        let detail = input
            .last
            .as_ref()
            .filter(|last| last.status == "needs_confirmation")
            .map(|last| last.summary.as_str())
            .filter(|summary| !summary.is_empty())
            .unwrap_or(t(SyncStatusMsg::PendingDeletionDefault));
        return SyncBarModel::shown(
            Tone::Warn,
            format!(
                "{prefix}{detail}{}",
                t(SyncStatusMsg::PendingDeletionSuffix)
            ),
        );
    }
    if input.baseline_broken {
        return SyncBarModel::shown(Tone::Error, t(SyncStatusMsg::BaselineBroken));
    }
    if !input.ready {
        return SyncBarModel::shown(Tone::Warn, t(SyncStatusMsg::NoMasterPassword));
    }
    if !input.password_available {
        return SyncBarModel::shown(Tone::Warn, t(SyncStatusMsg::NeedMasterPassword));
    }
    if input.auth_paused {
        let text = if input.provider == Some("onedrive") {
            t(SyncStatusMsg::OneDriveExpired).to_owned()
        } else {
            format!(
                "{}{}{}",
                t(SyncStatusMsg::AuthExpiredPrefix),
                credential_name(input.provider),
                t(SyncStatusMsg::AuthExpiredSuffix)
            )
        };
        return SyncBarModel::shown(Tone::Error, text);
    }
    let Some(last) = &input.last else {
        return SyncBarModel::shown(Tone::Idle, t(SyncStatusMsg::NotSyncedYet));
    };
    match last.status.as_str() {
        ERROR_STATUS => {
            return SyncBarModel::shown(
                Tone::Error,
                format!("{}{}", t(SyncStatusMsg::SyncFailedPrefix), last.summary),
            );
        }
        "local_changed" => {
            return SyncBarModel::shown(Tone::Idle, t(SyncStatusMsg::LocalChanged));
        }
        "cancelled" => return SyncBarModel::shown(Tone::Idle, t(SyncStatusMsg::Cancelled)),
        _ => {}
    }
    let stamp = format_stamp(last.at);
    let time = stamp.get(11..).unwrap_or(&stamp);
    if input.conflict_count > 0 {
        SyncBarModel::shown(
            Tone::Warn,
            format!(
                "{}{time}{}{}{}",
                t(SyncStatusMsg::SyncedPrefix),
                t(SyncStatusMsg::ConflictsMiddle),
                input.conflict_count,
                t(SyncStatusMsg::ConflictsSuffix)
            ),
        )
    } else {
        SyncBarModel::shown(
            Tone::Ok,
            format!("{}{time}", t(SyncStatusMsg::SyncedPrefix)),
        )
    }
}

// ---------------------------------------------------------------------------
// 进程级缓存
// ---------------------------------------------------------------------------

/// 设置页的主密码输入框里现在有没有字（没记住主密码时，输入框就是「内存里的主密码」）。
static PASSWORD_IN_MEMORY: AtomicBool = AtomicBool::new(false);

/// 读库得到的那部分输入（会变慢的东西），和每次重绘现算的部分分开存。
static STORED: OnceLock<Mutex<Stored>> = OnceLock::new();

/// 缓存版本号：数据变了就 +1，界面靠它判断要不要重绘。
static REVISION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, PartialEq)]
struct Stored {
    configured: bool,
    provider: Option<&'static str>,
    ready: bool,
    remembered: bool,
    baseline_broken: bool,
    last: Option<LastResult>,
    conflict_count: usize,
}

fn stored() -> &'static Mutex<Stored> {
    STORED.get_or_init(|| Mutex::new(Stored::default()))
}

/// 设置页（同步一节）把「输入框里有主密码」告诉状态条。
pub(super) fn set_password_in_memory(present: bool) {
    PASSWORD_IN_MEMORY.store(present, Ordering::SeqCst);
}

/// 读一遍本机设置，放进缓存。在阻塞线程里调用（要开库）。
pub(crate) fn refresh_now() {
    let data_dir = crate::settings::data_dir();
    let mut next = Stored::default();
    if data_dir.join("zssh.db").is_file()
        && let Ok(db) = open_existing_db_at(&data_dir)
    {
        let config = service::load_config(&db, &super::DpapiSealer)
            .ok()
            .flatten();
        next.configured = config.is_some();
        next.provider = config.as_ref().map(|config| config.provider());
        next.ready = service::is_ready(&db).unwrap_or(false);
        // 只关心「有没有记住」，不需要解开密文。
        next.remembered = service::load_master_password(&db, &super::DpapiSealer)
            .ok()
            .flatten()
            .is_some();
        next.last = service::load_last_result(&db).ok().flatten();
        drop(db);
        next.baseline_broken = baseline_broken_at(&data_dir);
        next.conflict_count = load_conflicts_at(&data_dir).map_or(0, |items| items.len());
    }
    let mut current = stored()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *current != next {
        *current = next;
        REVISION.fetch_add(1, Ordering::SeqCst);
    }
}

/// 在后台补一次刷新（同步 / 恢复 / 改设置之后调，不等定时器）。
pub(crate) fn request_refresh() {
    if let Ok(runtime) = crate::ssh_backend::runtime() {
        drop(runtime.spawn_blocking(refresh_now));
    }
}

/// 拉起定时刷新：主窗口启动时调一次；任务挂在 `App` 上，进程退出时一起结束。
pub(crate) fn spawn_status_refresher(cx: &mut App) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            request_refresh();
            cx.background_executor().timer(Duration::from_secs(2)).await;
        }
    })
    .detach();
}

/// 现在的状态条内容和版本号。每次重绘前现算「同步中 / 凭据暂停 / 主密码在不在内存里」，
/// 它们变得快，也不用读库。
pub(crate) fn bar_model() -> (u64, SyncBarModel) {
    let stored = stored()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let running = SYNC_IN_FLIGHT.load(Ordering::SeqCst) == ExclusiveKind::Sync as u8;
    let (pending, auth_paused) = with_sync_guard(|guard| {
        (
            guard.pending_confirmation,
            matches!(guard.pause, Some(AutoPause::Auth(_))),
        )
    });
    let input = BarInput {
        configured: stored.configured,
        provider: stored.provider,
        running,
        pending,
        baseline_broken: stored.baseline_broken,
        ready: stored.ready,
        password_available: stored.remembered || PASSWORD_IN_MEMORY.load(Ordering::SeqCst),
        auth_paused,
        last: stored.last,
        conflict_count: stored.conflict_count,
    };
    let model = model_for(&input);
    // 现算的那几项也要让重绘知道（同步开始 / 结束时版本号不变）。
    let live = u64::from(running) | u64::from(pending) << 1 | u64::from(auth_paused) << 2;
    (REVISION.load(Ordering::SeqCst) << 3 | live, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(status: &str, summary: &str, auto: bool) -> LastResult {
        LastResult {
            at: 1_700_000_000,
            status: status.to_owned(),
            summary: summary.to_owned(),
            version: None,
            conflicts: 0,
            auto,
        }
    }

    fn ready() -> BarInput {
        BarInput {
            configured: true,
            provider: Some("webdav"),
            ready: true,
            password_available: true,
            last: Some(last("completed", "ok", false)),
            ..BarInput::default()
        }
    }

    #[test]
    fn hidden_until_a_storage_is_connected() {
        assert!(!model_for(&BarInput::default()).visible);
        assert!(model_for(&ready()).visible);
    }

    #[test]
    fn running_and_pending_come_before_everything_else() {
        let mut input = ready();
        input.baseline_broken = true;
        input.running = true;
        assert_eq!(model_for(&input).text, "同步中…");
        assert_eq!(model_for(&input).tone, Tone::Busy);

        input.running = false;
        input.pending = true;
        input.last = Some(last(
            "needs_confirmation",
            "这次同步会删除 12 条记录（本地共 20 条），其中主机 3 台（本地共 5 台）",
            true,
        ));
        let model = model_for(&input);
        assert_eq!(model.tone, Tone::Warn);
        assert!(model.text.starts_with("自动同步已暂停："), "{}", model.text);
        assert!(model.text.contains("12 条记录"), "{}", model.text);
        assert!(model.text.ends_with("等待你确认"), "{}", model.text);

        // 手动停在确认上：不带「自动同步已暂停」。
        input.last = Some(last("needs_confirmation", "删除 3 条", false));
        assert!(!model_for(&input).text.starts_with("自动同步"));
    }

    #[test]
    fn broken_baseline_and_missing_password_states() {
        let mut input = ready();
        input.baseline_broken = true;
        let model = model_for(&input);
        assert_eq!(model.tone, Tone::Error);
        assert!(model.text.contains("基线损坏"));

        let mut input = ready();
        input.ready = false;
        assert_eq!(model_for(&input).text, "同步：还没有设置主密码");
        let mut input = ready();
        input.password_available = false;
        assert_eq!(model_for(&input).text, "同步：需要输入主密码");
    }

    #[test]
    fn credential_pause_names_the_right_credential() {
        let mut input = ready();
        input.auth_paused = true;
        assert_eq!(
            model_for(&input).text,
            "同步已暂停：账号或密码已失效，请重新填写"
        );
        input.provider = Some("github");
        assert_eq!(model_for(&input).text, "同步已暂停：令牌已失效，请重新填写");
        input.provider = Some("s3");
        assert_eq!(
            model_for(&input).text,
            "同步已暂停：访问密钥已失效，请重新填写"
        );
        input.provider = Some("onedrive");
        assert_eq!(
            model_for(&input).text,
            "同步已暂停：OneDrive 登录已失效，请重新登录"
        );
        assert_eq!(model_for(&input).tone, Tone::Error);
    }

    #[test]
    fn last_result_decides_the_rest() {
        let mut input = ready();
        input.last = None;
        assert_eq!(model_for(&input).text, "同步：尚未同步");

        input.last = Some(last(ERROR_STATUS, "连接超时", false));
        let model = model_for(&input);
        assert_eq!(
            (model.tone, model.text.as_str()),
            (Tone::Error, "同步失败：连接超时")
        );

        input.last = Some(last("cancelled", "", false));
        assert_eq!(model_for(&input).text, "上次同步已取消");
        input.last = Some(last("local_changed", "", false));
        assert_eq!(model_for(&input).text, "同步期间有改动，稍后自动重新同步");

        input.last = Some(last("completed", "", false));
        let model = model_for(&input);
        assert_eq!(model.tone, Tone::Ok);
        assert!(model.text.starts_with("已同步 "), "{}", model.text);
        input.conflict_count = 2;
        let model = model_for(&input);
        assert_eq!(model.tone, Tone::Warn);
        assert!(model.text.ends_with("，2 条冲突待处理"), "{}", model.text);
    }

    #[test]
    fn stamps_are_local_time_to_the_minute() {
        // 形状固定：YYYY-MM-DD HH:mm（本地时区由系统决定，测试里只看格式）。
        let stamp = format_stamp(1_700_000_000);
        assert_eq!(stamp.len(), 16, "{stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], " ");
        assert_eq!(&stamp[13..14], ":");
    }
}
