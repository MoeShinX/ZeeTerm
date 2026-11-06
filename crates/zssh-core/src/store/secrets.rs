//! 敏感信息的加解密：Windows DPAPI（绑定当前用户）。
//!
//! 用 `CryptProtectData` 加一个固定的应用级 entropy，别的程序拿到密文也没法直接
//! 解出来。密文只进 `secret_blob`，绝不出现明文列。

#[cfg(windows)]
use crate::error::reason_keys as rk;
use crate::error::{Reason, ZsshError};

/// 应用级 entropy。这是历史值：当初产品还叫 Z-ssh 时定下的，现在产品叫 ZeeTerm，
/// 这里也保持原样。**永远不要改**——改一个字节，已有的密文就都解不开了。
#[cfg(windows)]
const ENTROPY: &[u8] = b"Z-ssh/v1/secret";

/// 加密一段敏感信息（密码、私钥口令、私钥内容）。
#[cfg(windows)]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: to_u32(plaintext.len())?,
        pbData: plaintext.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: to_u32(ENTROPY.len())?,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: 输入 blob 是本地变量，指针在整个调用期间有效；输出由 DPAPI 分配，
    // 下面用 LocalFree 释放。
    unsafe {
        CryptProtectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(
                    rk::DPAPI_PROTECT_FAILED,
                    format!("CryptProtectData 失败：{err}"),
                )
                .with("cause", &err),
            )
        })?;
    }

    Ok(unsafe { take_blob(output) })
}

/// 解密 [`protect`] 写出来的密文。
#[cfg(windows)]
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: to_u32(blob.len())?,
        pbData: blob.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: to_u32(ENTROPY.len())?,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: 同 protect。
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|err| {
            ZsshError::Secret(
                Reason::new(
                    rk::DPAPI_UNPROTECT_FAILED,
                    format!("CryptUnprotectData 失败：{err}"),
                )
                .with("cause", &err),
            )
        })?;
    }

    Ok(unsafe { take_blob(output) })
}

/// 把 DPAPI 分配的输出 blob 拷成 `Vec`，然后释放。
///
/// # Safety
/// `blob` 必须是 DPAPI 刚分配出来、还没释放的输出。
#[cfg(windows)]
unsafe fn take_blob(blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows::Win32::Foundation::LocalFree;

    let bytes = if blob.pbData.is_null() {
        Vec::new()
    } else {
        // SAFETY: DPAPI 保证 pbData 指向 cbData 个字节。
        unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec()
    };
    if !blob.pbData.is_null() {
        // SAFETY: 这块内存是 DPAPI 用 LocalAlloc 分配的，只能交给 LocalFree。
        let _ = unsafe { LocalFree(Some(windows::Win32::Foundation::HLOCAL(blob.pbData.cast()))) };
    }
    bytes
}

#[cfg(windows)]
fn to_u32(len: usize) -> Result<u32, ZsshError> {
    u32::try_from(len).map_err(|_| {
        ZsshError::Secret(
            Reason::new(rk::DATA_TOO_LONG, format!("数据太长：{len} 字节")).with("len", len),
        )
    })
}

/// 资料库密钥的保管后端。core 不依赖任何平台库，应用启动时用
/// [`set_vault_backend`] 接上钥匙串或测试用的内存实现。
#[cfg(target_os = "macos")]
#[derive(Debug)]
pub enum VaultKeyError {
    Missing,
    Cancelled,
    Locked,
    Failed {
        status: i32,
    },
    /// 条目已存在且没有被覆盖。调用方回读确认后可以继续用它。
    Exists,
}

#[cfg(target_os = "macos")]
pub trait VaultBackend: Send + Sync {
    fn load(&self, service: &str, account: &str) -> Result<Vec<u8>, VaultKeyError>;
    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), VaultKeyError>;
}

#[cfg(target_os = "macos")]
fn backend() -> &'static dyn VaultBackend {
    #[cfg(debug_assertions)]
    if memory_backend_enabled() {
        return &MISSING_BACKEND;
    }
    // 测试构建里没注册后端时用一个进程内的内存后端：单元测试不该碰用户真实的钥匙串。
    // 生产构建没有这个回落，没注册就明确失败。
    #[cfg(test)]
    {
        TEST_BACKEND.with(|slot| slot.get().unwrap_or(&TEST_MEMORY))
    }
    #[cfg(not(test))]
    {
        BACKEND
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or(&MISSING_BACKEND)
    }
}

/// 测试专用的内存钥匙串。`szzsh-core` 的其它模块（凭据、代理、AI 端点）也走它。
#[cfg(all(test, target_os = "macos"))]
pub(crate) struct TestMemoryVault;

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn test_memory() -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>
{
    static ITEMS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    > = std::sync::OnceLock::new();
    ITEMS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(all(test, target_os = "macos"))]
