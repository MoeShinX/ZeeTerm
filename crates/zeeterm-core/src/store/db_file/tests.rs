use std::cell::Cell;
use std::io;
use std::path::Path;

use super::*;
use crate::store::Database;
use crate::test_util::TestDir;

/// 1.x 的库文件名（迁移只认这个）。
const LEGACY: &str = "zssh.db";

fn write_legacy(dir: &Path, key: &str, value: &str) {
    // 旧名字不是标准库文件名，所以这里不会触发迁移，等于旧版本建出来的库。
    let db = Database::open(&dir.join(LEGACY)).expect("建旧库");
    db.setting_set(key, value).expect("写设置");
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .expect("读目录")
        .map(|entry| {
            entry
                .expect("目录项")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn db_path_is_the_canonical_file_in_the_data_dir() {
    assert_eq!(
        db_path(Path::new("data")),
        Path::new("data").join("zeeterm.db")
    );
    assert_eq!(DB_FILE_NAME, "zeeterm.db");
}

#[test]
fn only_a_legacy_db_is_migrated_and_opening_the_new_path_reads_the_old_data() {
    let dir = TestDir::new("db-file-only-legacy");
    write_legacy(&dir, "host_note", "kept across the rename");
    assert_eq!(names(&dir), vec![LEGACY.to_owned()]);

    // 不先调迁移函数：直接按新路径打开，就是产品里所有调用点的做法。
    let db = Database::open(&db_path(&dir)).expect("打开新路径");
    assert_eq!(
        db.setting_get("host_note").expect("读设置").as_deref(),
        Some("kept across the rename")
    );
    drop(db);
    assert_eq!(
        names(&dir),
        vec![DB_FILE_NAME.to_owned()],
        "旧文件改名了，没有留下副本"
    );
    assert_eq!(
        migrate_legacy_db(&dir).expect("再迁一次"),
        LegacyDbMigration::NotNeeded,
        "迁完之后再调是空操作"
    );
}

#[test]
fn transactions_still_in_the_wal_are_not_lost() {
    let src = TestDir::new("db-file-wal-src");
    let dir = TestDir::new("db-file-wal");

    let legacy = Database::open(&src.join(LEGACY)).expect("建旧库");
    legacy
        .with_conn(|conn| Ok(conn.pragma_update(None, "wal_autocheckpoint", 0)?))
        .expect("关自动 checkpoint");
    legacy
        .setting_set("only_in_wal", "not merged")
        .expect("写设置");
    // 库还开着、一次 checkpoint 都没做：把三个文件原样拷走，等于进程还没来得及收尾就没了。
    for name in [LEGACY, "zssh.db-wal", "zssh.db-shm"] {
        std::fs::copy(src.join(name), dir.join(name)).expect("拷出库文件");
    }
    assert!(
        std::fs::metadata(dir.join("zssh.db-wal"))
            .expect("wal")
            .len()
            > 0,
        "数据在 wal 里"
    );
    drop(legacy);

    // 对照：只有主文件、没有 wal 的话，这条设置读不出来——说明下面读得出来靠的是 wal 一起搬了。
    let bare = TestDir::new("db-file-wal-bare");
    std::fs::copy(dir.join(LEGACY), bare.join("bare.db")).expect("只拷主文件");
    let bare_db = Database::open(&bare.join("bare.db")).expect("开对照库");
    assert_eq!(bare_db.setting_get("only_in_wal").expect("读设置"), None);
    drop(bare_db);

    assert_eq!(
        migrate_legacy_db(&dir).expect("迁移"),
        LegacyDbMigration::Migrated
    );
    for old in [LEGACY, "zssh.db-wal", "zssh.db-shm"] {
        assert!(!dir.join(old).exists(), "{old} 应该已经改名");
    }
    let db = Database::open(&db_path(&dir)).expect("打开新库");
    assert_eq!(
        db.setting_get("only_in_wal").expect("读设置").as_deref(),
        Some("not merged")
    );
}

#[test]
fn when_both_exist_nothing_is_touched_and_the_new_db_is_used() {
    let dir = TestDir::new("db-file-both");
    write_legacy(&dir, "which", "legacy");
    {
        let new_db = Database::open(&dir.join(DB_FILE_NAME)).expect("建新库");
        new_db.setting_set("which", "new").expect("写设置");
    }
    // 旧库旁边再放点伴随文件，确认它们也没被动。
    std::fs::write(dir.join("zssh.db-wal"), b"legacy wal bytes").expect("写 wal");
    let before: Vec<(String, Vec<u8>)> = names(&dir)
        .into_iter()
        .map(|name| {
            let bytes = std::fs::read(dir.join(&name)).expect("读文件");
            (name, bytes)
        })
        .collect();

    assert_eq!(
        migrate_legacy_db(&dir).expect("迁移检查"),
        LegacyDbMigration::NotNeeded
    );
    let db = Database::open(&db_path(&dir)).expect("打开");
    assert_eq!(
        db.setting_get("which").expect("读设置").as_deref(),
        Some("new")
    );
    drop(db);

    let after: Vec<(String, Vec<u8>)> = names(&dir)
        .into_iter()
        .map(|name| {
            let bytes = std::fs::read(dir.join(&name)).expect("读文件");
            (name, bytes)
        })
        .collect();
    let untouched = |files: &[(String, Vec<u8>)], name: &str| {
        files
            .iter()
            .find(|(file, _)| file == name)
            .map(|(_, bytes)| bytes.clone())
    };
    for name in [LEGACY, "zssh.db-wal"] {
        assert_eq!(
            untouched(&before, name),
            untouched(&after, name),
            "{name} 被动了"
        );
    }
}

#[test]
fn only_a_new_db_means_nothing_to_do() {
    let dir = TestDir::new("db-file-only-new");
    {
        let db = Database::open(&db_path(&dir)).expect("建新库");
        db.setting_set("k", "v").expect("写设置");
    }
    let before = names(&dir);
    assert_eq!(
        migrate_legacy_db(&dir).expect("迁移检查"),
        LegacyDbMigration::NotNeeded
    );
    assert_eq!(names(&dir), before);
    let db = Database::open(&db_path(&dir)).expect("打开");
    assert_eq!(db.setting_get("k").expect("读设置").as_deref(), Some("v"));
}

#[test]
fn with_neither_a_fresh_zeeterm_db_is_created() {
    let dir = TestDir::new("db-file-neither");
    assert_eq!(
        migrate_legacy_db(&dir).expect("迁移检查"),
        LegacyDbMigration::NotNeeded
    );
    assert!(names(&dir).is_empty());
    drop(Database::open(&db_path(&dir)).expect("新建"));
    assert_eq!(names(&dir), vec![DB_FILE_NAME.to_owned()]);
    assert!(!dir.join(LEGACY).exists(), "不会建出旧名字的库");
}

#[test]
fn other_legacy_looking_names_are_neither_read_nor_renamed() {
    let dir = TestDir::new("db-file-other-names");
    for name in [
        "zssh.db-journal",
        "zssh.db.bak",
        "ZSSH.DB.old",
        "zssh-sync.json",
    ] {
        std::fs::write(dir.join(name), name.as_bytes()).expect("写文件");
    }
    let before = names(&dir);
    assert_eq!(
        migrate_legacy_db(&dir).expect("迁移检查"),
        LegacyDbMigration::NotNeeded
    );
    assert_eq!(names(&dir), before, "没有 zssh.db 就什么都不认");
}

#[test]
fn a_failing_step_puts_the_files_back_and_reports_instead_of_creating_an_empty_db() {
    let dir = TestDir::new("db-file-rollback");
    for (name, bytes) in [
        (LEGACY, "main"),
        ("zssh.db-wal", "wal"),
        ("zssh.db-shm", "shm"),
    ] {
        std::fs::write(dir.join(name), bytes).expect("写文件");
    }

    // 伴随文件都搬完了，轮到主文件时失败。
    let err = migrate_with(&dir, |from, to| {
        if to.file_name().is_some_and(|name| name == DB_FILE_NAME) {
            Err(io::Error::other("locked by another process"))
        } else {
            std::fs::rename(from, to)
        }
    })
    .expect_err("要报错");
    let text = err.to_string();
    assert!(
        text.contains("zssh.db") && text.contains("zeeterm.db"),
        "{text}"
    );
    assert!(text.contains("locked by another process"), "{text}");
    assert_eq!(
        names(&dir),
        vec![
            LEGACY.to_owned(),
            "zssh.db-shm".to_owned(),
            "zssh.db-wal".to_owned()
        ],
        "三个文件都在原名上，没有任何新名字的文件"
    );
    assert_eq!(
        std::fs::read(dir.join("zssh.db-wal")).expect("读 wal"),
        b"wal"
    );
    assert_eq!(
        std::fs::read(dir.join("zssh.db-shm")).expect("读 shm"),
        b"shm"
    );
    assert_eq!(std::fs::read(dir.join(LEGACY)).expect("读主文件"), b"main");
}

#[test]
fn if_putting_back_also_fails_the_error_names_the_stuck_files() {
    let dir = TestDir::new("db-file-rollback-stuck");
    for (name, bytes) in [(LEGACY, "main"), ("zssh.db-wal", "wal")] {
        std::fs::write(dir.join(name), bytes).expect("写文件");
    }
    let calls = Cell::new(0);
    let err = migrate_with(&dir, |from, to| {
        calls.set(calls.get() + 1);
        match calls.get() {
            1 => std::fs::rename(from, to),
            _ => Err(io::Error::other("still locked")),
        }
    })
    .expect_err("要报错");
    let text = err.to_string();
    assert!(text.contains("没能退回原名"), "{text}");
    assert!(text.contains("zeeterm.db-wal"), "{text}");
}

#[test]
fn leftover_new_named_side_files_without_a_new_db_stop_the_open() {
    let dir = TestDir::new("db-file-orphans");
    write_legacy(&dir, "k", "v");
    std::fs::write(dir.join("zeeterm.db-wal"), b"orphan").expect("写孤儿 wal");
    let before = names(&dir);

    let err = migrate_legacy_db(&dir).expect_err("要报错");
    assert!(err.to_string().contains("zeeterm.db"), "{err}");
    // 按新路径打开也走同一道检查：报错，而且不会悄悄建一个空的 zeeterm.db。
    assert!(Database::open(&db_path(&dir)).is_err());
    assert_eq!(names(&dir), before);
}
