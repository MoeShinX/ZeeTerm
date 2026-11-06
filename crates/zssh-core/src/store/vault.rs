//! Linux 本机秘密：随机资料库密钥 + XChaCha20-Poly1305。
//!
//! Windows 继续用 DPAPI；Linux 没有等价的内核用户绑定能力，所以按方案 3.2：
//! - 生成 32 字节随机资料库密钥；
//! - 密文留在 SQLite 的 `secret_blob` 列，格式带 magic、版本和随机 nonce；
//! - 密钥的存放顺序（`load_or_create`）：
//!   1. 数据目录里已有 `vault.key` → 一直用它，不再问 Secret Service（否则会同时存在两把钥匙，
//!      哪份密文用哪把说不清）；
//!   2. Secret Service（GNOME Keyring / KWallet）里已有条目 → 用它；
//!   3. Secret Service 可用但还没有条目 → 在里面新建；
//!   4. Secret Service **不存在**（没有 D-Bus 会话 / 没人提供这个服务）→ 在数据目录建 `vault.key`
//!      （0600，原子写入），并把"密钥在本机文件里"记下来给界面显示。
//!
//! 整段"读文件 → 读钥匙串 → 必要时新建写入"在数据目录里的 `vault.lock` 排他锁下进行：两个进程同时
//! 第一次启动时，后拿到锁的会读到先写进去的密钥，而不是各建一把、后写的覆盖先写的。
//!
//! 钥匙串**被锁住 / 用户拒绝 / 超时 / 别的错误**一律明确报错，绝不降级到文件：否则用户在解锁
//! 对话框里点一次"取消"，应用就悄悄把密钥换成一份弱保护的文件，之后又和钥匙串里那把分叉。
//! 单元测试用固定测试密钥验证加解密本身；生产路径始终走 `load_or_create`。

use std::path::Path;
#[cfg(not(test))]
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngExt as _;
use zeroize::Zeroizing;

use crate::error::{reason_keys as rk, Reason, ZsshError};
use crate::store::db::Database;

/// 密文头：8 字节 magic（含格式版本）。
const MAGIC: &[u8; 8] = b"ZSVLT01\n";
/// XChaCha20-Poly1305 的 nonce 长度。
const NONCE_LEN: usize = 24;
/// 认证标签长度。
const TAG_LEN: usize = 16;
/// 资料库密钥长度。
const KEY_LEN: usize = 32;
/// 后备密钥文件名（放在应用数据目录里）。
const KEY_FILE: &str = "vault.key";
/// 标记文件：这个数据目录的密钥曾经放在系统钥匙串里（空文件，只看存在与否）。
///
/// 为什么需要：平时用钥匙串的用户，哪天从 SSH 登录、没有桌面会话时启动了 ZeeTerm，Secret Service
/// "不存在"。如果这时照常建 `vault.key`，以后文件优先，钥匙串里那把加密的所有旧密码就再也解不开了。
/// 有这个标记时，钥匙串连不上一律报错，不建文件。
const KEYRING_MARKER: &str = "vault.keyring";
/// 数据目录里的锁文件：`load_or_create` 整段持有它的排他 `flock`（内容恒为空）。
const LOCK_FILE: &str = "vault.lock";
/// Secret Service 里的产品命名空间与条目名。
#[cfg(not(test))]
const SERVICE: &str = "app.zeeterm";
#[cfg(not(test))]
const ENTRY: &str = "local-vault-key";
/// 等 Secret Service 的上限。用户可能要在钥匙串解锁对话框里输密码，15 秒不够。
#[cfg(not(test))]
const LOAD_TIMEOUT: Duration = Duration::from_secs(120);

type Key = Zeroizing<[u8; KEY_LEN]>;

/// 资料库密钥现在的状态（设置页显示用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// 还没取到（启动预热还在跑 / 还没人用过）。
    Loading,
    /// 已取到，放在这里。
    Ready(KeyBackend),
    /// 最近一次取密钥失败了（钥匙串被锁住 / 授权被取消 / 服务出错）；下次用到时会重试。
    Failed,
    /// 钥匙串在，但里面找不到 ZeeTerm 的密钥（换了桌面环境 / 换了用户 / 重置过钥匙串）；
    /// 已保存的密码解不开。只有用户明确"重置已保存的密码"才会再建一把。
    KeyMissing,
}
type LoadResult = Result<(Key, KeyBackend), Reason>;

/// 资料库密钥现在放在哪里（界面显示用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyBackend {
    /// 系统钥匙串（Secret Service）。
    SystemKeyring,
    /// 数据目录里的 `vault.key`，只靠文件权限保护。
    LocalFile,
}

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 应用数据目录。由应用启动时传进来（core 不自己猜路径）；没传时只走 Secret Service。
#[cfg(not(test))]
static DATA_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 告诉资料库后备密钥文件放哪个目录。必须在第一次加解密之前调用；密钥一旦加载成功就不再看它。
#[cfg_attr(test, allow(unused_variables))]
pub fn init(data_dir: &Path) {
    #[cfg(not(test))]
    {
        *lock(&DATA_DIR) = Some(data_dir.to_path_buf());
    }
}

/// 当前资料库密钥的状态；设置页据此显示"密钥在系统钥匙串里 / 在本机文件里"。不会触发加载。
pub fn key_state() -> KeyState {
    loader().state()
}

/// 启动后在后台把密钥取出来缓存好。
///
/// 为什么：界面线程上有一批同步的保存 / 查看密码处理函数（设置页的 AI 接口列表甚至在渲染时就要解密），
/// 它们第一次调用 `protect/unprotect` 时才去问钥匙串，就会把窗口卡到钥匙串回应为止（要输解锁密码时
/// 可能是几十秒）。启动时先在后台预热，这些调用到的时候密钥已经在缓存里了。
pub fn warm_up() {
    let spawned = std::thread::Builder::new()
        .name("zeeterm-vault-warmup".to_owned())
        .spawn(|| match vault_key() {
            Ok(_) => tracing::info!(state = ?key_state(), "资料库密钥已就绪"),
            Err(err) => tracing::warn!(%err, "预热资料库密钥失败，稍后用到时会重试"),
        });
    if let Err(err) = spawned {
        tracing::warn!(%err, "启动资料库密钥预热线程失败");
    }
}

