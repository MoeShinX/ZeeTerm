//! 从本机的 Z-SSH（0.2.x 的 Tauri 版）读数据，迁到 ZeeTerm。
//!
//! 为什么能直接读：两个产品共用同一套核心库。Z-SSH 的库是 `%APPDATA%\Z-ssh\data.db`（SQLite，
//! 迁移版本 1～11），ZeeTerm 的 schema 是它的超集（12、13 只是加列、加表）；凭据、代理密码用
//! Windows DPAPI（当前用户 + 固定的应用级 entropy）加密，两个产品的 entropy 相同，所以**同一个
//! Windows 用户**下 ZeeTerm 能直接解开。换了用户或换了电脑解不开，这时只能走 Z-SSH 的离线备份文件。
//!
//! 读的办法：把库（连同 `-wal`）和 `known_hosts` 复制到数据目录下的临时目录，在副本上跑迁移、
//! 读出一份 [`SyncSnapshot`]，再删掉副本。**绝不打开原库写入**，也就不会误改 Z-SSH 的数据；
//! 副本上的迁移只发生在副本里。Z-SSH 正在运行时它占着库文件，复制会读到写了一半的状态，
//! 所以先探测，占着就让用户先退出 Z-SSH（[`ZsshImportError::InUse`]）。
//!
//! 读出来的快照交给 [`SyncEngine::import_snapshot`] / [`SyncEngine::preview_import`]，和离线备份
//! 文件走同一条导入路径（合并 / 替换、导入前自动备份、不动同步基线）。

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::error::SyncError;
use super::package::SyncSnapshot;
use super::sealer::SecretSealer;
use super::settings::SettingsPolicy;
use super::snapshot::snapshot_local_with;
use crate::store::db::MIGRATIONS;
use crate::store::Database;

/// Z-SSH 的默认数据目录名（`%APPDATA%` 下）。Windows 路径不区分大小写，`Z-SSH` 也是同一个。
pub const ZSSH_APP_DIR: &str = "Z-ssh";
/// Z-SSH 的库文件名（ZeeTerm 自己的叫 `zssh.db`）。
pub const ZSSH_DB_FILE: &str = "data.db";
const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// "不再提示迁移"的标记（本机设置，不同步）。
pub const MIGRATE_DISMISSED_KEY: &str = "zssh_migrate_dismissed";

/// 找 Z-SSH 数据目录要用的环境。拆出来是为了测试能注入假路径，不碰真实目录。
#[derive(Debug, Clone, Default)]
pub struct DetectEnv {
    /// 明确指定的 Z-SSH 数据目录（用户在界面里手动选的；调试构建里还有测试钩子）。最优先。
    pub explicit_dir: Option<PathBuf>,
    /// `%APPDATA%`（Tauri 的 `data_dir()` 就是它）。
    pub appdata: Option<PathBuf>,
    /// ZeeTerm 自己的数据目录：用户把两个产品的数据目录设成同一个时，不能把自己当 Z-SSH 导入自己。
    pub own_data_dir: Option<PathBuf>,
}

impl DetectEnv {
    /// 按当前进程的环境组装。
    pub fn from_process(own_data_dir: PathBuf, explicit_dir: Option<PathBuf>) -> Self {
        Self {
            explicit_dir,
            appdata: std::env::var_os("APPDATA")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            own_data_dir: Some(own_data_dir),
        }
    }

    /// 按优先级排好的候选数据目录。
    pub fn candidate_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(dir) = &self.explicit_dir {
            dirs.push(dir.clone());
        }
        if let Some(appdata) = &self.appdata {
            dirs.push(appdata.join(ZSSH_APP_DIR));
        }
        dirs
    }
}

/// 找到的一份 Z-SSH 数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZsshSource {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// 库文件大小（字节）。
    pub db_bytes: u64,
    /// 库文件最后修改时间（Unix 秒）；读不出来是 `None`。
    pub modified_at: Option<i64>,
}

/// 在候选目录里找第一份有效的 Z-SSH 数据（有非空的 `data.db`）。只看文件在不在，不打开库。
pub fn detect(env: &DetectEnv) -> Option<ZsshSource> {
    env.candidate_dirs()
        .into_iter()
        .filter(|dir| {
            !env.own_data_dir
                .as_deref()
                .is_some_and(|own| same_dir(dir, own))
        })
        .find_map(|dir| source_at(&dir))
}

