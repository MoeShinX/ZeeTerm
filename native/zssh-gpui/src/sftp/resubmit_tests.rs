// 断线重建引擎后重试失败的传输行：跨主机复制必须带着原来的源主机（W04）。
// 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]` 属性宏。

use std::sync::{Arc, Weak};

use zssh_core::sftp::transfer::backend::{
    SftpEndpoints, TransferChannelProvider, TransferEndpoints,
};
use zssh_core::sftp::transfer::memory::MemoryEndpoints;
use zssh_core::sftp::transfer::{
    ConflictResolver, Direction, OverwriteAll, TransferEngine, TransferEvent, TransferJob,
    TransferOptions, TransferState,
};
use zssh_core::sftp::{SftpChannel, SftpError};
use zssh_core::ssh::BoxFuture;

use super::TransferRow;
use super::paste::PeerRef;
use super::transfer::{find_row, plan_resubmit};

/// 开通道一律失败，错误文字带着自己的标签：看错误就知道请求落在了哪台主机的数据面上。
struct Labelled(&'static str);

impl TransferChannelProvider for Labelled {
    fn channel<'a>(&'a self) -> BoxFuture<'a, Result<Arc<SftpChannel>, SftpError>> {
        let label = self.0;
        Box::pin(async move { Err(SftpError::Remote(format!("labelled:{label}"))) })
    }
}

fn failed_row(peer: Option<PeerRef>) -> TransferRow {
    let mut row = TransferRow::new(
        7,
        "report.txt".to_owned(),
        Direction::RemoteCopy,
        None,
        "/src/report.txt".to_owned(),
        "/dst/report.txt".to_owned(),
    );
    row.state = TransferState::Failed("connection lost".to_owned());
    row.peer = peer;
    row
}

fn peer_ref(name: &str, provider: &Arc<dyn TransferChannelProvider>) -> PeerRef {
    PeerRef {
        name: name.to_owned(),
        provider: Arc::downgrade(provider),
    }
}

/// 在 `endpoints` 上为复制任务建源 / 目标端点，返回失败的错误文字。
fn pair_error(endpoints: &SftpEndpoints, peer: Option<&str>) -> String {
    let runtime = crate::ssh_backend::runtime().expect("runtime");
    let result = runtime.block_on(endpoints.pair(
        Direction::RemoteCopy,
        peer,
        "/src/report.txt",
        "/dst/report.txt",
        "owner",
    ));
    match result {
        Ok(_) => panic!("测试里的数据面都开不了通道"),
        Err(err) => err.to_string(),
    }
}

/// 重试跨主机复制：任务带回原来的登记名，源主机的数据面重新登记进（重建后的）新引擎，
/// 源端落在源主机上，而不是目标主机。
#[test]
fn retrying_a_cross_host_copy_keeps_the_source_host() {
    let source: Arc<dyn TransferChannelProvider> = Arc::new(Labelled("source"));
    let row = failed_row(Some(peer_ref("view-3", &source)));

    let plan = plan_resubmit(&row).expect("源还开着，可以重试");
    assert_eq!(plan.job.peer.as_deref(), Some("view-3"), "不能丢掉 peer");
    assert_eq!(plan.job.direction, Direction::RemoteCopy);
    assert_eq!(plan.job.source, "/src/report.txt");
    assert_eq!(plan.job.target, "/dst/report.txt");
    let (name, provider) = plan.peer_provider.expect("要重新登记源主机");
    assert_eq!(name, "view-3");
    assert!(Arc::ptr_eq(&provider, &source));

    // 新引擎：只有目标主机，peers 是空的。登记之前，peer 找不到并明确报"源已关闭"，
    // 不会退回目标主机。
    let target: Arc<dyn TransferChannelProvider> = Arc::new(Labelled("target"));
    let rebuilt = SftpEndpoints::new(target);
    let before = pair_error(&rebuilt, plan.job.peer.as_deref());
    assert!(before.contains("已经关闭"), "{before}");
    assert!(!before.contains("labelled:target"), "{before}");
    // 按行登记之后，源端走源主机的数据面。
    rebuilt.register_peer(&name, provider);
    let after = pair_error(&rebuilt, plan.job.peer.as_deref());
    assert!(after.contains("labelled:source"), "{after}");
}