/// 重置本机已保存的密码：清掉所有凭据 / 代理（含 AI 接口 API Key）的密文和 `vault.keyring` 标记，
/// 下次取密钥时在钥匙串里新建一把。给 [`KeyState::KeyMissing`] 时界面上的「重置已保存的密码」用。
///
/// 只清 SQLite 里的 `secret_blob`——这些是原来密钥的密文，没有密钥也读不出来，所以全程不解密、不记日志。
/// 同步账号、备份文件、远程编辑草稿等也用资料库密钥加密，但它们解不开时各自有"损坏 / 无法解密"的提示，
/// 不在这里清。
pub fn reset_saved_credentials(db: &Database) -> Result<(), ZsshError> {
    reset_in(current_data_dir().as_deref(), db)?;
    // 失败有 5 秒冷却：不清掉的话，用户刚点完重置、马上用到密钥时还会拿到旧的"找不到"错误。
    loader().forget_failure();
    Ok(())
}

fn current_data_dir() -> Option<std::path::PathBuf> {
    #[cfg(not(test))]
    {
        lock(&DATA_DIR).clone()
    }
    #[cfg(test)]
    {
        None
    }
}

fn reset_in(dir: Option<&Path>, db: &Database) -> Result<(), ZsshError> {
    // 和 `load_or_create` 同一把锁：重置途中，另一个刚启动的实例不能插进来建密钥。
    let _lock = dir.and_then(lock_data_dir);
    db.with_conn_write(|conn| {
        for table in ["identities", "proxies"] {
            conn.execute(
                &format!("UPDATE {table} SET secret_blob = NULL WHERE secret_blob IS NOT NULL"),
                [],
            )?;
        }
        Ok(())
    })?;
    if let Some(dir) = dir {
        match std::fs::remove_file(dir.join(KEYRING_MARKER)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(ZsshError::Secret(
                    Reason::new(rk::VAULT_UNAVAILABLE, format!("删除钥匙串标记失败：{err}"))
                        .with("cause", err),
                ));
            }
        }
    }
    Ok(())
}

/// 加密一段敏感信息。
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    let key = vault_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|err| {
        ZsshError::Secret(
            Reason::new(
                rk::VAULT_CRYPTO_FAILED,
                format!("初始化资料库密钥失败：{err}"),
            )
            .with("cause", err),
        )
    })?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill(&mut nonce);
    let sealed = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: MAGIC,
            },
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(rk::VAULT_CRYPTO_FAILED, format!("加密失败：{err}")).with("cause", err),
            )
        })?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// 解密 [`protect`] 写出来的密文。
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    if blob.len() < MAGIC.len() + NONCE_LEN + TAG_LEN || &blob[..MAGIC.len()] != MAGIC {
        return Err(ZsshError::Secret(
            Reason::new(
                rk::VAULT_CORRUPT,
                "不是 ZeeTerm Linux 资料库密文（格式不识别）",
            )
            .with("cause", "unrecognized ciphertext format"),
        ));
    }
    let key = vault_key()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|err| {
        ZsshError::Secret(
            Reason::new(
                rk::VAULT_CRYPTO_FAILED,
                format!("初始化资料库密钥失败：{err}"),
            )
            .with("cause", err),
        )
    })?;
    let nonce = XNonce::from_slice(&blob[MAGIC.len()..MAGIC.len() + NONCE_LEN]);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &blob[MAGIC.len() + NONCE_LEN..],
                aad: MAGIC,
            },
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(rk::VAULT_CORRUPT, format!("资料库密文解不开：{err}"))
                    .with("cause", err),
            )
        })
}

/// 取资料库密钥；第一次调用会去找密钥（可能访问 Secret Service），之后缓存到进程结束。
fn vault_key() -> Result<Key, ZsshError> {
    // 单元测试用固定密钥，只验证 AEAD 本身；生产构建没有这条路径。
    #[cfg(test)]
    {
        Ok(Zeroizing::new([0x5a; KEY_LEN]))
    }

    #[cfg(not(test))]
    {
        loader()
            .get(LOAD_TIMEOUT, || {
                let dir = lock(&DATA_DIR).clone();
                load_or_create(dir.as_deref(), &SecretServiceStore)
            })
            .map_err(ZsshError::Secret)
    }
}

// ---------------------------------------------------------------------------
// 串行化：从"查缓存"到"读或建密钥并写入缓存"只能有一个创建者
// ---------------------------------------------------------------------------

/// 一次正在进行的加载。所有在它进行期间来要密钥的调用都等同一个结果。
struct Flight {
    done: Mutex<Option<LoadResult>>,
    ready: Condvar,
}

struct LoaderState {
    key: Option<(Key, KeyBackend)>,
    flight: Option<Arc<Flight>>,
    /// 最近一轮加载失败的时间和原因（成功后清掉）。
    failed: Option<(Instant, Reason)>,
}

/// 密钥缓存 + 加载去重。
///
/// 为什么不是"各线程各自去读 / 建、后写的覆盖先写的"：两个线程同时第一次加密，都看到"没有条目"、
/// 各生成一把，后写的把先写的覆盖掉，先加密的那份密文就永远解不开了。这里同一时刻最多一个工作线程在
/// 加载，其他调用只等它的结果。超时只是**调用方**不等了——工作线程还在跑（可能正卡在解锁对话框），
/// 下一次调用接着等同一个工作线程，绝不会再起第二个创建者。
struct KeyLoader {
    state: Mutex<LoaderState>,
    /// 失败后这段时间内的新调用直接返回上次的错误，不再去问钥匙串。
    ///
    /// 为什么：界面线程上有不少同步调用（保存、查看、渲染设置页都会解密）。钥匙串被锁住时每次尝试都会
    /// 弹解锁框并一直等到用户回应；用户点了"取消"之后，紧接着的几次调用不该再各弹一次、各卡一次界面。
    retry_after_failure: Duration,
}

