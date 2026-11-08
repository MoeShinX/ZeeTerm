//! 锁屏（V2-U26）的核心规则：密码规则、Argon2id 哈希的存取与校验、输错后的退避、设置键、
//! 忘记密码时“用 Windows 账户验证”的分支逻辑。
//!
//! 这里不碰界面：界面（底部栏的锁按钮、锁屏页、设置页）在 `native/zssh-gpui`，只调这里的函数。
//!
//! - 密码只保存 Argon2id 哈希 + 随机盐，不存明文，不可逆；参数取同步包 / 离线备份用的那一组
//!   （128 MiB × 8 次 × 1 并行，见 [`crate::sync::crypto`]），不更弱。
//! - 校验用常数时间比较；记录里的参数被改过也不会让校验崩溃或去分配几 GB 内存（有范围检查）。
//! - 设置键都只留在本机（`sync::settings::SETTINGS_SYNC_CLASS` 里是 `LocalOnly`），所以
//!   不会进同步包，也不会进离线备份。
//! - 密码不进日志：这个模块里没有任何 `tracing` 调用，`Debug` 输出里也只有盐和哈希的长度。

use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{reason_keys as rk, Reason, ZsshError};
use crate::store::Database;

/// 密码记录（JSON 文本）。键存在且能解析 = 锁屏已启用。
pub const HASH_KEY: &str = "app_lock_hash";
/// 空闲多久自动锁定（分钟，JSON 数字；0 = 不自动）。
pub const IDLE_MINUTES_KEY: &str = "app_lock_idle_minutes";
/// 关到托盘时自动锁定（JSON 布尔）。
pub const LOCK_ON_TRAY_KEY: &str = "app_lock_on_tray";

/// 启动时就锁定（JSON 布尔）。只有设了锁屏密码才能打开；关锁屏时一起清掉。
pub const LOCK_ON_START_KEY: &str = "app_lock_on_start";

/// 本模块用到的全部设置键（同步白名单里要逐个登记为本机键）。
pub const SETTING_KEYS: [&str; 4] = [
    HASH_KEY,
    IDLE_MINUTES_KEY,
    LOCK_ON_TRAY_KEY,
    LOCK_ON_START_KEY,
];

/// 锁屏密码的长度范围（按字符数，不是字节数）。
pub const MIN_PASSWORD_CHARS: usize = 4;
pub const MAX_PASSWORD_CHARS: usize = 128;

/// 设置页里“空闲多久自动锁定”的几档（分钟）；0 = 不自动，是默认。
pub const IDLE_CHOICES: [u32; 6] = [0, 5, 10, 15, 30, 60];

const FORMAT_VERSION: u32 = 1;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;

/// 输错几次以内不等待；之后每次输错都要等一会儿。
pub const FREE_ATTEMPTS: u32 = 5;
/// 第 [`FREE_ATTEMPTS`] 次输错后要等的时间，之后每次翻倍，直到 [`MAX_DELAY`]。
pub const BASE_DELAY: Duration = Duration::from_secs(5);
pub const MAX_DELAY: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// 密码规则
// ---------------------------------------------------------------------------

/// 新密码不合格的原因（文字直接给界面用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordProblem {
    TooShort,
    TooLong,
    Mismatch,
}

impl PasswordProblem {
    pub fn message(self) -> String {
        match self {
            Self::TooShort => format!("密码至少 {MIN_PASSWORD_CHARS} 个字符"),
            Self::TooLong => format!("密码最多 {MAX_PASSWORD_CHARS} 个字符"),
            Self::Mismatch => "两次输入的密码不一致".to_owned(),
        }
    }
}

impl std::fmt::Display for PasswordProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// 设置 / 修改 / 重置时的新密码检查：长度够、不超长、两次一致。先看长度再看一致，
/// 一次只报一个原因。密码原样使用，不去掉首尾空白（空格也是密码的一部分）。
pub fn check_new_password(first: &str, second: &str) -> Result<(), PasswordProblem> {
    let chars = first.chars().count();
    if chars < MIN_PASSWORD_CHARS {
        return Err(PasswordProblem::TooShort);
    }
    if chars > MAX_PASSWORD_CHARS {
        return Err(PasswordProblem::TooLong);
    }
    if first != second {
        return Err(PasswordProblem::Mismatch);
    }
    Ok(())
}

