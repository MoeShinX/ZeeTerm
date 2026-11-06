//! SQLite 连接与迁移。
//!
//! 数据库路径由调用方（Tauri 层）给，core 不自己决定放哪。
//!
//! 迁移按版本号递增执行，每个迁移一个事务；**写好的迁移不再改**，以后新增列或
//! 索引一律加新的迁移文件。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::error::ZsshError;

/// (版本号, SQL)。版本号必须严格递增。
///
/// `pub` 是为了让测试能单独铺一个旧版本的库，验证升级路径。
pub const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("migrations/0001_init.sql")),
    (2, include_str!("migrations/0002_legacy_algorithms.sql")),
    (3, include_str!("migrations/0003_proxy_mode.sql")),
    (4, include_str!("migrations/0004_ai_audit.sql")),
    (5, include_str!("migrations/0005_ai_audit_location.sql")),
    (6, include_str!("migrations/0006_ai_audit_files.sql")),
    (7, include_str!("migrations/0007_uid.sql")),
    (8, include_str!("migrations/0008_sync_state.sql")),
    (9, include_str!("migrations/0009_port_forwards.sql")),
    (10, include_str!("migrations/0010_host_memory.sql")),
    (11, include_str!("migrations/0011_host_os.sql")),
    (12, include_str!("migrations/0012_host_protocol.sql")),
    (13, include_str!("migrations/0013_ai_endpoints.sql")),
];

/// 一条 SQLite 连接，加锁后用。
///
/// `rusqlite::Connection` 不是 `Sync`，放进 Mutex 之后整个 `Database` 可以共享。
pub struct Database {
    conn: Mutex<Connection>,
    /// 用户改了会进同步包的数据时调用（自动同步的“变化后 30 秒”靠它）。
    /// 同步自己写本地库走的是 `apply_snapshot`，不经过这里，所以不会自己触发自己。
    change_listener: Mutex<Option<ChangeListener>>,
    /// 本机资料库持久化的密钥身份，移动目录不改变它。
    /// 老资料库沿用原路径哈希账户，以免遗弃已有钥匙。
    vault_account: String,
}

type ChangeListener = std::sync::Arc<dyn Fn() + Send + Sync>;

/// 收集所有可能由旧默认密钥加密的库内值：凭据、代理、冲突和设置密文。
///
/// 只用于“这个库是否曾经用过旧 database-key”的判定；无法识别的值不参与，
/// 由调用方保守地放弃启用回退。
#[cfg(target_os = "macos")]
fn collect_legacy_candidates(conn: &Connection) -> Result<Vec<Vec<u8>>, ZsshError> {
    use base64::Engine as _;
    let mut out = Vec::new();
    for table in ["identities", "proxies"] {
        let mut statement = conn.prepare(&format!(
            "SELECT secret_blob FROM {table} WHERE secret_blob IS NOT NULL"
        ))?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        for row in rows {
            out.push(row?);
        }
    }
    let mut statement = conn.prepare("SELECT kept, other FROM sync_conflicts")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (kept, other) = row?;
        for value in [kept, other] {
            if let Some(encoded) = value.strip_prefix("enc1:") {
                if let Ok(blob) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                    out.push(blob);
                }
            }
        }
    }
    let mut statement = conn.prepare("SELECT value FROM settings")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let value = row?;
        let Ok(encoded) = serde_json::from_str::<String>(&value) else {
            continue;
        };
        let Ok(blob) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            continue;
        };
        if blob.starts_with(b"ZTVAULT1") {
            out.push(blob);
        }
    }
    Ok(out)
}

