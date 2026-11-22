//! 本机数据库文件的位置，以及旧文件名 `zssh.db` 到 `zeeterm.db` 的一次性迁移。
//!
//! 1.x 版本把库叫 `zssh.db`；产品改名后统一叫 `zeeterm.db`。已经装了 1.x 的用户升级后
//! 不能丢掉主机和保险库，所以这里是整个仓库里**唯一**还认旧名字的地方：
//!
//! - 只认 `zssh.db`、`zssh.db-wal`、`zssh.db-shm` 这三个文件名，别的旧名一概不读不认；
//! - 只在 `zeeterm.db`、`zeeterm.db-wal`、`zeeterm.db-shm` 一个都不存在、而 `zssh.db` 存在时才动手；
//! - 先搬 `-wal`、`-shm`（还没合并进主文件的事务在里面），最后搬主文件；
//! - 任何一步失败就把已经搬过的退回原名并报错，**绝不**在这时候让上层去打开一个空的新库——
//!   那样用户会以为数据没了。
//!
//! 所有打开本机库的地方都用 [`db_path`] 拿路径；[`Database::open`](super::Database::open) 对
//! `zeeterm.db` 这个标准文件名会先过一遍这里，所以不管谁先打开，迁移一定发生在第一次打开之前。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Reason, ZeetermError};

/// 本机库的文件名。
pub const DB_FILE_NAME: &str = "zeeterm.db";

/// 1.x 的库文件名：只给一次性迁移用。
const LEGACY_DB_FILE_NAME: &str = "zssh.db";
/// SQLite WAL 模式的两个伴随文件后缀。
const SIDE_SUFFIXES: [&str; 2] = ["-wal", "-shm"];

/// 同一进程里的多个线程可能同时第一次打开库：迁移串行做，后到的看到的已经是迁移之后的样子。
static MIGRATE_LOCK: Mutex<()> = Mutex::new(());

/// 数据目录下的本机库路径。
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE_NAME)
}

/// 一次迁移检查的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyDbMigration {
    /// 没有要迁移的东西（没有旧库，或者新库已经在了）：什么文件都没动。
    NotNeeded,
    /// 旧库（含 `-wal`、`-shm`）已经改名成新名字。
    Migrated,
}

/// 必要时把 `zssh.db` 迁移成 `zeeterm.db`。重复调用安全：迁移完之后再调就是 `NotNeeded`。
pub fn migrate_legacy_db(data_dir: &Path) -> Result<LegacyDbMigration, ZeetermError> {
    migrate_with(data_dir, |from, to| std::fs::rename(from, to))
}

/// [`migrate_legacy_db`] 的实现；改名动作由调用方给，测试用它模拟中途失败。
pub(crate) fn migrate_with(
    data_dir: &Path,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<LegacyDbMigration, ZeetermError> {
    // 锁里只做文件系统操作，不会 panic；万一被毒化也照样往下走（状态全在磁盘上）。
    let _guard = MIGRATE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let new_db = data_dir.join(DB_FILE_NAME);
    let legacy_db = data_dir.join(LEGACY_DB_FILE_NAME);
    let present = |path: &Path| exists(path).map_err(|err| failure(path, &err));

    // 新库已经在（含"新旧都有"）：不动任何文件，用新库。
    if present(&new_db)? {
        return Ok(LegacyDbMigration::NotNeeded);
    }
    // 没有旧库（含全新安装）：交给调用方正常新建。
    if !present(&legacy_db)? {
        return Ok(LegacyDbMigration::NotNeeded);
    }
    // 新库不在，却留着新名字的 -wal / -shm：上一次迁移被打断、又没退回干净。照常打开会让
    // SQLite 把这些孤儿文件当成一个新库的日志，不能碰，让用户看到错误。
    let mut moves = Vec::new();
    for suffix in SIDE_SUFFIXES {
        let new_side = side_path(&new_db, suffix);
        if present(&new_side)? {
            return Err(failure(
                &new_side,
                &io::Error::other("新名字的 -wal / -shm 已经存在，但 zeeterm.db 不在"),
            ));
        }
        let legacy_side = side_path(&legacy_db, suffix);
        if present(&legacy_side)? {
            moves.push((legacy_side, new_side));
        }
    }
    // 主文件最后搬：中途失败时退回的是伴随文件，主文件一直没动过。
    moves.push((legacy_db, new_db));

    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for step in &moves {
        if let Err(err) = rename(&step.0, &step.1) {
            let mut stuck = Vec::new();
            for (from, to) in done.iter().rev().map(|step| (&step.0, &step.1)) {
                if let Err(undo) = rename(to, from) {
                    stuck.push(format!("{}（{undo}）", to.display()));
                }
            }
            let mut failed = failure(&step.0, &err);
            if !stuck.is_empty() {
                failed = ZeetermError::Internal(Reason::text(format!(
                    "{failed}；而且这些文件没能退回原名：{}",
                    stuck.join("、")
                )));
            }
            return Err(failed);
        }
        done.push(step);
    }
    Ok(LegacyDbMigration::Migrated)
}

/// `<库文件>-wal` / `<库文件>-shm`。
fn side_path(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// 路径上有没有东西（符号链接本身也算）；读不出来就报错，不当成"没有"。
fn exists(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn failure(path: &Path, err: &io::Error) -> ZeetermError {
    ZeetermError::Internal(Reason::text(format!(
        "无法把旧数据库 zssh.db 迁移成 zeeterm.db（{}：{err}）。没有创建新库，原来的文件保持原样。\
         请关闭其他正在运行的 ZeeTerm 后重试。 / \
         Could not migrate the old database zssh.db to zeeterm.db ({}: {err}). No new database was \
         created and the original files were left in place. Close any other running ZeeTerm and try again.",
        path.display(),
        path.display()
    )))
}

#[cfg(test)]
mod tests;