/// 用户指定的目录：和 [`detect`] 一样的判断，但自己的数据目录也要拒绝。
pub fn source_at(dir: &Path) -> Option<ZsshSource> {
    let db_path = dir.join(ZSSH_DB_FILE);
    let meta = std::fs::metadata(&db_path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    let modified_at = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    Some(ZsshSource {
        data_dir: dir.to_path_buf(),
        db_path,
        db_bytes: meta.len(),
        modified_at,
    })
}

/// 两个路径是不是同一个目录：能解析就按真实路径比，不能就按不区分大小写的文本比。
pub fn same_dir(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return a == b;
    }
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    norm(a) == norm(b)
}

#[derive(Debug, thiserror::Error)]
pub enum ZsshImportError {
    /// Z-SSH 开着：它一直占着库文件，这时复制可能读到写了一半的状态。
    #[error("Z-SSH 正在运行，数据库被它占着：请先退出 Z-SSH 再导入")]
    InUse,

    #[error("这里不是有效的 Z-SSH 数据：{0}")]
    NotZsshData(String),

    #[error(
        "Z-SSH 的数据库版本（v{found}）比这个 ZeeTerm 能读的（v{supported}）新，请先升级 ZeeTerm"
    )]
    TooNew { found: i64, supported: i64 },

    /// 库里的密码解不开：多半是另一个 Windows 用户（或另一台电脑）上的 Z-SSH 留下的。
    #[error(
        "Z-SSH 里保存的密码解不开（只有保存它们的那个 Windows 用户能解）：请改用 Z-SSH 导出的\
         备份文件导入"
    )]
    Undecryptable,

    #[error("读取 Z-SSH 的数据失败：{0}")]
    Unreadable(String),
}

/// 读出来的结果。
#[derive(Debug)]
pub struct ZsshRead {
    pub snapshot: SyncSnapshot,
    /// Z-SSH 库的版本号（读的时候的，升级发生在副本上）。
    pub schema_version: i64,
    /// 没读全的地方（比如 known_hosts 读不了）：不影响导入，但要告诉用户。
    pub notes: Vec<String>,
}

/// Z-SSH 的库现在有没有被别的进程占着。
///
/// SQLite 打开库时给的共享方式允许别人读写，我们用"不共享"的方式试着打开：只要 Z-SSH（或任何
/// 进程）还拿着句柄就会被拒绝。只读、立刻放手，不会碰库里的内容。
pub fn is_in_use(db_path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(db_path)
        {
            Ok(_) => false,
            Err(err) => is_sharing_violation(&err),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = db_path;
        false
    }
}

/// ERROR_SHARING_VIOLATION（32）/ ERROR_LOCK_VIOLATION（33）。
fn is_sharing_violation(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(32) | Some(33))
}

/// 临时目录守卫：副本里有（DPAPI 加密的）凭据，用完立刻删。
struct Scratch(PathBuf);