/// “空闲多久自动锁定”存进库的值要落在几档里：不认识的值按 0（不自动）处理。
pub fn sanitize_idle_minutes(minutes: u32) -> u32 {
    if IDLE_CHOICES.contains(&minutes) {
        minutes
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// 哈希记录
// ---------------------------------------------------------------------------

/// Argon2id 的代价参数（KiB / 次数 / 并行度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfCost {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl KdfCost {
    /// 正式用的参数：和同步包 / 离线备份一致（128 MiB × 8 × 1）。
    pub const STANDARD: Self = Self {
        memory_kib: 128 * 1024,
        iterations: 8,
        parallelism: 1,
    };

    /// 记录里读出来的参数要在这个范围内才肯算（防止改库文件让校验去分配几 GB 内存）。
    fn in_range(self) -> bool {
        (8 * 1024..=256 * 1024).contains(&self.memory_kib)
            && (1..=64).contains(&self.iterations)
            && (1..=64).contains(&self.parallelism)
    }
}

/// 存进库的密码记录：参数、盐、哈希。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordRecord {
    #[serde(rename = "v")]
    version: u32,
    #[serde(rename = "alg")]
    algorithm: String,
    #[serde(flatten)]
    cost: KdfCost,
    /// base64，16 字节随机盐。
    salt: String,
    /// base64，32 字节 Argon2id 输出。
    hash: String,
}

impl std::fmt::Debug for PasswordRecord {
    // 盐和哈希虽然不是密码，也没必要进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordRecord")
            .field("version", &self.version)
            .field("cost", &self.cost)
            .finish_non_exhaustive()
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

fn derive(
    password: &str,
    salt: &[u8],
    cost: KdfCost,
) -> Result<Zeroizing<[u8; HASH_BYTES]>, ZsshError> {
    let params = Params::new(
        cost.memory_kib,
        cost.iterations,
        cost.parallelism,
        Some(HASH_BYTES),
    )
    // 参数来自代码里的常量，坏的话是开发错误，用户见不到。
    .map_err(|err| ZsshError::InvalidInput(Reason::text(format!("锁屏哈希参数不合法：{err}"))))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; HASH_BYTES]);
    argon
        .hash_password_into(password.as_bytes(), salt, out.as_mut())
        .map_err(|err| ZsshError::InvalidInput(Reason::text(format!("锁屏哈希计算失败：{err}"))))?;
    Ok(out)
}