/// 失败后多久内不重试（见 [`KeyLoader::retry_after_failure`]）。
#[cfg(not(test))]
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl KeyLoader {
    const fn new(retry_after_failure: Duration) -> Self {
        Self {
            state: Mutex::new(LoaderState {
                key: None,
                flight: None,
                failed: None,
            }),
            retry_after_failure,
        }
    }

    /// 忘掉最近一次失败：下一次调用马上重新加载，不等冷却期（重置已保存的密码之后用）。
    fn forget_failure(&self) {
        lock(&self.state).failed = None;
    }

    fn state(&self) -> KeyState {
        let state = lock(&self.state);
        match (&state.key, &state.failed) {
            (Some((_, backend)), _) => KeyState::Ready(*backend),
            (None, Some((_, reason))) if reason.key() == rk::VAULT_KEY_MISSING => {
                KeyState::KeyMissing
            }
            (None, Some(_)) => KeyState::Failed,
            (None, None) => KeyState::Loading,
        }
    }

    /// 取密钥。`source` 只在没有缓存、也没有正在进行的加载时才会被运行（在新开的工作线程里）。
    fn get<F>(&'static self, timeout: Duration, source: F) -> Result<Key, Reason>
    where
        F: FnOnce() -> LoadResult + Send + 'static,
    {
        let flight = {
            let mut state = lock(&self.state);
            if let Some((key, _)) = &state.key {
                return Ok(key.clone());
            }
            if state.flight.is_none() {
                if let Some((at, reason)) = &state.failed {
                    if at.elapsed() < self.retry_after_failure {
                        return Err(reason.clone());
                    }
                }
            }
            match &state.flight {
                Some(flight) => Arc::clone(flight),
                None => {
                    let flight = Arc::new(Flight {
                        done: Mutex::new(None),
                        ready: Condvar::new(),
                    });
                    let worker_flight = Arc::clone(&flight);
                    // 在持有 state 锁时起线程、成功后才登记：工作线程收尾时要拿同一把锁，
                    // 所以不会在我们登记之前就把 flight 清掉。
                    std::thread::Builder::new()
                        .name("zeeterm-vault-key".to_owned())
                        .spawn(move || self.run_flight(&worker_flight, source))
                        .map_err(|err| {
                            Reason::new(rk::VAULT_UNAVAILABLE, format!("启动密钥线程失败：{err}"))
                                .with("cause", err)
                        })?;
                    state.flight = Some(Arc::clone(&flight));
                    flight
                }
            }
        };

        let guard = lock(&flight.done);
        let (guard, _) = flight
            .ready
            .wait_timeout_while(guard, timeout, |done| done.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(Ok((key, _))) => Ok(key.clone()),
            Some(Err(reason)) => Err(reason.clone()),
            None => Err(Reason::new(
                rk::VAULT_TIMEOUT,
                "等待系统钥匙串超时（可能在等你输入解锁密码，或服务没有回应）",
            )),
        }
    }

    fn run_flight<F>(&self, flight: &Flight, source: F)
    where
        F: FnOnce() -> LoadResult,
    {
        // source 里 panic 也要收尾：否则 flight 永远不结束，之后所有调用都白等到超时。
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(source)).unwrap_or_else(|_| {
                Err(
                    Reason::new(rk::VAULT_UNAVAILABLE, "读取资料库密钥时内部出错")
                        .with("cause", "internal error"),
                )
            });
        {
            // 先登记结果、清掉 flight，再通知等待者：这样新来的调用要么看到缓存，要么（失败时）
            // 可以开始新一轮，不会撞上一个已经结束却还挂着的 flight。
            let mut state = lock(&self.state);
            match &result {
                Ok((key, backend)) => {
                    state.key = Some((key.clone(), *backend));
                    state.failed = None;
                }
                Err(reason) => state.failed = Some((Instant::now(), reason.clone())),
            }
            state.flight = None;
        }
        *lock(&flight.done) = Some(result);
        flight.ready.notify_all();
    }
}

fn loader() -> &'static KeyLoader {
    static LOADER: OnceLock<KeyLoader> = OnceLock::new();
    #[cfg(not(test))]
    let retry = RETRY_AFTER_FAILURE;
    #[cfg(test)]
    let retry = Duration::ZERO;
    LOADER.get_or_init(|| KeyLoader::new(retry))
}

// ---------------------------------------------------------------------------
// 找或建密钥
// ---------------------------------------------------------------------------

/// 钥匙串访问失败的类别：决定能不能降级到文件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureKind {
    /// 这台机器上没有 Secret Service（没有 D-Bus 会话 / 没人提供服务）。**只有这一类**可以降级到文件。
    Absent,
    /// 钥匙串被锁住 / 解锁被取消 / 访问被拒绝。降级会让用户点一次"取消"就悄悄换成文件，所以报错。
    Locked,
    /// 其他错误，不明就报错。
    Other,
}

struct StoreError {
    kind: FailureKind,
    detail: String,
}

/// 对 Secret Service 里那一个条目的读写（测试里换成替身）。
trait KeyringStore {
    fn get(&self) -> Result<Option<String>, StoreError>;
    fn set(&self, value: &str) -> Result<(), StoreError>;
}

fn load_or_create(dir: Option<&Path>, store: &dyn KeyringStore) -> LoadResult {
    // 持锁到本函数结束：读到"没有"和写进去之间不能让别的进程插进来。
    let _lock = dir.and_then(lock_data_dir);
    load_or_create_locked(dir, store)
}

/// 拿数据目录里 `vault.lock` 的排他锁，已被别的进程拿着就阻塞等待。返回的文件离开作用域（关闭）即释放，
/// 进程崩溃时由内核释放。拿不到（目录建不了 / 只读文件系统……）时只记日志、
/// 退回到没有锁的行为：这时后备文件路径本来也写不了，不值得为锁让整个资料库不可用。
fn lock_data_dir(dir: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    let opened = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(dir.join(LOCK_FILE))
        });
    let file = match opened {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(%err, "无法创建资料库锁文件，继续（没有跨进程互斥）");
            return None;
        }
    };
    match file.lock() {
        Ok(()) => Some(file),
        Err(err) => {
            tracing::warn!(%err, "无法锁定资料库锁文件，继续（没有跨进程互斥）");
            None
        }
    }
}