impl Scratch {
    fn new(root: &Path) -> Result<Self, ZsshImportError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = root.join(format!("zssh-import-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|err| ZsshImportError::Unreadable(format!("建不了临时目录：{err}")))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SQLite 的后台句柄偶尔晚一点放，重试几次。
        for _ in 0..10 {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        tracing::warn!("Z-SSH 导入的临时目录没删掉，下次启动前请手动清理");
    }
}

/// 复制一个文件；占用冲突按"Z-SSH 在运行"处理。错误文本不带路径（路径里有用户名）。
fn copy_file(from: &Path, to: &Path) -> Result<(), ZsshImportError> {
    std::fs::copy(from, to).map(|_| ()).map_err(|err| {
        if is_sharing_violation(&err) {
            ZsshImportError::InUse
        } else if err.kind() == std::io::ErrorKind::NotFound {
            ZsshImportError::NotZsshData("找不到数据库文件".to_owned())
        } else {
            ZsshImportError::Unreadable(format!("复制数据库失败（{}）", err.kind()))
        }
    })
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// 读出 Z-SSH 库里的全部可迁移数据。`scratch_root` 是放临时副本的目录（ZeeTerm 的数据目录）。
///
/// 阻塞操作（复制、SQLite、DPAPI），调用方放在后台线程里。
pub fn read_zssh(
    source: &ZsshSource,
    sealer: &dyn SecretSealer,
    scratch_root: &Path,
) -> Result<ZsshRead, ZsshImportError> {
    if is_in_use(&source.db_path) {
        return Err(ZsshImportError::InUse);
    }
    let scratch = Scratch::new(scratch_root)?;
    let copy = scratch.0.join(ZSSH_DB_FILE);
    copy_file(&source.db_path, &copy)?;
    // 上次没正常退出时还有没合并进主文件的 -wal：一起带上，SQLite 打开副本时会自己恢复。
    let wal = sibling(&source.db_path, "-wal");
    if wal.is_file() {
        copy_file(&wal, &sibling(&copy, "-wal"))?;
    }
    let mut notes = Vec::new();
    let known_hosts_copy = scratch.0.join(KNOWN_HOSTS_FILE);
    let have_known_hosts = {
        let original = source.data_dir.join(KNOWN_HOSTS_FILE);
        if original.is_file() {
            match std::fs::copy(&original, &known_hosts_copy) {
                Ok(_) => true,
                Err(_) => {
                    notes.push("已信任的主机指纹没能读出来，没有导入".to_owned());
                    false
                }
            }
        } else {
            false
        }
    };

    let schema_version = copy_schema_version(&copy)?;
    let supported = MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0);
    if schema_version > supported {
        return Err(ZsshImportError::TooNew {
            found: schema_version,
            supported,
        });
    }

    let snapshot = {
        // 副本上升级到当前版本（只改副本），再按同步快照的办法读。
        let db = Database::open(&copy)
            .map_err(|err| ZsshImportError::Unreadable(format!("打开数据库副本失败：{err}")))?;
        let device_id = db
            .sync_device_id()
            .map_err(|err| ZsshImportError::Unreadable(err.to_string()))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        let read = |known_hosts: Option<&Path>| {
            snapshot_local_with(
                &db,
                sealer,
                &device_id,
                known_hosts,
                now,
                &SettingsPolicy::CURRENT,
            )
        };
        let result = if have_known_hosts {
            match read(Some(&known_hosts_copy)) {
                // 指纹文件坏了（不是文本之类）：不让它拦住整次导入。
                Err(SyncError::Io(_)) => {
                    notes.push("已信任的主机指纹文件读不了，没有导入".to_owned());
                    read(None)
                }
                other => other,
            }
        } else {
            read(None)
        };
        let mut snapshot = result.map_err(map_snapshot_error)?;
        // 墓碑是"Z-SSH 那边删过什么"，带过来只会误删 ZeeTerm 里的东西。
        snapshot.tombstones.clear();
        snapshot
    };
    Ok(ZsshRead {
        snapshot,
        schema_version,
        notes,
    })
}

/// 副本库的迁移版本。不是 SQLite 库、或没有 `schema_version` 表都说成"不是 Z-SSH 数据"。
fn copy_schema_version(copy: &Path) -> Result<i64, ZsshImportError> {
    let conn = Connection::open(copy)
        .map_err(|err| ZsshImportError::NotZsshData(format!("打不开：{err}")))?;
    conn.query_row(
        "SELECT coalesce(max(version), 0) FROM schema_version",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map_err(|_| ZsshImportError::NotZsshData("库里没有 Z-SSH 的版本记录".to_owned()))
}

/// 解密失败单独说（用户要的是"换条路"，不是一句数据库错误）。
fn map_snapshot_error(err: SyncError) -> ZsshImportError {
    let text = err.to_string();
    if text.contains(DECRYPT_FAILED_MARK) {
        ZsshImportError::Undecryptable
    } else {
        ZsshImportError::Unreadable(text)
    }
}

/// `snapshot::decrypt_secret` 解密失败时错误文本里的固定片段；有测试钉住它没被改掉。
const DECRYPT_FAILED_MARK: &str = "密文解不开";

#[cfg(test)]
mod tests;

/// 错误码与结构化参数：界面按它们出当前语言的整句。
impl crate::error::ErrorWithCode for ZsshImportError {
    fn code(&self) -> &'static str {
        match self {
            ZsshImportError::InUse => "import.in_use",
            ZsshImportError::NotZsshData(_) => "import.not_data",
            ZsshImportError::TooNew { .. } => "import.too_new",
            ZsshImportError::Undecryptable => "import.undecryptable",
            ZsshImportError::Unreadable(_) => "import.unreadable",
        }
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        match self {
            ZsshImportError::NotZsshData(detail) | ZsshImportError::Unreadable(detail) => {
                vec![("detail", detail.clone())]
            }
            ZsshImportError::TooNew { found, supported } => vec![
                ("found", found.to_string()),
                ("supported", supported.to_string()),
            ],
            ZsshImportError::InUse | ZsshImportError::Undecryptable => Vec::new(),
        }
    }
}