static TEST_MEMORY: TestMemoryVault = TestMemoryVault;

#[cfg(all(test, target_os = "macos"))]
impl VaultBackend for TestMemoryVault {
    fn load(&self, service: &str, account: &str) -> Result<Vec<u8>, VaultKeyError> {
        test_memory()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&format!("{service}\u{0}{account}"))
            .cloned()
            .ok_or(VaultKeyError::Missing)
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), VaultKeyError> {
        test_memory()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(format!("{service}\u{0}{account}"), secret.to_vec());
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
thread_local! {
    // 故障注入只影响当前测试线程；派发钥匙串线程前捕获本次后端。
    static TEST_BACKEND: std::cell::Cell<Option<&'static dyn VaultBackend>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(not(test), target_os = "macos"))]
static BACKEND: std::sync::RwLock<Option<&'static dyn VaultBackend>> = std::sync::RwLock::new(None);

#[cfg(target_os = "macos")]
struct MissingBackend;

#[cfg(target_os = "macos")]
static MISSING_BACKEND: MissingBackend = MissingBackend;

#[cfg(target_os = "macos")]
impl VaultBackend for MissingBackend {
    fn load(&self, service: &str, account: &str) -> Result<Vec<u8>, VaultKeyError> {
        // 调试构建的自动化（应用层测试、截图脚本）用内存钥匙串，不碰用户真实条目。
        // 发行版没有这条路径，没注册后端就明确失败。
        #[cfg(debug_assertions)]
        if memory_backend_enabled() {
            return memory_backend_load(service, account);
        }
        #[cfg(not(debug_assertions))]
        let _ = (service, account);
        Err(VaultKeyError::Failed { status: 0 })
    }

    fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), VaultKeyError> {
        #[cfg(debug_assertions)]
        if memory_backend_enabled() {
            return memory_backend_store(service, account, secret);
        }
        #[cfg(not(debug_assertions))]
        let _ = (service, account, secret);
        Err(VaultKeyError::Failed { status: 0 })
    }
}

/// 调试构建专用：`ZSSH_VAULT_MEMORY=1` 时用进程内的内存钥匙串。
#[cfg(all(target_os = "macos", debug_assertions))]
fn memory_backend_enabled() -> bool {
    std::env::var_os("ZSSH_VAULT_MEMORY").is_some_and(|value| value == "1")
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn memory_items() -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>> {
    static ITEMS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
    > = std::sync::OnceLock::new();
    ITEMS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn memory_backend_load(service: &str, account: &str) -> Result<Vec<u8>, VaultKeyError> {
    memory_items()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&format!("{service}\u{0}{account}"))
        .cloned()
        .ok_or(VaultKeyError::Missing)
}

#[cfg(all(target_os = "macos", debug_assertions))]
fn memory_backend_store(service: &str, account: &str, secret: &[u8]) -> Result<(), VaultKeyError> {
    memory_items()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(format!("{service}\u{0}{account}"), secret.to_vec());
    Ok(())
}

/// 注册资料库密钥的保管后端。后注册的替换先注册的，测试用它换失败场景。
#[cfg(target_os = "macos")]
pub fn set_vault_backend(backend: &'static dyn VaultBackend) {
    // 测试会在同一进程里换后端（正常、取消、锁定、缺失）。不清缓存的话，换上去的失败后端
    // 根本不会被问到，测试看不出“失败时拒绝解密”。生产里后端只注册一次，清掉无害。
    #[cfg(test)]
    vault::clear_cache_for_test();
    #[cfg(test)]
    TEST_BACKEND.with(|slot| slot.set(Some(backend)));
    #[cfg(not(test))]
    {
        *BACKEND
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(backend);
    }
}

#[cfg(target_os = "macos")]
fn key_error(err: VaultKeyError) -> ZsshError {
    let (key, text) = match err {
        VaultKeyError::Missing => (
            "vault_key_missing",
            "钥匙串里没有这把资料库密钥，已有密文保持原样".to_owned(),
        ),
        VaultKeyError::Cancelled => (
            "vault_key_cancelled",
            "钥匙串授权已取消，没有读写密钥".to_owned(),
        ),
        VaultKeyError::Locked => ("vault_key_locked", "钥匙串已锁定，没有读写密钥".to_owned()),
        VaultKeyError::Failed { status } => (
            "vault_key_failed",
            format!("钥匙串读写失败（状态 {status}）"),
        ),
        // 正常路径在回读确认后就消化了这个状态，走到这里说明没有确认就放弃了。
        VaultKeyError::Exists => (
            "vault_key_exists",
            "钥匙串里已经有这把资料库密钥，没有覆盖".to_owned(),
        ),
    };
    ZsshError::Secret(Reason::new(key, text))
}

/// 是不是“条目已存在、没有覆盖”的那一种失败。调用方据此回读已有密钥。
fn is_key_exists(err: &ZsshError) -> bool {
    matches!(err, ZsshError::Secret(reason) if reason.key() == "vault_key_exists")
}

/// 是不是“钥匙串里没有这把密钥”。只有这时才允许新建。
fn is_key_missing(err: &ZsshError) -> bool {
    matches!(err, ZsshError::Secret(reason) if reason.key() == "vault_key_missing")
}

#[cfg(target_os = "macos")]
pub use vault::set_thread_vault;

/// 在一段代码里把秘密操作绑定到某个资料库身份，离开作用域自动恢复。
/// 后台线程（同步、备份）不经过数据库连接，必须自己带上这份身份。
#[cfg(target_os = "macos")]
pub struct VaultScope {
    previous: Option<String>,
}

#[cfg(target_os = "macos")]
impl VaultScope {
    pub fn new(account: &str) -> Self {
        Self {
            previous: vault::set_thread_vault(Some(account.to_owned())),
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for VaultScope {
    fn drop(&mut self) {
        vault::set_thread_vault(self.previous.take());
    }
}

/// macOS：秘密用资料库密钥做 XChaCha20-Poly1305。密钥只放在钥匙串里，
/// 密文仍进原来的字段。信封带固定前缀，和 Windows 的 DPAPI 密文区分开。
#[cfg(target_os = "macos")]
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    vault::seal(plaintext)
}

/// 只允许新资料库显式初始化；普通保存绝不能替换丢失的密钥。
#[cfg(target_os = "macos")]
pub(crate) fn initialize_vault(account: &str) -> Result<(), ZsshError> {
    let _scope = VaultScope::new(account);
    vault::vault_key(true).map(|_| ())
}

#[cfg(target_os = "macos")]
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    vault::open(blob)
}

/// 既不是 Windows 也不是 macOS：没有可用的本机保管，明确报错。
#[cfg(not(any(windows, target_os = "macos")))]
pub fn protect(_plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
    Err(ZsshError::Secret(Reason::text(
        "这个平台没有可用的本机密钥保管，敏感信息无法加密",
    )))
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
    Err(ZsshError::Secret(Reason::text(
        "这个平台没有可用的本机密钥保管，敏感信息无法解密",
    )))
}

#[cfg(target_os = "macos")]
mod vault {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    use zeroize::Zeroizing;

    use super::*;

    /// 新信封的前缀。DPAPI 密文没有这个前缀，读到时按旧格式处理。
    const MAGIC: &[u8] = b"ZTVAULT1";
    const NONCE_LEN: usize = 24;
    /// 钥匙串里的服务名。账户名按数据目录区分，不从用户名或机器名派生。
    const SERVICE: &str = "ZeeTerm.vault";

    /// 当前线程正在处理的资料库。没设置时用默认账户，已有的单库调用不用改。
    /// 不同资料库必须设不同值，否则会共用一把密钥。
    #[cfg(test)]
    pub(super) fn vault_account_for_test() -> String {
        vault_account()
    }

    /// 测试切换后端时丢掉已缓存的密钥，让下一次读取真的问到新后端。
    #[cfg(test)]
    pub(super) fn clear_cache_for_test() {
        let mut guard = KEY_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cache) = guard.as_mut() {
            cache.clear();
        }
    }

    static KEY_CACHE: std::sync::Mutex<
        Option<std::collections::HashMap<String, Zeroizing<[u8; 32]>>>,
    > = std::sync::Mutex::new(None);

    fn vault_account() -> String {
        VAULT_ACCOUNT
            .try_with(|cell| cell.borrow().clone())
            .ok()
            .flatten()
            .or_else(|| std::env::var("ZSSH_VAULT_ACCOUNT").ok())
            .unwrap_or_else(|| "database-key".to_owned())
    }

    thread_local! {
        static VAULT_ACCOUNT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    type KeychainJob = Box<dyn FnOnce() + Send>;

    /// 一把长期存在的线程，专门跑钥匙串调用。
    ///
    /// 不能每次调用都新建线程：一次连接要解开好几条密文。也不能在调用方线程上直接跑，
    /// 系统授权框需要自己的事件循环。队列空时线程停在 `recv` 上，不占 CPU。
    fn keychain_thread() -> &'static std::sync::mpsc::Sender<KeychainJob> {
        static THREAD: std::sync::OnceLock<std::sync::mpsc::Sender<KeychainJob>> =
            std::sync::OnceLock::new();
        THREAD.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<KeychainJob>();
            // 起不来时后面的 send 会失败，调用方拿到明确的错误，不在这里崩。
            let _ = std::thread::Builder::new()
                .name("vault-keychain".to_owned())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                });
            tx
        })
    }

    /// 把一次钥匙串调用派到 [`keychain_thread`] 上，再把结果带回来。
    fn keychain_call<T>(
        call: impl FnOnce() -> Result<T, VaultKeyError> + Send + 'static,
    ) -> Result<T, ZsshError>
    where
        T: Send + 'static,
    {
        // 测试的后端是线程局部的（每个测试换自己的失败场景），派到另一条线程就看不到了。
        // 测试里没有系统授权框，直接在调用方线程上跑。
        #[cfg(test)]
        {
            return call().map_err(key_error);
        }
        #[cfg(not(test))]
        {
            let (tx, rx) = std::sync::mpsc::channel();
            keychain_thread()
                .send(Box::new(move || {
                    let _ = tx.send(call());
                }))
                .map_err(|_| ZsshError::Secret(Reason::text("钥匙串线程已经退出")))?;
            rx.recv()
                .map_err(|_| ZsshError::Secret(Reason::text("钥匙串线程没有返回结果")))?
                .map_err(key_error)
        }
    }

    /// 在当前线程把随后的加密解密绑定到一个资料库身份上。返回之前的身份。
    pub fn set_thread_vault(account: Option<String>) -> Option<String> {
        VAULT_ACCOUNT.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), account))
    }

    pub fn seal(plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
        // 资料库账户只能由初始化入口建钥匙；未绑定的历史文件级调用保留其账户。
        let scoped = VAULT_ACCOUNT.with(|slot| slot.borrow().is_some());
        let key = vault_key(!scoped)?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce)
            .map_err(|err| ZsshError::Secret(Reason::text(format!("系统随机数不可用：{err}"))))?;
        let cipher = XChaCha20Poly1305::new((&*key).into());
        let encrypted = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: MAGIC,
                },
            )
            .map_err(|_| ZsshError::Secret(Reason::text("本地加密失败")))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + encrypted.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&encrypted);
        Ok(out)
    }

    pub fn open(blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
        let rest = blob
            .strip_prefix(MAGIC)
            .ok_or_else(|| ZsshError::Secret(Reason::text("不是本机信封，无法解密")))?;
        if rest.len() < NONCE_LEN {
            return Err(ZsshError::Secret(Reason::text("本机信封不完整")));
        }
        let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
        // 解密只读取已有密钥：缺失时失败，不新建一把把旧密文变成解不开的。
        let key = vault_key(false)?;
        let cipher = XChaCha20Poly1305::new((&*key).into());
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: MAGIC,
                },
            )
            .map_err(|_| ZsshError::Secret(Reason::text("本机密文校验失败")))
    }

    /// `create` 为真且钥匙串里还没有密钥时，生成一把并保存。
    /// 已有密文但密钥缺失时调用方传 `false`，这里直接失败。
    pub(super) fn vault_key(create: bool) -> Result<Zeroizing<[u8; 32]>, ZsshError> {
        let mut cache = KEY_CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cache = cache.get_or_insert_with(std::collections::HashMap::new);
        let account = vault_account();
        // 缓存命中就不再问钥匙串：一次连接要解好几条密文，每条都走一次 IPC 会把连接拖慢。
        // 缓存只在读取失败时清掉（见下面的 Err 分支），所以取消、锁定和缺失仍然会暴露出来。
        if let Some(cached) = cache.get(&account) {
            return Ok(cached.clone());
        }
        // 钥匙串调用放到专用线程上：系统授权框需要能跑自己的事件循环，不能占着调用方的线程。
        let lookup = account.clone();
        let selected_backend = backend();
        match keychain_call(move || selected_backend.load(SERVICE, &lookup)) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                let key = Zeroizing::new(key);
                cache.insert(account, key.clone());
                Ok(key)
            }
            Ok(_) => Err(ZsshError::Secret(Reason::text(
                "钥匙串里的资料库密钥长度不对，没有使用它",
            ))),
            Err(err) if is_key_missing(&err) && create => {
                let mut key = Zeroizing::new([0u8; 32]);
                getrandom::fill(&mut key[..]).map_err(|err| {
                    ZsshError::Secret(Reason::text(format!("系统随机数不可用：{err}")))
                })?;
                let account = vault_account();
                let secret = key.clone();
                // 上次打开时密钥已经写进钥匙串、但“已初始化”没能落盘时，条目还在。
                // 这时不覆盖（覆盖会让已有密文全部解不开），回读那一把并用它。
                let stored = match keychain_call({
                    let account = account.clone();
                    move || backend().store(SERVICE, &account, secret.as_slice())
                }) {
                    Ok(()) => key.to_vec(),
                    Err(err) if is_key_exists(&err) => keychain_call({
                        let account = account.clone();
                        move || backend().load(SERVICE, &account)
                    })?,
                    Err(err) => return Err(err),
                };
                if stored.len() != 32 {
                    return Err(ZsshError::Secret(Reason::text(
                        "钥匙串里的资料库密钥长度不对，没有使用它",
                    )));
                }
                key.copy_from_slice(&stored);
                cache.insert(account, key.clone());
                Ok(key)
            }
            Err(err) => {
                cache.remove(&vault_account());
                Err(err)
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    fn install() {
        set_vault_backend(&TEST_MEMORY);
    }

    /// 本模块的历史测试共用固定账户名；只串行这些场景，不限制其他资料库测试。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn failing_backend_does_not_affect_another_database_thread() {
        set_vault_backend(&CANCELLED);
        let result = std::thread::spawn(|| {
            let db = crate::store::Database::open_in_memory().unwrap();
            let blob = db.protect_secret(b"parallel").unwrap();
            assert_eq!(db.unprotect_secret(&blob).unwrap(), b"parallel");
        })
        .join();
        assert!(result.is_ok());
        assert!(initialize_vault("cancelled-thread").is_err());
        install();
    }

    #[test]
    fn roundtrip_keeps_plaintext_out_of_the_blob() {
        let _serial = serial();
        install();
        let plaintext = "一段带中文的密码 p@ssw0rd".as_bytes();
        let blob = protect(plaintext).expect("加密应该成功");
        assert!(blob.starts_with(b"ZTVAULT1"), "新信封应有固定前缀");
        assert!(!String::from_utf8_lossy(&blob).contains("p@ssw0rd"));
        assert_eq!(unprotect(&blob).expect("解密应该成功"), plaintext);
    }

    #[test]
    fn tampered_blob_is_rejected() {
        let _serial = serial();
        install();
        let mut blob = protect(b"secret").expect("加密应该成功");
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(unprotect(&blob).is_err(), "改过的密文不该解得开");
    }

    /// 固定返回一种失败的后端，用来验证失败时不新建密钥、不改已有密文。
    struct FailingVault(VaultKeyError);

    impl VaultBackend for FailingVault {
        fn load(&self, _: &str, _: &str) -> Result<Vec<u8>, VaultKeyError> {
            Err(match self.0 {
                VaultKeyError::Missing => VaultKeyError::Missing,
                VaultKeyError::Cancelled => VaultKeyError::Cancelled,
                VaultKeyError::Locked => VaultKeyError::Locked,
                VaultKeyError::Failed { status } => VaultKeyError::Failed { status },
                VaultKeyError::Exists => VaultKeyError::Exists,
            })
        }

        fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), VaultKeyError> {
            Err(VaultKeyError::Failed { status: 0 })
        }
    }

    /// 第一次写入报“已存在”，回读时返回调用前放进去的那把密钥。
    /// 用来模拟：上次打开已经把密钥写进钥匙串，但“已初始化”没能落盘。
    struct ExistsThenKeeps {
        key: std::sync::Mutex<Option<Vec<u8>>>,
    }

    impl VaultBackend for ExistsThenKeeps {
        fn load(&self, _: &str, _: &str) -> Result<Vec<u8>, VaultKeyError> {
            self.key
                .lock()
                .unwrap()
                .clone()
                .ok_or(VaultKeyError::Missing)
        }

        fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), VaultKeyError> {
            Err(VaultKeyError::Exists)
        }
    }

    #[test]
    fn an_existing_keychain_item_is_reused_instead_of_failing_the_database() {
        let existing = vec![7u8; 32];
        static BACKEND: std::sync::OnceLock<ExistsThenKeeps> = std::sync::OnceLock::new();
        let backend = BACKEND.get_or_init(|| ExistsThenKeeps {
            key: std::sync::Mutex::new(None),
        });
        *backend.key.lock().unwrap() = Some(existing.clone());
        set_vault_backend(backend);

        let db = crate::store::Database::open_in_memory().unwrap();
        let blob = db.protect_secret(b"reused").unwrap();
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"reused");
        // 用的是钥匙串里原有的那把，不是新生成的。
        assert_eq!(backend.key.lock().unwrap().clone().unwrap(), existing);
        install();
    }

    #[test]
    fn a_panic_inside_a_connection_restores_the_previous_vault_identity() {
        install();
        let db = crate::store::Database::open_in_memory().unwrap();
        let account = db.vault_account().to_owned();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            db.with_conn(|_| -> Result<(), ZsshError> {
                panic!("闭包中途失败");
            })
        }));
        assert!(caught.is_err());
        // 身份必须回到调用前的状态，否则这个线程上的后续操作会用错库的密钥。
        assert_ne!(vault::vault_account_for_test(), account);
        assert_eq!(vault::vault_account_for_test(), "database-key");
    }

    static CANCELLED: FailingVault = FailingVault(VaultKeyError::Cancelled);
    static LOCKED: FailingVault = FailingVault(VaultKeyError::Locked);
    static MISSING: FailingVault = FailingVault(VaultKeyError::Missing);

    #[test]
    fn cancelled_locked_or_missing_key_keeps_the_existing_ciphertext() {
        let _serial = serial();
        install();
        let blob = protect(b"original-secret").expect("先用正常后端加密");
        let stored_before = test_memory()
            .lock()
            .unwrap()
            .get("ZeeTerm.vault\u{0}database-key")
            .cloned();

        for (backend, expect) in [
            (&CANCELLED as &dyn VaultBackend, "取消"),
            (&LOCKED as &dyn VaultBackend, "锁定"),
            (&MISSING as &dyn VaultBackend, "没有这把资料库密钥"),
        ] {
            set_vault_backend(backend);
            let err = unprotect(&blob).expect_err("失败时必须拒绝解密");
            let text = err.to_string();
            assert!(text.contains(expect), "{text}");
            assert!(protect(b"new").is_err(), "失败时也不能加密出新密文");
        }

        install();
        assert_eq!(
            test_memory()
                .lock()
                .unwrap()
                .get("ZeeTerm.vault\u{0}database-key")
                .cloned(),
            stored_before,
            "失败路径不应新增或替换钥匙串条目"
        );
        assert_eq!(
            unprotect(&blob).expect("恢复后端后原密文仍可解"),
            b"original-secret"
        );
    }

    #[test]
    fn two_vaults_keep_separate_keys_and_refuse_each_others_blobs() {
        let _serial = serial();
        install();
        // 交替使用两个资料库身份保存，确认不会串用一把密钥。
        initialize_vault("vault-a").unwrap();
        initialize_vault("vault-b").unwrap();
        let a_old = vault::set_thread_vault(Some("vault-a".to_owned()));
        let blob_a = protect(b"secret-a").expect("A 加密");
        let b_old = vault::set_thread_vault(Some("vault-b".to_owned()));
        let blob_b = protect(b"secret-b").expect("B 加密");
        let _ = (a_old, b_old);
        vault::set_thread_vault(Some("vault-a".to_owned()));

        assert_eq!(unprotect(&blob_a).expect("A 解密"), b"secret-a");
        vault::set_thread_vault(Some("vault-b".to_owned()));
        assert_eq!(unprotect(&blob_b).expect("B 解密"), b"secret-b");

        // 用 B 的身份解 A 的密文必须失败：两把密钥不同，认证会拒绝。
        assert!(unprotect(&blob_a).is_err(), "串库必须被拒绝");
        vault::set_thread_vault(Some("vault-a".to_owned()));
        assert!(unprotect(&blob_b).is_err(), "反方向也要被拒绝");

        // 两个资料库各有一把互不相同的密钥。
        let entries = test_memory().lock().unwrap().clone();
        let key_a = entries
            .get("ZeeTerm.vault\u{0}vault-a")
            .expect("vault-a 应有密钥")
            .clone();
        let key_b = entries
            .get("ZeeTerm.vault\u{0}vault-b")
            .expect("vault-b 应有密钥")
            .clone();
        assert_ne!(key_a, key_b, "两个资料库不能共用一把密钥");
        vault::set_thread_vault(None);
        let _ = b_old;
    }

    #[test]
    fn a_missing_key_never_creates_a_replacement() {
        let _serial = serial();
        install();
        initialize_vault("vault-lost").unwrap();
        let old = vault::set_thread_vault(Some("vault-lost".to_owned()));
        let blob = protect(b"keep-me").expect("加密");
        // 模拟密钥被删：清掉后端条目和缓存，但保留密文。
        test_memory()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove("ZeeTerm.vault\u{0}vault-lost");
        vault::clear_cache_for_test();
        let err = unprotect(&blob).expect_err("密钥缺失必须失败");
        assert!(err.to_string().contains("没有这把资料库密钥"), "{err}");
        assert!(
            !test_memory()
                .lock()
                .unwrap()
                .contains_key("ZeeTerm.vault\u{0}vault-lost"),
            "缺失时不能自动生成新密钥"
        );
        vault::set_thread_vault(old);
    }

    #[test]
    fn database_identity_survives_move_and_missing_key_refuses_new_secrets() {
        let _serial = serial();
        install();
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.db");
        let moved = dir.path().join("moved.db");
        let db = crate::store::Database::open(&original).unwrap();
        let account = db.vault_account().to_owned();
        let blob = db.protect_secret(b"original").unwrap();
        drop(db);
        std::fs::rename(&original, &moved).unwrap();
        let db = crate::store::Database::open(&moved).unwrap();
        assert_eq!(db.vault_account(), account);
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"original");
        test_memory()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&format!("ZeeTerm.vault\u{0}{account}"));
        vault::clear_cache_for_test();
        assert!(db.protect_secret(b"replacement").is_err());
        drop(db);
        let db = crate::store::Database::open(&moved).unwrap();
        assert!(db.protect_secret(b"replacement").is_err());
    }

    #[test]
    fn file_database_failed_saves_preserve_rows_after_move_and_reopen() {
        use crate::model::{IdentityDraft, IdentityKind};
        use crate::store::Database;

        install();
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let moved = dir.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        let db = Database::open(&original.join("zssh.db")).unwrap();
        let account = db.vault_account().to_owned();
        assert!(account.starts_with("vault-"));
        let mut draft = IdentityDraft {
            id: None,
            name: "test-identity".to_owned(),
            kind: IdentityKind::Password,
            key_path: None,
            secret: Some("synthetic-original".to_owned()),
            passphrase: None,
        };
        draft.id = Some(db.identity_save(&draft).unwrap().id);
        // 比对整行而不仅是密文，避免失败保存偷偷改名称或时间戳。
        let snapshot = |db: &Database| {
            db.with_conn(|conn| {
                let mut statement = conn.prepare("SELECT * FROM identities ORDER BY id")?;
                let columns = statement.column_count();
                let rows = statement.query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
        };
        let before = snapshot(&db);
        let blob = db
            .with_conn(|conn| {
                Ok(
                    conn.query_row("SELECT secret_blob FROM identities", [], |row| {
                        row.get::<_, Vec<u8>>(0)
                    })?,
                )
            })
            .unwrap();
        drop(db);
        std::fs::rename(&original, &moved).unwrap();
        let path = moved.join("zssh.db");
        let db = Database::open(&path).unwrap();
        assert_eq!(db.vault_account(), account);
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"synthetic-original");
        let item = format!("ZeeTerm.vault\u{0}{account}");
        let key = test_memory().lock().unwrap().remove(&item).unwrap();
        vault::clear_cache_for_test();
        draft.name = "failed-change".to_owned();
        draft.secret = Some("synthetic-replacement".to_owned());
        for db in [db, Database::open(&path).unwrap()] {
            assert!(db.unprotect_secret(&blob).is_err());
            assert!(db.identity_save(&draft).is_err());
            let mut insert = draft.clone();
            insert.id = None;
            assert!(db.identity_save(&insert).is_err());
            assert_eq!(snapshot(&db), before);
            assert!(!test_memory().lock().unwrap().contains_key(&item));
        }
        test_memory().lock().unwrap().insert(item, key);
        let db = Database::open(&path).unwrap();
        assert_eq!(snapshot(&db), before);
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"synthetic-original");
    }

    #[test]
    fn cancelled_initial_key_write_retries_without_changing_identity() {
        struct CancelWrite;
        impl VaultBackend for CancelWrite {
            fn load(&self, _: &str, _: &str) -> Result<Vec<u8>, VaultKeyError> {
                Err(VaultKeyError::Missing)
            }
            fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), VaultKeyError> {
                Err(VaultKeyError::Cancelled)
            }
        }
        static CANCEL_WRITE: CancelWrite = CancelWrite;
        set_vault_backend(&CANCEL_WRITE);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.db");
        assert!(crate::store::Database::open(&path).is_err());
        let conn = rusqlite::Connection::open(&path).unwrap();
        let (account, initialized): (String, bool) = conn
            .query_row("SELECT account, initialized FROM local_vault", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert!(!initialized);
        assert!(!test_memory()
            .lock()
            .unwrap()
            .contains_key(&format!("ZeeTerm.vault\u{0}{account}")));
        drop(conn);
        install();
        let db = crate::store::Database::open(&path).unwrap();
        assert_eq!(db.vault_account(), account);
        let blob = db.protect_secret(b"synthetic-retry").unwrap();
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"synthetic-retry");
    }

    #[test]
    fn legacy_path_identity_is_persisted_without_replacing_its_key() {
        let _serial = serial();
        install();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let db = crate::store::Database::open(&path).unwrap();
        let legacy = crate::store::db::vault_account_for(&path);
        initialize_vault(&legacy).unwrap();
        let blob = {
            let _scope = VaultScope::new(&legacy);
            protect(b"legacy").unwrap()
        };
        db.with_conn(|conn| {
            conn.execute("INSERT INTO identities (name, kind, secret_blob, uid) VALUES ('test', 'password', ?1, 'test-legacy')", [&blob])?;
            conn.execute("DROP TABLE local_vault", [])?;
            Ok(())
        })
        .unwrap();
        drop(db);
        let db = crate::store::Database::open(&path).unwrap();
        assert_eq!(db.vault_account(), legacy);
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"legacy");
        drop(db);
        let moved = dir.path().join("legacy-moved.db");
        std::fs::rename(&path, &moved).unwrap();
        let db = crate::store::Database::open(&moved).unwrap();
        assert_eq!(db.vault_account(), legacy);
        let stored: Vec<u8> = db
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT secret_blob FROM identities", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(stored, blob);
        assert_eq!(db.unprotect_secret(&stored).unwrap(), b"legacy");
    }

    #[test]
    fn legacy_missing_key_is_not_initialized_even_without_secret_rows() {
        let _serial = serial();
        install();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-empty.db");
        let db = crate::store::Database::open(&path).unwrap();
        db.with_conn(|conn| {
            conn.execute("DROP TABLE local_vault", [])?;
            Ok(())
        })
        .unwrap();
        drop(db);
        let account = crate::store::db::vault_account_for(&path);
        let db = crate::store::Database::open(&path).unwrap();
        assert_eq!(db.vault_account(), account);
        assert!(db.protect_secret(b"new").is_err());
        assert!(!test_memory()
            .lock()
            .unwrap()
            .contains_key(&format!("ZeeTerm.vault\u{0}{account}")));
    }

    #[test]
    fn cancelled_initialization_retries_the_persisted_pending_identity() {
        let _serial = serial();
        set_vault_backend(&CANCELLED);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.db");
        assert!(crate::store::Database::open(&path).is_err());
        let conn = rusqlite::Connection::open(&path).unwrap();
        let account: String = conn
            .query_row("SELECT account FROM local_vault", [], |row| row.get(0))
            .unwrap();
        drop(conn);
        install();
        let db = crate::store::Database::open(&path).unwrap();
        assert_eq!(db.vault_account(), account);
        assert!(db.protect_secret(b"new").is_ok());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn memory_override_isolates_an_installed_backend() {
        if std::env::var("ZSSH_VAULT_MEMORY").as_deref() != Ok("1") {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            let status = crate::process_util::hide_console(&mut command)
                .args([
                    "--exact",
                    "store::secrets::macos_tests::memory_override_isolates_an_installed_backend",
                    "--test-threads=1",
                ])
                .env("ZSSH_VAULT_MEMORY", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        struct Forbidden;
        impl VaultBackend for Forbidden {
            fn load(&self, _: &str, _: &str) -> Result<Vec<u8>, VaultKeyError> {
                panic!("real backend was accessed")
            }
            fn store(&self, _: &str, _: &str, _: &[u8]) -> Result<(), VaultKeyError> {
                panic!("real backend was accessed")
            }
        }
        static FORBIDDEN: Forbidden = Forbidden;
        set_vault_backend(&FORBIDDEN);
        let db = crate::store::Database::open_in_memory().unwrap();
        let blob = db.protect_secret(b"isolated").unwrap();
        assert_eq!(db.unprotect_secret(&blob).unwrap(), b"isolated");
    }

    #[test]
    fn a_foreign_blob_is_not_decrypted_as_empty() {
        let _serial = serial();
        install();
        // 没有本机信封前缀的内容（例如从 Windows 拷来的 DPAPI 密文）必须失败，
        // 不能解出空串，也不能顺手生成一把新密钥。
        let err = unprotect(b"not-a-vault-blob").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("不是本机信封"), "{text}");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_restores_plaintext() {
        let plaintext = "一段带中文的密码 p@ssw0rd".as_bytes();
        let blob = protect(plaintext).expect("加密应该成功");

        assert_ne!(blob, plaintext, "密文不该等于明文");
        assert!(!blob.is_empty(), "密文不该是空的");
        assert_eq!(
            unprotect(&blob).expect("解密应该成功"),
            plaintext,
            "解密之后应该和原文一样"
        );
    }

    #[test]
    fn roundtrip_handles_empty_input() {
        let blob = protect(b"").expect("空数据也能加密");
        assert!(unprotect(&blob).expect("解密应该成功").is_empty());
    }

    #[test]
    fn tampered_blob_is_rejected() {
        let mut blob = protect(b"secret").expect("加密应该成功");
        // 改一个字节：DPAPI 会校验完整性，应该失败而不是解出垃圾。
        if let Some(byte) = blob.first_mut() {
            *byte ^= 0xff;
        }
        assert!(unprotect(&blob).is_err(), "改过的密文不该解得开");
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext() {
        let plaintext = "a-very-distinctive-password-9f3a";
        let blob = protect(plaintext.as_bytes()).expect("加密应该成功");
        let haystack = String::from_utf8_lossy(&blob);
        assert!(!haystack.contains(plaintext), "密文里不该出现明文片段");
    }
}