impl Database {
    /// 打开（或新建）指定路径的库，并把迁移跑到最新。
    pub fn open(path: &Path) -> Result<Self, ZsshError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn, vault_account_for(path))
    }

    /// 内存库，只给测试用。每个内存库有自己的密钥身份，不会互相串。
    pub fn open_in_memory() -> Result<Self, ZsshError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, unique_memory_account())
    }

    /// 这份资料库的密钥身份。
    pub fn vault_account(&self) -> &str {
        &self.vault_account
    }

    /// 在本次调用的整段时间内绑定本库身份。**在数据库锁外**解密或加密时必须用它：
    /// 先把行读出来、再解锁、再解密，会让解密落到默认账户上，和写入时的账户对不上。
    pub fn with_vault_scope<T>(&self, run: impl FnOnce() -> T) -> T {
        #[cfg(target_os = "macos")]
        {
            let _scope = crate::store::secrets::VaultScope::new(&self.vault_account);
            run()
        }
        #[cfg(not(target_os = "macos"))]
        {
            run()
        }
    }

    /// 用本资料库的身份加密一段秘密。
    ///
    /// 加解密常常发生在 `with_conn` 之前（先算出密文再写库），这类调用必须走这里，
    /// 不能直接调 `secrets::protect`：那时线程上还没有本库身份，会落到默认账户上，
    /// 写进去的密文和之后读它的人对不上。
    pub fn protect_secret(&self, plaintext: &[u8]) -> Result<Vec<u8>, ZsshError> {
        #[cfg(target_os = "macos")]
        {
            let _scope = crate::store::secrets::VaultScope::new(&self.vault_account);
            crate::store::secrets::protect(plaintext)
        }
        #[cfg(not(target_os = "macos"))]
        {
            crate::store::secrets::protect(plaintext)
        }
    }

    /// 用本资料库的身份解密一段秘密。见 [`Self::protect_secret`]。
    pub fn unprotect_secret(&self, blob: &[u8]) -> Result<Vec<u8>, ZsshError> {
        #[cfg(target_os = "macos")]
        {
            let _scope = crate::store::secrets::VaultScope::new(&self.vault_account);
            crate::store::secrets::unprotect(blob)
        }
        #[cfg(not(target_os = "macos"))]
        {
            crate::store::secrets::unprotect(blob)
        }
    }

    fn from_connection(conn: Connection, vault_account: String) -> Result<Self, ZsshError> {
        // 外键默认是关的，每个连接都要打开。
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL：读写不互相阻塞。内存库会返回 memory，不用管。
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;

        #[cfg(target_os = "macos")]
        let fresh = current_version(&conn)? == 0;
        let database = Self {
            conn: Mutex::new(conn),
            change_listener: Mutex::new(None),
            vault_account,
        };
        database.migrate()?;
        #[cfg(target_os = "macos")]
        let mut database = database;
        #[cfg(target_os = "macos")]
        {
            use rusqlite::OptionalExtension;
            let mut guard = database.lock();
            // 多个后台连接可能同时首次打开资料库，身份选择和落盘必须串行。
            let conn = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // local_vault 只记录“这把密钥在本机钥匙串里的身份”，不进迁移、也不进同步快照。
            // 它随数据库文件一起被拷到另一台机器时，那边的钥匙串里没有对应密钥，密文解不开，
            // 这是预期：密钥不出本机。换机恢复要单独迁移钥匙串条目，不能靠复制这个文件。
            conn.execute_batch("CREATE TABLE IF NOT EXISTS local_vault (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), account TEXT NOT NULL, initialized INTEGER NOT NULL, legacy_default INTEGER NOT NULL DEFAULT 0)")?;
            let has_legacy_column = {
                let mut statement = conn.prepare("PRAGMA table_info(local_vault)")?;
                let names: Vec<String> = statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<_, _>>()?;
                names.iter().any(|name| name == "legacy_default")
            };
            if !has_legacy_column {
                conn.execute(
                    "ALTER TABLE local_vault ADD COLUMN legacy_default INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            let saved: Option<(String, bool, bool)> = conn
                .query_row(
                    "SELECT account, initialized, legacy_default FROM local_vault WHERE singleton = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (account, initialized, legacy_default) = match saved {
                Some((account, initialized, legacy)) if !account.is_empty() => {
                    (account, initialized, legacy)
                }
                Some(_) => {
                    return Err(ZsshError::Secret(crate::error::Reason::text(
                        "资料库密钥身份损坏",
                    )));
                }
                None => {
                    // 旧库保留原路径账户；绝不因密钥缺失切换到新身份。
                    let account = if fresh {
                        format!("vault-{}", uuid::Uuid::new_v4())
                    } else {
                        database.vault_account.clone()
                    };
                    conn.execute(
                        "INSERT INTO local_vault (singleton, account, initialized, legacy_default) \
                         VALUES (1, ?1, ?2, 0)",
                        rusqlite::params![account, !fresh],
                    )?;
                    (account, !fresh, false)
                }
            };
            conn.commit()?;
            // pending 身份先落盘：授权取消后可重试同一账户，不能换身份。
            if !initialized {
                crate::store::secrets::initialize_vault(&account)?;
                guard.execute(
                    "UPDATE local_vault SET initialized = 1 WHERE singleton = 1",
                    [],
                )?;
            }
            drop(guard);
            if account.starts_with("vault-") {
                let mut needs_legacy_fallback = legacy_default;
                if !needs_legacy_fallback {
                    let candidates = {
                        let conn = database.lock();
                        collect_legacy_candidates(&conn)?
                    };
                    if !candidates.is_empty() {
                        let _scope = crate::store::secrets::VaultScope::new(&account);
                        let mut saw_legacy = false;
                        let mut unsure = false;
                        for blob in candidates {
                            match crate::store::secrets::probe_legacy(&blob) {
                                crate::store::secrets::LegacyProbe::Current => {}
                                crate::store::secrets::LegacyProbe::Legacy => saw_legacy = true,
                                crate::store::secrets::LegacyProbe::Unknown
                                | crate::store::secrets::LegacyProbe::KeyUnavailable => {
                                    unsure = true;
                                    break;
                                }
                            }
                        }
                        if saw_legacy && !unsure {
                            database.lock().execute(
                                "UPDATE local_vault SET legacy_default = 1 WHERE singleton = 1",
                                [],
                            )?;
                            needs_legacy_fallback = true;
                        }
                    }
                }
                if needs_legacy_fallback {
                    crate::store::secrets::register_legacy_fallback(&account);
                }
            }
            database.vault_account = account;
        }
        Ok(database)
    }

    /// 把没跑过的迁移按顺序跑一遍。重复调用是安全的。
    pub fn migrate(&self) -> Result<(), ZsshError> {
        self.with_conn(|conn| {
            let current = current_version(conn)?;
            for (version, sql) in MIGRATIONS {
                if *version <= current {
                    continue;
                }
                let tx = conn.transaction()?;
                tx.execute_batch(sql)?;
                tx.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    [version],
                )?;
                tx.commit()?;
            }
            repair_uids(conn)?;
            Ok(())
        })
    }

    /// 当前 schema 版本号，全新库是 0。
    pub fn schema_version(&self) -> Result<i64, ZsshError> {
        self.with_conn(|conn| current_version(conn))
    }

    /// SQLite 的 `PRAGMA data_version`：**别的连接**提交过写操作时这个值会变。
    ///
    /// 主窗口拿它判断"主机库要不要重读"，不用再每隔几秒无条件跑一遍 12 条查询
    /// （审查 M-14）。同一个连接自己的写不会让它变，所以自己的写操作还是写完直接重读。
    pub fn data_version(&self) -> Result<i64, ZsshError> {
        self.with_conn(|conn| Ok(conn.query_row("PRAGMA data_version", [], |row| row.get(0))?))
    }

    /// 借连接用一下。锁里只做 SQLite 操作，不会 panic。
    pub(crate) fn with_conn<T>(
        &self,
        run: impl FnOnce(&mut Connection) -> Result<T, ZsshError>,
    ) -> Result<T, ZsshError> {
        let mut guard = self.lock();
        // 在这份资料库的身份下读写秘密：同一线程先后操作两个库，各用各的钥匙串条目。
        // 用作用域而不是手动恢复：闭包里 panic 时也要回到调用方原来的身份，
        // 否则这个线程池线程会一直带着上一个库的身份，之后的库用错密钥。
        #[cfg(target_os = "macos")]
        {
            let _scope = crate::store::secrets::VaultScope::new(&self.vault_account);
            run(&mut guard)
        }
        #[cfg(not(target_os = "macos"))]
        {
            run(&mut guard)
        }
    }

    /// 写操作用：成功后通知“数据变了”。
    pub(crate) fn with_conn_write<T>(
        &self,
        run: impl FnOnce(&mut Connection) -> Result<T, ZsshError>,
    ) -> Result<T, ZsshError> {
        let result = self.with_conn(run);
        if result.is_ok() {
            self.notify_changed();
        }
        result
    }

    pub(crate) fn notify_changed(&self) {
        let listener = self
            .change_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener();
        }
    }

    /// 登记“同步数据变了”的回调（只登记一个，后登记的覆盖前面的）。回调里不能
    /// 再访问数据库。
    pub fn set_change_listener(&self, listener: impl Fn() + Send + Sync + 'static) {
        *self
            .change_listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::sync::Arc::new(listener));
    }

    /// 同 [`Database::with_conn`]，但不限定错误类型。同步层用自己的 [`SyncError`]，
    /// 又需要在一个事务里做完导入，所以单独开这个口子。
    pub(crate) fn with_conn_as<T, E>(
        &self,
        run: impl FnOnce(&mut Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut guard = self.lock();
        // 和 with_conn 一样：panic 时也要恢复调用方原来的身份。
        #[cfg(target_os = "macos")]
        {
            let _scope = crate::store::secrets::VaultScope::new(&self.vault_account);
            run(&mut guard)
        }
        #[cfg(not(target_os = "macos"))]
        {
            run(&mut guard)
        }
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 给 `uid` 为空的老记录补 UUID。
///
/// 迁移文件本身是静态 SQL，生成不了 UUID，所以每次打开库都检查一遍：迁移刚跑完
/// 的旧库、以及被外部工具塞进来的空 uid 都在这里补齐。唯一索引已经建好，重复值
/// 会直接报错而不是悄悄覆盖。
fn repair_uids(conn: &Connection) -> Result<(), ZsshError> {
    for table in ["groups", "hosts", "identities", "proxies"] {
        let sql = format!("SELECT id FROM {table} WHERE uid IS NULL OR uid = ''");
        let mut statement = conn.prepare(&sql)?;
        let ids: Vec<i64> = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        drop(statement);
        for id in ids {
            conn.execute(
                &format!("UPDATE {table} SET uid = ?1 WHERE id = ?2"),
                rusqlite::params![crate::store::new_uid(), id],
            )?;
        }
    }
    Ok(())
}

/// 由库文件路径派生钥匙串里的资料库身份。用哈希而不是路径明文：
/// 账户名会出现在钥匙串界面和日志里，不能带出用户目录。
pub fn vault_account_for(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("db-{}", hex_prefix(&digest[..16]))
}

/// 每个内存库一个独立身份，测试之间不会互相串用密钥。
fn unique_memory_account() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("memory-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn hex_prefix(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `schema_version` 里最大的版本号。表还没建出来就是 0。
fn current_version(conn: &Connection) -> Result<i64, ZsshError> {
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT coalesce(max(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?)
}