/// 源标签关了（弱引用升不上去）：拒绝重试并给出可见的原因，不降级成同主机复制。
#[test]
fn retrying_is_refused_when_the_source_connection_is_gone() {
    let dead: Weak<dyn TransferChannelProvider> = {
        let gone: Arc<dyn TransferChannelProvider> = Arc::new(Labelled("source"));
        Arc::downgrade(&gone)
    };
    let row = failed_row(Some(PeerRef {
        name: "view-3".to_owned(),
        provider: dead,
    }));
    let Err(message) = plan_resubmit(&row) else {
        panic!("源已关闭，不能重试");
    };
    assert!(message.contains("已经关闭"), "{message}");
}

/// 同一台主机上的复制没有 peer，重试也没有。
#[test]
fn retrying_a_same_host_copy_stays_same_host() {
    let plan = plan_resubmit(&failed_row(None)).expect("可以重试");
    assert_eq!(plan.job.peer, None);
    assert!(plan.peer_provider.is_none());
}

/// W05：断线重建引擎后，旧引擎留下的行和新引擎的任务各用各的行。
/// 两个引擎的任务号不相同；新引擎的进度只落在新行上，旧行不被覆盖，
/// 旧行的任务号在新引擎里也找不到（它的暂停 / 取消 / 重试不会命中新任务）。
#[tokio::test(flavor = "multi_thread")]
async fn reports_of_a_rebuilt_engine_never_land_on_rows_of_the_old_one() {
    let _dir_guard = tempfile::Builder::new()
        .prefix("zssh-ids-")
        .tempdir()
        .expect("建临时目录");
    let dir = _dir_guard.path().to_path_buf();
    let old_file = dir.join("old.bin");
    let new_file = dir.join("new.bin");
    std::fs::write(&old_file, vec![1u8; 10]).expect("写旧文件");
    std::fs::write(&new_file, vec![2u8; 20]).expect("写新文件");

    let engine_with_one_upload = |file: &std::path::Path, target: &str| {
        let resolver: Arc<dyn ConflictResolver> = Arc::new(OverwriteAll);
        let (engine, events) = TransferEngine::new(
            Arc::new(MemoryEndpoints::new("/remote")),
            resolver,
            TransferOptions::default(),
        );
        let id = engine.submit(TransferJob {
            direction: Direction::Upload,
            source: file.display().to_string(),
            target: target.to_owned(),
            recursive: false,
            peer: None,
        });
        (engine, events, id)
    };
    // 把一个任务的事件全部喂给「行表」，直到它结束。
    async fn feed(
        rows: &mut [TransferRow],
        events: &mut tokio::sync::mpsc::UnboundedReceiver<TransferEvent>,
        id: u64,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            assert!(std::time::Instant::now() < deadline, "任务 60 秒内没结束");
            let Ok(Some(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), events.recv()).await
            else {
                continue;
            };
            let (TransferEvent::Progress(report) | TransferEvent::Finished(report)) = event;
            if let Some(row) = find_row(rows, report.id) {
                row.apply(&report);
            }
            if report.id == id && report.state.is_terminal() {
                return;
            }
        }
    }
    let row_of = |id: u64, name: &str| {
        TransferRow::new(
            id,
            name.to_owned(),
            Direction::Upload,
            None,
            String::new(),
            String::new(),
        )
    };

    // 旧引擎：一个任务做完，它的行留在界面上。
    let (old_engine, mut old_events, old_id) = engine_with_one_upload(&old_file, "/remote/old.bin");
    let mut rows = vec![row_of(old_id, "old.bin")];
    feed(&mut rows, &mut old_events, old_id).await;
    assert_eq!(rows[0].state, TransferState::Done);
    assert_eq!(rows[0].total_bytes, 10);

    // 重建：新引擎提交新任务，新行跟在旧行后面。
    let (new_engine, mut new_events, new_id) = engine_with_one_upload(&new_file, "/remote/new.bin");
    assert_ne!(old_id, new_id, "任务号必须在进程内唯一");
    rows.push(row_of(new_id, "new.bin"));
    feed(&mut rows, &mut new_events, new_id).await;

    assert_eq!(rows[1].state, TransferState::Done);
    assert_eq!(rows[1].total_bytes, 20, "新任务的进度落在新行上");
    assert_eq!(rows[0].total_bytes, 10, "旧行不能被新任务的进度覆盖");

    // 旧行的操作不会命中新任务，反过来也一样。
    assert!(!new_engine.cancel(old_id));
    assert!(!new_engine.retry(old_id));
    assert!(!old_engine.cancel(new_id));
}
