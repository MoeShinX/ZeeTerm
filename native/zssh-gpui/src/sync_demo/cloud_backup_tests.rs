//! U33：云端备份目标在界面这一层的路径（不连任何真实云存储）。
//!
//! 被测的入口就是设置页调的那几个函数：`run_sync_with`（立即同步）、`backup_after_sync`（同步成功后顺带备份）、
//! `backup_with`（某个目标的「立即备份」）、`restore_with`（「从这里恢复」）。云端是 `sync_stub` 里的本机桩，
//! 主同步和备份目标指向桩上同一个目录。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use zssh_core::store::Database;
use zssh_core::sync::backup_targets::{self, OutcomeStatus};
use zssh_core::sync::engine::SyncStatus;
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::service::{self, StorageConfig};

use super::cloud_backup::{
    CloudBackup, backup_after_sync, backup_with, last_text, resolve_password, restore_with,
};
use super::run_sync_with;
use crate::sync_stub::{DAV_PASS, MASTER_PASSWORD, StubServer, XorSealer, draft_for_tests};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

struct Device {
    dir: PathBuf,
    db: Database,
}

impl Device {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("zssh-u33-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        Self { dir, db }
    }

    fn sealer() -> XorSealer {
        XorSealer(0x5A)
    }

    fn add_host(&self, name: &str) {
        self.db.host_save(&draft_for_tests(name)).expect("建主机");
    }