fn load_or_create_locked(dir: Option<&Path>, store: &dyn KeyringStore) -> LoadResult {
    // 1. 数据目录里已有密钥文件：一直用它。
    if let Some(dir) = dir {
        if let Some(key) = read_key_file(dir)? {
            return Ok((key, KeyBackend::LocalFile));
        }
    }
    // 2/3. Secret Service。
    match store.get() {
        Ok(Some(encoded)) => {
            let key = decode_stored_key(&encoded)?;
            mark_keyring(dir);
            Ok((key, KeyBackend::SystemKeyring))
        }
        // 钥匙串在、条目不见了，而这个数据目录以前明明把密钥放进过钥匙串：不能悄悄新建一把——
        // 新建的那把解不开旧密文，回到原来的桌面环境时又会各自损坏一部分。让用户明确决定要不要重置。
        Ok(None) if dir.is_some_and(|dir| dir.join(KEYRING_MARKER).exists()) => Err(Reason::new(
            rk::VAULT_KEY_MISSING,
            "系统钥匙串里找不到 ZeeTerm 的密钥（可能换了桌面环境、换了用户，或重置过钥匙串），\
             已保存的密码解不开。为了不再造成分叉，没有自动新建密钥；需要的话可以在设置里重置已保存的密码",
        )),
        Ok(None) => {
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            rand::rng().fill(&mut *key);
            let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(*key));
            match store.set(&encoded) {
                Ok(()) => {
                    mark_keyring(dir);
                    Ok((key, KeyBackend::SystemKeyring))
                }
                // 已经读到过"条目为空"，说明服务在；这时写失败不是"没有服务"，不降级。
                Err(err) => Err(store_failure(err, false)),
            }
        }
        Err(err) if err.kind == FailureKind::Absent => match dir {
            // 以前用过钥匙串：现在只是这次连不上（SSH 登录、没有桌面会话），不能另起一把密钥。
            Some(dir) if dir.join(KEYRING_MARKER).exists() => Err(Reason::new(
                rk::VAULT_KEYRING_GONE,
                format!(
                    "已保存的密码由系统钥匙串里的密钥保护，但现在连不上钥匙串（例如通过 SSH 登录、没有桌面会话）。\
                     为了不让它们再也解不开，没有改用本机文件：{}",
                    err.detail
                ),
            )
            .with("cause", err.detail)),
            // 4. 没有 Secret Service：在数据目录建后备密钥文件。
            Some(dir) => Ok((create_key_file(dir)?, KeyBackend::LocalFile)),
            None => Err(store_failure(err, true)),
        },
        Err(err) => Err(store_failure(err, false)),
    }
}

/// 记下"这个数据目录的密钥在系统钥匙串里"（见 [`KEYRING_MARKER`]）。尽力而为：写不了只记日志——
/// 最坏情况退回到没有标记时的行为，不影响这次取到的密钥。
fn mark_keyring(dir: Option<&Path>) {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    let Some(dir) = dir else { return };
    let path = dir.join(KEYRING_MARKER);
    if path.exists() {
        return;
    }
    // 实际启动时数据目录早已建好；目录不在时（`ZSSH_DATA_DIR` 指到还没建的地方）先建，
    // 否则标记写不下去，以后一次无桌面会话的启动会建出 vault.key，把钥匙串里那把永远盖住。
    let created = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
        })
        .and_then(|file| file.sync_all());
    if let Err(err) = created {
        tracing::warn!(%err, "记录「密钥在系统钥匙串里」失败");
    }
}

fn store_failure(err: StoreError, no_dir: bool) -> Reason {
    let (key, zh) = match err.kind {
        FailureKind::Locked => (
            rk::VAULT_LOCKED,
            "系统钥匙串被锁住，或解锁 / 授权被取消（为了保护已保存的密码，没有改用本机文件）",
        ),
        FailureKind::Absent if no_dir => (
            rk::VAULT_UNAVAILABLE,
            "这台电脑上没有可用的系统钥匙串（Secret Service），也没有指定存放后备密钥的目录",
        ),
        _ => (rk::VAULT_UNAVAILABLE, "访问系统钥匙串失败"),
    };
    Reason::new(key, format!("{zh}：{}", err.detail)).with("cause", err.detail)
}

fn decode_stored_key(encoded: &str) -> Result<Key, Reason> {
    let bytes = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|err| {
                Reason::new(rk::VAULT_CORRUPT, format!("资料库密钥编码损坏：{err}"))
                    .with("cause", err)
            })?,
    );
    let key: [u8; KEY_LEN] = bytes[..].try_into().map_err(|_| {
        Reason::new(rk::VAULT_CORRUPT, "系统钥匙串里的资料库密钥长度不对")
            .with("cause", "wrong key length in the keyring")
    })?;
    Ok(Zeroizing::new(key))
}

// ---------------------------------------------------------------------------
// 后备密钥文件
// ---------------------------------------------------------------------------

/// 读数据目录里的 `vault.key`。不存在是 `Ok(None)`；存在但不是恰好 32 字节、或读不了，明确报错
/// （不能当成"没有"再去建一份：那会覆盖掉一把可能只是暂时读不了的真密钥）。
fn read_key_file(dir: &Path) -> Result<Option<Key>, Reason> {
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;

    let path = dir.join(KEY_FILE);
    let read_failed = |err: std::io::Error| {
        Reason::new(
            rk::VAULT_UNAVAILABLE,
            format!("读取本机密钥文件失败：{err}"),
        )
        .with("cause", err)
    };
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(read_failed(err)),
    };
    // 多读一个字节，才分得出"恰好 32"和"更长"。
    let mut buf = Zeroizing::new(Vec::with_capacity(KEY_LEN + 1));
    file.take(KEY_LEN as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(read_failed)?;
    let key: [u8; KEY_LEN] = buf[..].try_into().map_err(|_| {
        Reason::new(
            rk::VAULT_KEY_FILE_INVALID,
            format!(
                "本机密钥文件 {KEY_FILE} 已损坏（应恰好 {KEY_LEN} 字节）。\
                 没有擅自重建：重建会让已保存的密码永远解不开"
            ),
        )
        .with("file", KEY_FILE)
        .with("len", KEY_LEN)
    })?;
    // 别的用户能读的密钥文件等于没保护：收紧到 0600（只改我们自己建的这个文件）。
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.permissions().mode() & 0o077 != 0 {
            tracing::warn!("本机密钥文件权限过宽，已收紧为 0600");
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(Some(Zeroizing::new(key)))
}

/// 在数据目录里原子地建 `vault.key`：先写 0600 的临时文件并落盘，再用硬链接"不覆盖地"放到正式名字上。
///
/// 为什么不直接 `create_new` 正式文件：读的一方可能看到只写了一半的文件，把它当成损坏。
/// 为什么用硬链接而不是 rename：rename 会覆盖——两个进程同时第一次启动时，后来的会把先来的密钥
/// 换掉，先写进数据库的密文就解不开了。硬链接遇到已存在会失败，这时改读已经在那里的那把。
fn create_key_file(dir: &Path) -> Result<Key, Reason> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    let fail = |what: &str, err: std::io::Error| {
        Reason::new(rk::VAULT_UNAVAILABLE, format!("{what}：{err}")).with("cause", err)
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|err| fail("创建数据目录失败", err))?;

    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    rand::rng().fill(&mut *key);
    let mut suffix = [0u8; 8];
    rand::rng().fill(&mut suffix);
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let tmp = dir.join(format!(".{KEY_FILE}.{suffix}.tmp"));
    let path = dir.join(KEY_FILE);

    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&key[..])?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail("写入本机密钥文件失败", err));
    }

    let outcome = match std::fs::hard_link(&tmp, &path) {
        Ok(()) => Ok(key),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            // 别的进程抢先建好了：用它的，我们这把丢掉。
            match read_key_file(dir) {
                Ok(Some(existing)) => Ok(existing),
                Ok(None) => Err(fail("本机密钥文件状态异常", err)),
                Err(reason) => Err(reason),
            }
        }
        // 文件系统不支持硬链接：退而用 rename（有极小的并发窗口，单实例保证下不会撞上）。
        Err(_) => std::fs::rename(&tmp, &path)
            .map(|()| key)
            .map_err(|err| fail("写入本机密钥文件失败", err)),
    };
    let _ = std::fs::remove_file(&tmp);
    if outcome.is_ok() {
        // 目录项也落盘，崩溃后不会出现"数据库里有密文、密钥文件却没了"。
        if let Ok(dir_file) = std::fs::File::open(dir) {
            let _ = dir_file.sync_all();
        }
    }
    outcome
}

