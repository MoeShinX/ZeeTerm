//! 本地写入的结果信号和"写入意图"。
//!
//! 现状是 `send` 不返回任何东西：锁屏、连接未就绪、队列满、通道已关，调用方都看不出来，
//! 输入跟踪就会把没发出去的字符当成已经输入。所以每次本地写入都要有一个可区分的结果。
//!
//! `Queued` 只表示"本地接受了、排进了发送队列"，**不是**服务器收到或回显的证据；
//! 是否真的回显，只能靠屏幕上的单元格确认（见 [`super::echo`]）。

use std::time::Instant;

use super::state::RequestIdentity;

/// 一次本地写入的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// 本地接受并排进了发送队列。不代表送达，更不代表回显。
    Queued,
    /// 本地就拒绝了，一个字节都没有发出去。
    Rejected(RejectReason),
    /// 交给传输层时出错。传输层可能已经写出去了一部分，所以内容是否到达远端不确定。
    TransportFailed,
}

impl WriteOutcome {
    pub fn is_queued(self) -> bool {
        matches!(self, Self::Queued)
    }
}

/// 本地拒绝写入的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// 窗格不在 Live 状态（连接中、已断开、空白标签）。
    NotLive,
    /// 应用锁屏中。
    Locked,
    /// 发送队列满了（输入被丢弃）。
    QueueFull,
    /// 没有可写的内容。
    Empty,
}

/// 这次写入的内容是怎么来的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentOrigin {
    /// 用户在终端里逐键输入的字符。
    Manual,
    /// 输入法的最终提交。
    ImeCommit,
    /// 用户明确接受的候选，只发后缀（P4 用）。
    Accept,
}

/// 写入意图的处置状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// 已登记，还没拿到发送结果（同步回环回显会在这个阶段到达，所以必须先登记再发送）。
    Pending,
    Queued,
    Rejected(RejectReason),
    TransportFailed,
}

/// 先登记、再发送的写入意图。`local_seq` 只关联本地请求：远端字节不携带它，
/// 所以不能据此宣称"远端已经回显了第 N 次写入"。
#[derive(Clone, Debug)]
pub struct WriteIntent {
    pub local_seq: u64,
    pub origin: IntentOrigin,
    /// 登记那一刻的请求身份。
    pub identity: RequestIdentity,
    /// 登记前预测的输入文本（光标前）。
    pub expected_base: String,
    /// 这次写入追加在光标前文本后面的内容。
    pub append_suffix: String,
    /// 回显必须在这之前被屏幕确认，否则失同步。从登记起算，不会被持续输出延长。
    pub deadline: Instant,
    pub disposition: Disposition,
    /// 这条意图的内容在"待确认后缀"累计序列里的结束偏移（字节）。
    pub(crate) end_offset: usize,
}