    fn host_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .db
            .host_list()
            .expect("读主机")
            .into_iter()
            .map(|host| host.name)
            .collect();
        names.sort();
        names
    }

    fn add_target(&self, config: StorageConfig) -> String {
        backup_targets::add_target(&self.db, &Self::sealer(), None, config, true)
            .expect("添加备份目标")
            .id
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_successful_sync_is_followed_by_a_backup_to_every_target_and_a_bad_one_stays_isolated() {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let device = Device::new("after-sync");
    device.add_host("web-1");
    let main = server.storage_config("webdav", "").expect("设置");
    service::save_config(&device.db, &Device::sealer(), &main).expect("保存主同步");
    // 目标 1：和主同步完全相同的地址、目录、账号；目标 2：密码是错的
    let good = device.add_target(main.clone());
    let mut wrong = main;
    if let StorageConfig::Webdav(config) = &mut wrong {
        config.password = "not-the-password".to_owned();
    }
    let bad = device.add_target(wrong);

    let report = run_sync_with(
        runtime.handle(),
        &device.dir,
        &Device::sealer(),
        MASTER_PASSWORD,
        Some(DeletionDecision::Continue),
    )
    .expect("主同步");
    assert_eq!(report.status, SyncStatus::Completed);
    backup_after_sync(
        runtime.handle(),
        &device.dir,
        &Device::sealer(),
        MASTER_PASSWORD,
    );

    // 同一个目录里两个对象：同步包和备份，互不覆盖
    assert_eq!(
        server.with(|state| state.object_keys()),
        vec![
            "/dav/zssh/zeeterm-backup.json".to_owned(),
            "/dav/zssh/zssh-sync.json".to_owned()
        ]
    );
    // 结果只记在各自的目标上
    let targets = backup_targets::load_targets(&device.db, &Device::sealer()).unwrap();
    let last = |id: &str| targets.iter().find(|t| t.id == id).unwrap();
    assert!(last(&good).last.as_ref().unwrap().ok);
    let failed = last(&bad).last.as_ref().unwrap();
    assert!(
        !failed.ok && !failed.message.contains("not-the-password"),
        "{failed:?}"
    );
    // 界面上的一行字
    assert!(last_text(last(&good)).1.contains("成功"));
    let (ok, text) = last_text(last(&bad));
    assert!(!ok && text.contains("失败"), "{text}");
    assert!(
        text.find(DAV_PASS).is_none(),
        "失败原因里不能带密码：{text}"
    );

    // 数据没变：再来一次自动备份，成功的那个目标不再上传（自动同步每隔几分钟一次，不能每次都重传）
    let puts = |server: &StubServer| {
        server.with(|state| state.object_requests("PUT", "zeeterm-backup.json"))
    };
    assert_eq!(puts(&server), 1);
    backup_after_sync(
        runtime.handle(),
        &device.dir,
        &Device::sealer(),
        MASTER_PASSWORD,
    );
    assert_eq!(puts(&server), 1, "数据没变，不重复上传");
    // 本机数据变了才会再传
    device.add_host("web-2");
    backup_after_sync(
        runtime.handle(),
        &device.dir,
        &Device::sealer(),
        MASTER_PASSWORD,
    );
    assert_eq!(puts(&server), 2);
}

#[test]
fn manual_backup_needs_no_main_sync_and_a_short_password_never_reaches_the_server() {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let device = Device::new("manual");
    device.add_host("web-1");
    // 没有配置主同步，只有一个备份目标
    let id = device.add_target(server.storage_config("webdav", "").expect("设置"));
    assert!(
        service::load_config(&device.db, &Device::sealer())
            .unwrap()
            .is_none()
    );
    let outcomes = backup_with(
        runtime.handle(),
        &device.dir,
        &Device::sealer(),
        MASTER_PASSWORD,
        Some(&id),
    )
    .expect("立即备份");
    assert_eq!(outcomes[0].status, OutcomeStatus::Uploaded);
    assert_eq!(
        server.with(|state| state.object_keys()),
        vec!["/dav/zssh/zeeterm-backup.json".to_owned()]
    );
    // 主密码太短：不碰服务
    let before = server.with(|state| state.requests());
    let err = backup_with(
        runtime.handle(),
        &device.dir,
        &Device::sealer(),
        "short",
        Some(&id),
    )
    .unwrap_err();
    assert!(err.contains("至少"), "{err}");
    assert_eq!(server.with(|state| state.requests()), before);
}

#[test]
fn restoring_from_a_target_replaces_this_computers_data_and_keeps_a_local_backup() {
    let (runtime, server) = (runtime(), StubServer::start().expect("桩"));
    let source = Device::new("restore-src");
    source.add_host("from-cloud-1");
    source.add_host("from-cloud-2");
    let config = server.storage_config("webdav", "").expect("设置");
    let id = source.add_target(config.clone());
    backup_with(
        runtime.handle(),
        &source.dir,
        &Device::sealer(),
        MASTER_PASSWORD,
        Some(&id),
    )
    .expect("备份");

    let target = Device::new("restore-dst");
    target.add_host("only-here");
    let dst_id = target.add_target(config);
    let wrong = restore_with(
        runtime.handle(),
        &target.dir,
        &Device::sealer(),
        &dst_id,
        "wrong-master-password",
    )
    .unwrap_err();
    assert!(wrong.contains("主密码"), "{wrong}");
    assert_eq!(target.host_names(), vec!["only-here"], "密码错一个字节不动");

    let report = restore_with(
        runtime.handle(),
        &target.dir,
        &Device::sealer(),
        &dst_id,
        MASTER_PASSWORD,
    )
    .expect("恢复");
    assert_eq!(target.host_names(), vec!["from-cloud-1", "from-cloud-2"]);
    assert!(report.safety_backup.name.starts_with("zssh-backup-"));
    // 备份目标列表还在
    assert_eq!(
        backup_targets::load_targets(&target.db, &Device::sealer())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_empty_data_directory_has_no_targets_and_is_not_created_just_to_look() {
    let dir = std::env::temp_dir().join(format!("zssh-u33-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cloud = CloudBackup::load(&dir);
    assert!(cloud.targets.is_empty() && cloud.load_error.is_none());
    assert!(!dir.exists(), "为了读列表不能建数据目录 / 库");
}

#[test]
fn the_password_for_a_backup_comes_from_the_box_or_the_remembered_one() {
    let remembered = || Some("remembered-password".to_owned());
    // 输入框里有就用输入的
    assert_eq!(
        resolve_password("typed-password-1", "", remembered(), false).unwrap(),
        "typed-password-1"
    );
    // 没输就用记住的
    assert_eq!(
        resolve_password("", "", remembered(), false).unwrap(),
        "remembered-password"
    );
    // 都没有
    assert!(
        resolve_password("", "", None, false)
            .unwrap_err()
            .contains("请先输入")
    );
    // 太短
    assert!(
        resolve_password("short", "short", None, true)
            .unwrap_err()
            .contains("至少")
    );
    // 单独用云端备份：要再输一遍，两遍不一样就拒绝
    assert!(
        resolve_password("typed-password-1", "typed-password-2", None, true)
            .unwrap_err()
            .contains("不一样")
    );
    assert!(resolve_password("typed-password-1", "typed-password-1", None, true).is_ok());
    // 用记住的不用再输一遍
    assert!(resolve_password("", "", remembered(), true).is_ok());
}
