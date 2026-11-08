//! SQLite 连接与迁移。
//!
//! 数据库路径由调用方（Tauri 层）给，core 不自己决定放哪。
//!
//! 迁移按版本号递增执行，每个迁移一个事务；**写好的迁移不再改**，以后新增列或
//! 索引一律加新的迁移文件。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, TransactionBehavior};

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
}

type ChangeListener = std::sync::Arc<dyn Fn() + Send + Sync>;

impl Database {
    /// 打开（或新建）指定路径的库，并把迁移跑到最新。
    pub fn open(path: &Path) -> Result<Self, ZsshError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// 内存库，只给测试用。
    pub fn open_in_memory() -> Result<Self, ZsshError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, ZsshError> {
        // 外键默认是关的，每个连接都要打开。
        conn.pragma_update(None, "foreign_keys", true)?;
        // WAL：读写不互相阻塞。内存库会返回 memory，不用管。
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;

        let database = Self {
            conn: Mutex::new(conn),
            change_listener: Mutex::new(None),
        };
        database.migrate()?;
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
        run(&mut guard)
    }

    /// 在一个写事务里跑 `run`：返回 `Ok` 才提交，返回 `Err`（或 panic）整个回滚。
    ///
    /// 用 `IMMEDIATE`：一开始就拿写锁。多条语句的写操作里常有"先读后写"（查冲突再插入、
    /// 读旧值再合并），默认的延迟事务在读和写之间被别的连接（比如同时开着的另一个实例）
    /// 抢先写入时，要么读到过期结果，要么在升级写锁时直接报 busy。
    ///
    /// `run` 拿到的是事务里的连接（只给 `&Connection`，开不了嵌套事务）；
    /// 需要局部回滚的用 SAVEPOINT。
    pub(crate) fn with_tx<T>(
        &self,
        run: impl FnOnce(&Connection) -> Result<T, ZsshError>,
    ) -> Result<T, ZsshError> {
        self.with_conn(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = run(&tx)?;
            tx.commit()?;
            Ok(value)
        })
    }

    /// 写操作用：整个 `run` 是一个事务，**提交成功之后**才通知“数据变了”。
    /// 失败（含提交失败）时库回到调用前的样子，也不通知。
    pub(crate) fn with_conn_write<T>(
        &self,
        run: impl FnOnce(&Connection) -> Result<T, ZsshError>,
    ) -> Result<T, ZsshError> {
        let result = self.with_tx(run);
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
        run(&mut guard)
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