/// 常数时间比较（长度不同直接不等：长度本来就不是秘密）。
fn equal_constant_time(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

impl PasswordRecord {
    /// 用随机盐和正式参数给新密码建一条记录（要上百毫秒，界面放到后台线程里做）。
    pub fn create(password: &str) -> Result<Self, ZsshError> {
        Self::create_with(password, KdfCost::STANDARD)
    }

    /// 同 [`Self::create`]，参数可指定（测试用小参数省时间）。
    pub fn create_with(password: &str, cost: KdfCost) -> Result<Self, ZsshError> {
        if !cost.in_range() {
            return Err(ZsshError::InvalidInput(Reason::text(
                "锁屏哈希参数超出范围",
            )));
        }
        let mut salt = [0u8; SALT_BYTES];
        getrandom::fill(&mut salt)
            .map_err(|err| ZsshError::InvalidInput(Reason::text(format!("取不到随机数：{err}"))))?;
        let hash = derive(password, &salt, cost)?;
        Ok(Self {
            version: FORMAT_VERSION,
            algorithm: "argon2id".to_owned(),
            cost,
            salt: b64(&salt),
            hash: b64(hash.as_ref()),
        })
    }

    /// 密码对不对。记录本身坏了（版本不认识、base64 不对、参数越界）一律当“不对”。
    pub fn verify(&self, password: &str) -> bool {
        if self.version != FORMAT_VERSION || self.algorithm != "argon2id" || !self.cost.in_range() {
            return false;
        }
        let (Some(salt), Some(expected)) = (unb64(&self.salt), unb64(&self.hash)) else {
            return false;
        };
        if salt.len() < 8 || expected.len() != HASH_BYTES {
            return false;
        }
        match derive(password, &salt, self.cost) {
            Ok(actual) => equal_constant_time(actual.as_ref(), &expected),
            Err(_) => false,
        }
    }

    /// 存进库的 JSON 文本。
    pub fn to_json(&self) -> Result<String, ZsshError> {
        serde_json::to_string(self).map_err(|err| {
            ZsshError::InvalidInput(Reason::text(format!("锁屏记录序列化失败：{err}")))
        })
    }

    /// 从库里的 JSON 文本读回；格式不对返回 `None`。
    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

// ---------------------------------------------------------------------------
// 设置的读写
// ---------------------------------------------------------------------------

/// 锁屏现在的状态（从库里读出来的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockStatus {
    /// 没启用。
    Disabled,
    /// 启用了，记录完好。
    Enabled(PasswordRecord),
    /// 库里有记录但读不懂（文件被改、版本更新）。**不能当成没启用**：悄悄关掉锁等于绕过它。
    /// 按已启用处理，密码无法校验，只能走“用 Windows 账户验证后重置”。
    Unreadable,
    /// 读库本身失败了（不是“没有这条记录”）：不知道有没有设过锁。**同样不能当成没启用**——
    /// 临时的读错误不该让锁悄悄失效。按受限处理：启动即锁、托盘锁定都生效，锁屏页上不给密码框，
    /// 只能“重试”（重新读，读成功就按真实状态走）或退出程序。
    ReadFailed,
}

impl LockStatus {
    /// 锁屏算不算生效。`Unreadable` 和 `ReadFailed` 都按生效算（失败即收紧）。
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// 是不是“读不到设置、不知道有没有锁”。这种状态没有密码可以校验。
    pub fn is_read_failed(&self) -> bool {
        matches!(self, Self::ReadFailed)
    }
}

/// 锁屏的全部设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockConfig {
    pub status: LockStatus,
    /// 空闲多久自动锁定（分钟，0 = 不自动）。
    pub idle_minutes: u32,
    /// 关到托盘时自动锁定。
    pub lock_on_tray: bool,
    /// 启动时就锁定（程序一开就显示锁屏页）。库里这一项单独存着，是否真的生效还要看锁屏有没有启用，
    /// 见 [`LockConfig::start_locked`]。
    pub lock_on_start: bool,
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            status: LockStatus::Disabled,
            idle_minutes: 0,
            lock_on_tray: false,
            lock_on_start: false,
        }
    }
}

impl LockConfig {
    /// 读设置。区分三种情况：
    /// - 库里没有记录（`Ok(None)`）= 没启用，其余项取默认；
    /// - 值存在但读不懂 = 取默认（`Unreadable` 另算），这不是读库失败；
    /// - **读库本身失败（`Err`）**：密码记录读不出来 = [`LockStatus::ReadFailed`]；锁已生效（含
    ///   `Unreadable` / `ReadFailed`）时，另外几项读失败取最严格的值（启动即锁、托盘锁定都开），
    ///   空闲分钟数读失败保持 0（不凭空多出一条空闲锁）。
    pub fn load(db: &Database) -> Self {
        let status = match db.setting_get(HASH_KEY) {
            Ok(Some(raw)) => status_from_stored(&raw),
            Ok(None) => LockStatus::Disabled,
            Err(_) => LockStatus::ReadFailed,
        };
        let strict = status.is_enabled();
        let idle_minutes = db
            .setting_get(IDLE_MINUTES_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<u32>(&raw).ok())
            .map_or(0, sanitize_idle_minutes);
        let lock_on_tray = read_flag(db, LOCK_ON_TRAY_KEY, strict);
        let lock_on_start = read_flag(db, LOCK_ON_START_KEY, strict);
        Self {
            status,
            idle_minutes,
            lock_on_tray,
            lock_on_start,
        }
    }

