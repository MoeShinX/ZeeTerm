// 断线重建引擎后重试失败的传输行：跨主机复制必须带着原来的源主机（W04）。
// 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]` 属性宏。

use std::sync::{Arc, Weak};

use zssh_core::sftp::transfer::backend::{
    SftpEndpoints, TransferChannelProvider, TransferEndpoints,
};
use zssh_core::sftp::transfer::{Direction, TransferState};
use zssh_core::sftp::{SftpChannel, SftpError};
use zssh_core::ssh::BoxFuture;

use super::TransferRow;
use super::paste::PeerRef;
use super::transfer::plan_resubmit;

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
