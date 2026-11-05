//! opencode / codex / claude 三个驱动共用的运行时生命周期小工具（代码审查去重项）。
//!
//! 这里只放和协议无关、三份实现里重复的东西：重试超限后的停止编排、系统随机数。
//! 各驱动自己的中止路径（opencode 的 `post_abort`、codex 的 `turn/interrupt`、claude 的
//! interrupt 控制消息）留在各自模块：请求怎么发、失败日志怎么写本来就不同。

use std::future::Future;

use super::retry::RETRY_STOP_GRACE;

/// 系统随机数 → 小写十六进制（能力密钥、端口密码、临时文件名、执行编号前缀）。
///
/// 系统随机数（BCryptGenRandom）在 Windows 10/11 上不会失败；真失败了也不能退回可预测的
/// 令牌，宁可 panic。
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut buf).expect("系统随机数不可用");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 重试超限后的停止编排：先发中止请求，宽限期过后兜底收尾。
///
/// - 中止请求单独 `spawn`：它慢或失败都不占宽限期。失败怎么记日志由调用方在 `abort`
///   future 里写（三个驱动的日志文本和字段不同，不能在这里统一）。
/// - 兜底任务睡 [`RETRY_STOP_GRACE`] 再收尾：对方没听中止、收尾通知丢了时，用户不会
///   永远卡在"正在重试"上。是不是同一个回合、要不要收尾，仍由调用方判断。
///
/// 必须在 tokio 运行时里调用（和原来三份实现的 `tokio::spawn` 约定一致）；没有运行时的
/// 场景（只会在单测里出现）由调用方在调用前就地收尾——那一步是同步的，没法用 future
/// 表达，留在各自的 `stop_after_retry_limit` 里。
pub(crate) fn stop_after_retry_limit<A, F>(abort: A, fallback: F)
where
    A: Future<Output = ()> + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(abort);
    tokio::spawn(async move {
        tokio::time::sleep(RETRY_STOP_GRACE).await;
        fallback.await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_hex_is_lowercase_hex_of_requested_length() {
        let value = random_hex(8);
        assert_eq!(value.len(), 16);
        assert!(value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_ne!(random_hex(16), random_hex(16));
    }
}