    /// 关到托盘时要不要锁：开关打开，或者读不到设置（不知道有没有锁，按受限处理）。
    pub fn tray_locks(&self) -> bool {
        self.lock_on_tray || self.status.is_read_failed()
    }

    /// 这次启动要不要一开始就锁着：开关打开**并且**锁屏还启用着（库里的开关残留、而密码已经没了时
    /// 不能把人锁在一个没有密码的锁屏页上）。读不到设置（`ReadFailed`）时一律锁着，页面上是“重试 / 退出”。
    pub fn start_locked(&self) -> bool {
        self.status.is_read_failed() || (self.lock_on_start && self.status.is_enabled())
    }
}

/// 读一个布尔开关。读库失败：`strict` 时取 `true`（失败即收紧），否则取 `false`；
/// 值存在但不是布尔、或根本没有这一项，一律 `false`（维持原来的宽容）。
fn read_flag(db: &Database, key: &str, strict: bool) -> bool {
    match db.setting_get(key) {
        Ok(raw) => raw
            .and_then(|raw| serde_json::from_str::<bool>(&raw).ok())
            .unwrap_or(false),
        Err(_) => strict,
    }
}

fn status_from_stored(raw: &str) -> LockStatus {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return LockStatus::Disabled;
    }
    match PasswordRecord::from_json(trimmed) {
        Some(record) => LockStatus::Enabled(record),
        None => LockStatus::Unreadable,
    }
}

/// 存密码记录（启用 / 修改 / 重置都走这里）。
pub fn save_password(db: &Database, record: &PasswordRecord) -> Result<(), ZsshError> {
    db.setting_set(HASH_KEY, &record.to_json()?)
}

/// 关闭锁屏：清掉密码记录（设置表没有删除接口，写成 JSON `null`，读的时候当没启用）。
/// “启动时锁定”依附于密码，一起关掉——不然哪天重新启用锁屏，会在用户没要求的情况下悄悄变成启动即锁。
pub fn clear_password(db: &Database) -> Result<(), ZsshError> {
    db.setting_set(HASH_KEY, "null")?;
    db.setting_set(LOCK_ON_START_KEY, "false")
}

pub fn save_idle_minutes(db: &Database, minutes: u32) -> Result<(), ZsshError> {
    db.setting_set(
        IDLE_MINUTES_KEY,
        &sanitize_idle_minutes(minutes).to_string(),
    )
}

pub fn save_lock_on_tray(db: &Database, on: bool) -> Result<(), ZsshError> {
    db.setting_set(LOCK_ON_TRAY_KEY, if on { "true" } else { "false" })
}

/// 打开 / 关闭“启动时锁定”。打开要求锁屏已经启用（设了密码）：没有密码的锁屏页谁都解不开，
/// 也不该有这种状态。界面上开关会置灰，这里是最后一道防线。关闭随时可以。
pub fn save_lock_on_start(db: &Database, on: bool) -> Result<(), ZsshError> {
    if on && !LockConfig::load(db).status.is_enabled() {
        return Err(ZsshError::InvalidInput(Reason::new(
            rk::LOCK_ON_START_NEEDS_PASSWORD,
            "请先设置锁屏密码，再打开“启动时锁定”",
        )));
    }
    db.setting_set(LOCK_ON_START_KEY, if on { "true" } else { "false" })
}

// ---------------------------------------------------------------------------
// 输错后的退避
// ---------------------------------------------------------------------------

/// 输错 `failures` 次（连续）之后要等多久才能再试：前 4 次不等，第 5 次起 5 秒、10 秒、20 秒……
/// 上限 60 秒。
pub fn delay_after(failures: u32) -> Duration {
    if failures < FREE_ATTEMPTS {
        return Duration::ZERO;
    }
    let doublings = (failures - FREE_ATTEMPTS).min(8);
    BASE_DELAY
        .checked_mul(1u32 << doublings)
        .map_or(MAX_DELAY, |delay| delay.min(MAX_DELAY))
}