// ---------------------------------------------------------------------------
// 真实的 Secret Service
// ---------------------------------------------------------------------------

/// keyring 的 Entry 是同步阻塞 API：只在 `KeyLoader` 的工作线程里调用，
/// 不能放在 GPUI 主线程或 Tokio worker 上等 D-Bus。
#[cfg(not(test))]
struct SecretServiceStore;

#[cfg(not(test))]
impl SecretServiceStore {
    fn entry() -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(SERVICE, ENTRY).map_err(from_keyring)
    }
}

#[cfg(not(test))]
impl KeyringStore for SecretServiceStore {
    fn get(&self) -> Result<Option<String>, StoreError> {
        match Self::entry()?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(from_keyring(err)),
        }
    }

    fn set(&self, value: &str) -> Result<(), StoreError> {
        Self::entry()?.set_password(value).map_err(from_keyring)
    }
}

#[cfg_attr(test, allow(dead_code))]
fn from_keyring(err: keyring::Error) -> StoreError {
    StoreError {
        kind: classify(&err),
        detail: err.to_string(),
    }
}

/// 把 keyring 的错误分成"没有服务 / 被锁或拒绝 / 其他"。
///
/// keyring 把 Secret Service 的错误装在 `PlatformFailure` / `NoStorageAccess` 的盒子里；
/// 这里按 `secret_service::Error` 的变体判断，认不出来的一律算 `Other`（报错而不是降级）。
fn classify(err: &keyring::Error) -> FailureKind {
    use secret_service::Error as Ss;

    match err {
        keyring::Error::NoStorageAccess(_) => FailureKind::Locked,
        keyring::Error::PlatformFailure(inner) => match inner.downcast_ref::<Ss>() {
            Some(Ss::Unavailable) => FailureKind::Absent,
            Some(Ss::Locked | Ss::Prompt | Ss::NoResult) => FailureKind::Locked,
            Some(Ss::Zbus(inner)) => classify_bus_error(&format!("{inner:?}")),
            Some(Ss::ZbusFdo(inner)) => classify_bus_error(&format!("{inner:?}")),
            _ => FailureKind::Other,
        },
        _ => FailureKind::Other,
    }
}