/// 输错计数 + 退避。时间由调用方传进来，测试不用真等。**所有校验入口共用一份**（锁屏页、
/// 修改 / 关闭密码的“原密码”框），不然对话框就成了绕过退避的后门。
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    failures: u32,
    blocked_until: Option<Instant>,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// 连续输错的次数。
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// 还要等多久；`None` = 现在可以试。
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.blocked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    /// 记一次输错，返回接下来要等多久（可能为零）。
    pub fn record_failure(&mut self, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let delay = delay_after(self.failures);
        self.blocked_until = (!delay.is_zero()).then(|| now + delay);
        delay
    }

    /// 输对了：计数清零。
    pub fn record_success(&mut self) {
        self.failures = 0;
        self.blocked_until = None;
    }
}

// ---------------------------------------------------------------------------
// 查看 / 复制已保存密码前的验证
// ---------------------------------------------------------------------------

/// 验证通过之后多久内不再要求（查看 / 复制已保存的密码）。
pub const REVEAL_WINDOW: Duration = Duration::from_secs(5 * 60);

/// “查看 / 复制已保存密码”的验证状态：最近一次验证通过的时间。
///
/// 只在锁屏启用时才有意义（没启用锁屏的用户行为不变）；整窗锁屏 / 解锁都会 [`Self::reset`]，之后
/// 第一次查看 / 复制又要验证。时间由调用方传进来，测试不用真等。输错的退避不在这里：它和锁屏页
/// 共用同一份 [`Backoff`]，由界面层持有。
///
/// 威胁模型：防旁人看屏幕 / 顺手操作，不防本机上的恶意程序（它能直接读写库和进程内存）。
#[derive(Debug, Clone, Copy, Default)]
pub struct RevealGate {
    verified_at: Option<Instant>,
}

impl RevealGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// 窗口期内吗。
    pub fn is_fresh(&self, now: Instant) -> bool {
        self.verified_at
            .is_some_and(|at| now.saturating_duration_since(at) < REVEAL_WINDOW)
    }

    /// 现在查看 / 复制要不要先验证：锁屏启用着，并且窗口期已过（或从没验证过）。
    pub fn needs_verification(&self, lock_enabled: bool, now: Instant) -> bool {
        lock_enabled && !self.is_fresh(now)
    }

    /// 刚验证通过。
    pub fn mark_verified(&mut self, now: Instant) {
        self.verified_at = Some(now);
    }

    /// 作废（锁屏 / 解锁时）。
    pub fn reset(&mut self) {
        self.verified_at = None;
    }
}

// ---------------------------------------------------------------------------
// 忘记密码：用 Windows 账户验证
// ---------------------------------------------------------------------------

/// 一次系统层面的身份验证的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsCheck {
    /// 验证通过。
    Verified,
    /// 验证了，但没通过（输错 PIN / 密码、重试次数用完）。
    Failed,
    /// 用户自己取消了。
    Cancelled,
    /// 这条路走不了（设备不支持 Windows Hello、没配置、被策略禁用、系统接口出错）。
    Unavailable,
}

/// 两条验证路径。真实实现在 Windows 上调系统接口（`os_windows`），测试用桩——
/// 测试和自动化不能真的弹出 Windows Hello / 凭据框。
pub trait OsVerifier {
    /// Windows Hello / PIN（`UserConsentVerifier`）。
    fn windows_hello(&self, message: &str) -> OsCheck;
    /// 退一步：弹系统凭据框，用 `LogonUserW` 核对当前用户的 Windows 密码。
    fn windows_credentials(&self, message: &str) -> OsCheck;
}

/// 先 Windows Hello；设备不支持才退到凭据框。用户取消或验证没通过就停在那里，不换另一条路再问一遍
/// （换一条路问等于让人连试两次）。两条路都走不了：`Unavailable`。
pub fn verify_os_user(verifier: &dyn OsVerifier, message: &str) -> OsCheck {
    match verifier.windows_hello(message) {
        OsCheck::Unavailable => verifier.windows_credentials(message),
        decided => decided,
    }
}

#[cfg(windows)]
mod os_windows;
#[cfg(windows)]
pub use os_windows::WindowsVerifier;

#[cfg(test)]
mod tests;