/// D-Bus 层的错误里，"总线上没有人提供 org.freedesktop.secrets"（也包括总线 socket 连不上）算没有服务。
fn classify_bus_error(debug: &str) -> FailureKind {
    const MISSING: [&str; 4] = [
        "ServiceUnknown",
        "NameHasNoOwner",
        "ServiceNotFound",
        "ConnectionRefused",
    ];
    if MISSING.iter().any(|needle| debug.contains(needle)) {
        FailureKind::Absent
    } else {
        FailureKind::Other
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- 加解密本身（固定测试密钥） ----

    #[test]
    fn roundtrip_restores_plaintext() {
        let plaintext = "一段带中文的密码 p@ssw0rd".as_bytes();
        let blob = protect(plaintext).expect("加密应该成功");
        assert_ne!(blob, plaintext);
        assert_eq!(unprotect(&blob).expect("解密应该成功"), plaintext);
    }

    #[test]
    fn tampering_and_foreign_formats_are_refused() {
        let mut blob = protect(b"secret").expect("加密");
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(unprotect(&blob).is_err(), "改动密文必须解不开");
        assert!(
            unprotect(b"not-a-vault-blob").is_err(),
            "旧格式/垃圾要明确拒绝"
        );
    }

    #[test]
    fn two_encryptions_differ_because_of_the_nonce() {
        let first = protect(b"same").expect("加密");
        let second = protect(b"same").expect("加密");
        assert_ne!(first, second, "同一明文两次加密不应相同");
    }

    // ---- 钥匙串替身 ----

    enum Mock {
        /// 已有条目。
        Has(String),
        /// 可用但还没有条目；写入会存下来。
        Empty,
        /// 读取失败。
        GetFails(FailureKind),
        /// 读到空，但写入失败。
        SetFails(FailureKind),
    }

    struct MockStore {
        mode: Mock,
        gets: AtomicUsize,
        stored: Mutex<Option<String>>,
    }

    impl MockStore {
        fn new(mode: Mock) -> Self {
            Self {
                mode,
                gets: AtomicUsize::new(0),
                stored: Mutex::new(None),
            }
        }
    }

    impl KeyringStore for MockStore {
        fn get(&self) -> Result<Option<String>, StoreError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            match &self.mode {
                Mock::Has(value) => Ok(Some(value.clone())),
                Mock::Empty | Mock::SetFails(_) => Ok(None),
                Mock::GetFails(kind) => Err(StoreError {
                    kind: *kind,
                    detail: "mock".to_owned(),
                }),
            }
        }

        fn set(&self, value: &str) -> Result<(), StoreError> {
            if let Mock::SetFails(kind) = &self.mode {
                return Err(StoreError {
                    kind: *kind,
                    detail: "mock".to_owned(),
                });
            }
            *lock(&self.stored) = Some(value.to_owned());
            Ok(())
        }
    }

    fn encoded(byte: u8) -> String {
        base64::engine::general_purpose::STANDARD.encode([byte; KEY_LEN])
    }

    // ---- 后备文件 ----

    #[test]
    fn missing_service_falls_back_to_a_0600_key_file_of_32_bytes() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = MockStore::new(Mock::GetFails(FailureKind::Absent));
        let (key, backend) = load_or_create(Some(dir.path()), &store).expect("应降级到文件");
        assert_eq!(backend, KeyBackend::LocalFile);

        let path = dir.path().join(KEY_FILE);
        let on_disk = std::fs::read(&path).expect("密钥文件应该存在");
        assert_eq!(on_disk.len(), KEY_LEN);
        assert_eq!(&on_disk[..], &key[..]);
        let mode = std::fs::metadata(&path)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "密钥文件必须是 0600");
        // 临时文件不能留下。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("读目录")
            .filter_map(Result::ok)
            // 锁文件是故意留在那里的（见 `lock_data_dir`），不算残留。
            .filter(|entry| entry.file_name() != KEY_FILE && entry.file_name() != LOCK_FILE)
            .collect();
        assert!(leftovers.is_empty(), "不该留临时文件：{leftovers:?}");
    }

    #[test]
    fn an_existing_key_file_wins_and_the_keyring_is_not_asked() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(dir.path().join(KEY_FILE), [7u8; KEY_LEN]).expect("写密钥文件");
        let store = MockStore::new(Mock::Has(encoded(9)));
        let (key, backend) = load_or_create(Some(dir.path()), &store).expect("读文件");
        assert_eq!(backend, KeyBackend::LocalFile);
        assert_eq!(&key[..], &[7u8; KEY_LEN]);
        assert_eq!(
            store.gets.load(Ordering::SeqCst),
            0,
            "有文件就不该再问钥匙串"
        );
    }

    #[test]
    fn a_wrong_sized_key_file_is_a_clear_error_and_is_not_replaced() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join(KEY_FILE);
        for bad in [&b""[..], &[1u8; 31][..], &[1u8; 33][..], &[1u8; 4096][..]] {
            std::fs::write(&path, bad).expect("写坏文件");
            let store = MockStore::new(Mock::GetFails(FailureKind::Absent));
            let err = load_or_create(Some(dir.path()), &store)
                .err()
                .unwrap_or_else(|| panic!("损坏文件必须报错"));
            assert_eq!(err.key(), rk::VAULT_KEY_FILE_INVALID);
            assert_eq!(std::fs::read(&path).expect("读回"), bad, "不能覆盖坏文件");
            assert_eq!(store.gets.load(Ordering::SeqCst), 0, "不能悄悄改用钥匙串");
        }
    }

    #[test]
    fn a_world_readable_key_file_is_tightened_to_0600() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join(KEY_FILE);
        std::fs::write(&path, [3u8; KEY_LEN]).expect("写密钥文件");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("改权限");
        assert!(read_key_file(dir.path()).expect("读").is_some());
        let mode = std::fs::metadata(&path)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn creating_the_key_file_never_replaces_one_that_appeared_first() {
        let dir = tempfile::tempdir().expect("临时目录");
        let first = create_key_file(dir.path()).expect("第一次创建");
        let second = create_key_file(dir.path()).expect("第二次应读到第一把");
        assert_eq!(&first[..], &second[..]);
        assert_eq!(
            std::fs::read(dir.path().join(KEY_FILE)).expect("读"),
            first.to_vec()
        );
    }

    #[test]
    fn many_threads_creating_the_key_file_all_end_up_with_the_same_key() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().to_path_buf();
        let keys: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || create_key_file(&path).map(|key| key.to_vec()))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("线程").expect("创建"))
            .collect();
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "各线程拿到的密钥必须一致"
        );
    }

    // ---- 降级与不降级 ----

    #[test]
    fn locked_denied_and_unknown_failures_never_fall_back_to_a_file() {
        for kind in [FailureKind::Locked, FailureKind::Other] {
            let dir = tempfile::tempdir().expect("临时目录");
            let store = MockStore::new(Mock::GetFails(kind));
            let err = load_or_create(Some(dir.path()), &store)
                .err()
                .unwrap_or_else(|| panic!("必须报错"));
            let expected = if kind == FailureKind::Locked {
                rk::VAULT_LOCKED
            } else {
                rk::VAULT_UNAVAILABLE
            };
            assert_eq!(err.key(), expected);
            assert!(!dir.path().join(KEY_FILE).exists(), "不能偷偷建文件");
        }
    }

    #[test]
    fn a_failed_write_after_an_empty_read_does_not_fall_back_either() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = MockStore::new(Mock::SetFails(FailureKind::Absent));
        assert!(load_or_create(Some(dir.path()), &store).is_err());
        assert!(!dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn once_the_keyring_was_used_a_missing_service_never_creates_a_key_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        // 先在有钥匙串的会话里用过（读到已有条目、或新建条目都会留下标记）。
        let existing = MockStore::new(Mock::Has(encoded(4)));
        load_or_create(Some(dir.path()), &existing).expect("读条目");
        assert!(
            dir.path().join(KEYRING_MARKER).exists(),
            "用过钥匙串要留标记"
        );

        // 之后某次没有 Secret Service（SSH 登录）：报错，不建文件，旧密文还能等钥匙串回来再解。
        let absent = MockStore::new(Mock::GetFails(FailureKind::Absent));
        let err = load_or_create(Some(dir.path()), &absent)
            .err()
            .unwrap_or_else(|| panic!("必须报错"));
        assert_eq!(err.key(), rk::VAULT_KEYRING_GONE);
        assert!(!dir.path().join(KEY_FILE).exists(), "不能另起一把密钥");

        // 新建条目同样留标记。
        let fresh = tempfile::tempdir().expect("临时目录");
        load_or_create(Some(fresh.path()), &MockStore::new(Mock::Empty)).expect("建条目");
        assert!(fresh.path().join(KEYRING_MARKER).exists());
    }

    #[test]
    fn without_a_data_dir_a_missing_service_is_an_error() {
        let store = MockStore::new(Mock::GetFails(FailureKind::Absent));
        let err = load_or_create(None, &store)
            .err()
            .unwrap_or_else(|| panic!("必须报错"));
        assert_eq!(err.key(), rk::VAULT_UNAVAILABLE);
    }

    #[test]
    fn keyring_entries_are_used_and_created_when_the_service_is_there() {
        let existing = MockStore::new(Mock::Has(encoded(4)));
        let (key, backend) = load_or_create(None, &existing).expect("读条目");
        assert_eq!(
            (&key[..], backend),
            (&[4u8; KEY_LEN][..], KeyBackend::SystemKeyring)
        );

        let empty = MockStore::new(Mock::Empty);
        let (key, backend) = load_or_create(None, &empty).expect("建条目");
        assert_eq!(backend, KeyBackend::SystemKeyring);
        let stored = lock(&empty.stored).clone().expect("应写进钥匙串");
        assert_eq!(
            stored,
            base64::engine::general_purpose::STANDARD.encode(*key)
        );

        let broken = MockStore::new(Mock::Has("!!!".to_owned()));
        assert_eq!(
            load_or_create(None, &broken)
                .err()
                .unwrap_or_else(|| panic!("坏编码"))
                .key(),
            rk::VAULT_CORRUPT
        );
        let short = MockStore::new(Mock::Has(
            base64::engine::general_purpose::STANDARD.encode([1u8; 5]),
        ));
        assert_eq!(
            load_or_create(None, &short)
                .err()
                .unwrap_or_else(|| panic!("太短"))
                .key(),
            rk::VAULT_CORRUPT
        );
    }

    #[test]
    fn keyring_errors_are_classified_by_what_they_mean() {
        let boxed = |err: secret_service::Error| keyring::Error::PlatformFailure(Box::new(err));
        assert_eq!(
            classify(&boxed(secret_service::Error::Unavailable)),
            FailureKind::Absent
        );
        assert_eq!(
            classify(&boxed(secret_service::Error::Locked)),
            FailureKind::Locked
        );
        assert_eq!(
            classify(&boxed(secret_service::Error::Prompt)),
            FailureKind::Locked
        );
        assert_eq!(
            classify(&boxed(secret_service::Error::Crypto("x"))),
            FailureKind::Other
        );
        assert_eq!(
            classify(&keyring::Error::NoStorageAccess(Box::new(
                secret_service::Error::Locked
            ))),
            FailureKind::Locked
        );
        assert_eq!(classify(&keyring::Error::NoEntry), FailureKind::Other);
        assert_eq!(
            classify(&keyring::Error::PlatformFailure("boom".into())),
            FailureKind::Other
        );
        assert_eq!(
            classify_bus_error("MethodError(\"org.freedesktop.DBus.Error.ServiceUnknown\")"),
            FailureKind::Absent
        );
        assert_eq!(
            classify_bus_error("Failure(\"disk full\")"),
            FailureKind::Other
        );
    }

    // ---- 首次创建竞态（跨进程靠数据目录里的 flock；各线程各自打开锁文件，flock 照样互斥） ----

    /// 几个"进程"共用的钥匙串替身：`get` 之后故意拖一会儿，没有锁时各方都会读到"空"。
    struct SharedStore {
        stored: Arc<Mutex<Option<String>>>,
    }

    impl KeyringStore for SharedStore {
        fn get(&self) -> Result<Option<String>, StoreError> {
            let seen = lock(&self.stored).clone();
            std::thread::sleep(Duration::from_millis(80));
            Ok(seen)
        }

        fn set(&self, value: &str) -> Result<(), StoreError> {
            *lock(&self.stored) = Some(value.to_owned());
            Ok(())
        }
    }

    #[test]
    fn several_first_starts_on_one_data_dir_end_up_with_the_same_key() {
        let dir = tempfile::tempdir().expect("临时目录");
        let stored = Arc::new(Mutex::new(None));
        let keys: Vec<_> = (0..4)
            .map(|_| {
                let path = dir.path().to_path_buf();
                let store = SharedStore {
                    stored: Arc::clone(&stored),
                };
                std::thread::spawn(move || {
                    load_or_create(Some(&path), &store).map(|(key, _)| key.to_vec())
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("线程").expect("加载"))
            .collect();
        assert!(
            keys.windows(2).all(|pair| pair[0] == pair[1]),
            "各进程拿到的必须是同一把密钥"
        );
        let in_keyring = lock(&stored).clone().expect("钥匙串里应有条目");
        assert_eq!(
            in_keyring,
            base64::engine::general_purpose::STANDARD.encode(&keys[0])
        );
        let mode = std::fs::metadata(dir.path().join(LOCK_FILE))
            .expect("锁文件")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "锁文件必须是 0600");
    }

    // ---- 标记在、条目没了 ----

    #[test]
    fn a_marker_with_a_missing_entry_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(dir.path().join(KEYRING_MARKER), b"").expect("写标记");
        let store = MockStore::new(Mock::Empty);
        let err = load_or_create(Some(dir.path()), &store)
            .err()
            .unwrap_or_else(|| panic!("必须报错"));
        assert_eq!(err.key(), rk::VAULT_KEY_MISSING);
        assert!(lock(&store.stored).is_none(), "不能往钥匙串里写新密钥");
        assert!(!dir.path().join(KEY_FILE).exists(), "也不能建密钥文件");
    }

    #[test]
    fn the_key_missing_error_shows_up_as_its_own_key_state() {
        let loader = leaked_loader();
        let err = loader
            .get(Duration::from_secs(10), || {
                Err(Reason::new(rk::VAULT_KEY_MISSING, "gone"))
            })
            .err()
            .unwrap_or_else(|| panic!("失败"));
        assert_eq!(err.key(), rk::VAULT_KEY_MISSING);
        assert_eq!(loader.state(), KeyState::KeyMissing);
        loader.forget_failure();
        assert_eq!(loader.state(), KeyState::Loading);
    }

    fn insert_secret_rows(db: &Database) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO identities (name, kind, secret_blob) VALUES ('a', 'password', x'01020304')",
                [],
            )?;
            conn.execute(
                "INSERT INTO identities (name, kind, key_path) VALUES ('b', 'key_file', '/k')",
                [],
            )?;
            conn.execute(
                "INSERT INTO proxies (name, kind, host, port, username, secret_blob) \
                 VALUES ('p', 'socks5', '127.0.0.1', 1080, 'u', x'0a0b')",
                [],
            )?;
            Ok(())
        })
        .unwrap_or_else(|err| panic!("插数据：{err}"));
    }

    fn count(db: &Database, sql: &str) -> i64 {
        db.with_conn(|conn| Ok(conn.query_row(sql, [], |row| row.get(0))?))
            .unwrap_or_else(|err| panic!("查数据：{err}"))
    }

    #[test]
    fn resetting_clears_the_secrets_and_the_marker_and_then_a_new_key_is_created() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(dir.path().join(KEYRING_MARKER), b"").expect("写标记");
        let db = Database::open_in_memory().expect("内存库");
        insert_secret_rows(&db);
        let with_secret = "SELECT (SELECT count(*) FROM identities WHERE secret_blob IS NOT NULL) \
                           + (SELECT count(*) FROM proxies WHERE secret_blob IS NOT NULL)";
        assert_eq!(count(&db, with_secret), 2);

        // 重置前：报"找不到"。
        let store = MockStore::new(Mock::Empty);
        assert!(load_or_create(Some(dir.path()), &store).is_err());

        reset_in(Some(dir.path()), &db).expect("重置");
        assert_eq!(count(&db, with_secret), 0, "密文必须全部清掉");
        assert_eq!(
            count(&db, "SELECT count(*) FROM identities"),
            2,
            "只清密文，凭据记录本身还在"
        );
        assert!(!dir.path().join(KEYRING_MARKER).exists(), "标记必须删掉");

        // 重置后：新建成功，并重新留下标记。
        let (_, backend) = load_or_create(Some(dir.path()), &store).expect("应新建密钥");
        assert_eq!(backend, KeyBackend::SystemKeyring);
        assert!(lock(&store.stored).is_some());
        assert!(dir.path().join(KEYRING_MARKER).exists());

        // 标记不在时再重置一次也不报错。
        std::fs::remove_file(dir.path().join(KEYRING_MARKER)).expect("删标记");
        reset_in(Some(dir.path()), &db).expect("幂等");
    }

    #[test]
    fn the_marker_is_written_even_if_the_data_dir_does_not_exist_yet() {
        let parent = tempfile::tempdir().expect("临时目录");
        let dir = parent.path().join("not-yet");
        let (_, backend) =
            load_or_create(Some(&dir), &MockStore::new(Mock::Empty)).expect("建条目");
        assert_eq!(backend, KeyBackend::SystemKeyring);
        assert!(dir.join(KEYRING_MARKER).exists());
        let mode = std::fs::metadata(&dir).expect("目录").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "兜底建的目录必须是 0700");
    }

    // ---- 串行化 ----

    /// 测试里每个用例自己一个加载器（`get` 要 `'static`，泄漏一个小对象即可）。
    fn leaked_loader() -> &'static KeyLoader {
        Box::leak(Box::new(KeyLoader::new(Duration::ZERO)))
    }

    #[test]
    fn right_after_a_failure_new_calls_get_the_same_error_without_asking_again() {
        static ASKED: AtomicUsize = AtomicUsize::new(0);
        let loader: &'static KeyLoader =
            Box::leak(Box::new(KeyLoader::new(Duration::from_secs(60))));
        for _ in 0..3 {
            let err = loader
                .get(Duration::from_secs(10), || {
                    ASKED.fetch_add(1, Ordering::SeqCst);
                    Err(Reason::new(rk::VAULT_LOCKED, "locked"))
                })
                .err()
                .unwrap_or_else(|| panic!("失败"));
            assert_eq!(err.key(), rk::VAULT_LOCKED);
        }
        assert_eq!(
            ASKED.load(Ordering::SeqCst),
            1,
            "冷却期内不该重复去问钥匙串"
        );
    }

    #[test]
    fn many_threads_asking_at_once_cause_exactly_one_creation() {
        static CREATED: AtomicUsize = AtomicUsize::new(0);
        let loader = leaked_loader();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                std::thread::spawn(move || {
                    loader
                        .get(Duration::from_secs(10), || {
                            CREATED.fetch_add(1, Ordering::SeqCst);
                            // 拖一会儿，让其他线程都撞上"正在创建"。
                            std::thread::sleep(Duration::from_millis(100));
                            Ok((Zeroizing::new([1u8; KEY_LEN]), KeyBackend::LocalFile))
                        })
                        .map(|key| key.to_vec())
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(
                handle.join().expect("线程").expect("拿到密钥"),
                vec![1u8; KEY_LEN]
            );
        }
        assert_eq!(CREATED.load(Ordering::SeqCst), 1, "只能有一个创建者");
        assert_eq!(loader.state(), KeyState::Ready(KeyBackend::LocalFile));
    }

    #[test]
    fn a_timed_out_caller_does_not_start_a_second_creator() {
        static STARTED: AtomicUsize = AtomicUsize::new(0);
        let loader = leaked_loader();
        let slow = || {
            STARTED.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            Ok((Zeroizing::new([2u8; KEY_LEN]), KeyBackend::SystemKeyring))
        };
        let err = loader
            .get(Duration::from_millis(50), slow)
            .err()
            .unwrap_or_else(|| panic!("应超时"));
        assert_eq!(err.key(), rk::VAULT_TIMEOUT);
        // 工作线程还在跑：再来的调用接着等它，不再起新的。
        let key = loader
            .get(Duration::from_secs(10), || {
                STARTED.fetch_add(100, Ordering::SeqCst);
                Err(Reason::text("不该被调用"))
            })
            .expect("接着等同一个工作线程");
        assert_eq!(&key[..], &[2u8; KEY_LEN]);
        assert_eq!(STARTED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_load_is_shared_and_the_next_call_retries() {
        let loader = leaked_loader();
        let err = loader
            .get(Duration::from_secs(10), || {
                Err(Reason::new(rk::VAULT_LOCKED, "locked"))
            })
            .err()
            .unwrap_or_else(|| panic!("失败"));
        assert_eq!(err.key(), rk::VAULT_LOCKED);
        assert_eq!(loader.state(), KeyState::Failed);
        let key = loader
            .get(Duration::from_secs(10), || {
                Ok((Zeroizing::new([5u8; KEY_LEN]), KeyBackend::SystemKeyring))
            })
            .expect("下一次应重试并成功");
        assert_eq!(&key[..], &[5u8; KEY_LEN]);
    }

    #[test]
    fn a_panicking_source_does_not_wedge_the_loader() {
        let loader = leaked_loader();
        assert!(loader
            .get(Duration::from_secs(10), || -> LoadResult { panic!("boom") })
            .is_err());
        assert!(loader
            .get(Duration::from_secs(10), || {
                Ok((Zeroizing::new([6u8; KEY_LEN]), KeyBackend::LocalFile))
            })
            .is_ok());
    }
}
