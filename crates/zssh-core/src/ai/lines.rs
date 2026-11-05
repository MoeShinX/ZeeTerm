//! 按行读子进程输出：带单行上限，并且不把"解码失败"当成"进程退出"。
//!
//! 智能体（claude / codex / opencode）的 stdout 是外部程序的输出：可能出现一条
//! 没有换行的超长流（崩溃转储、被误用的 `printf`、恶意 shim），也可能混进非 UTF-8
//! 字节。`BufReader::lines()` 两种情况都会出错：前者把内存吃光，后者返回 `Err`
//! 让调用方的 `while let Ok(Some(..))` 直接跳出，表现成"进程已经退出"。
//!
//! 这里读到 `\n` 为止，最多攒 [`MAX_AGENT_LINE_BYTES`]：超出的部分继续从流里吞掉
//! （保证下一行从正确的位置开始），但不再累积；非 UTF-8 用 lossy 转换。
//!
//! 工具桥（`bridge`）另有一份更严格的实现：协议消息超长必须断开连接，不能被截断
//! 后继续解析，所以两边不合并。

use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncReadExt as _;

/// 单行最多攒这么多字节（1 MiB）。超出的部分丢弃，调用方会收到 `truncated`。
pub(crate) const MAX_AGENT_LINE_BYTES: usize = 1024 * 1024;

/// 读到的一行。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BoundedLine {
    /// 行内容（已去掉行尾的 `\r`）；超长时是前 [`MAX_AGENT_LINE_BYTES`] 字节。
    pub text: String,
    /// 这一行超过上限、后面的内容被丢掉了。
    pub truncated: bool,
}

/// 读一行；EOF 返回 `None`。
pub(crate) async fn read_bounded_line<R>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<BoundedLine>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    // 多读一个字节：正好 `max` 字节的内容加上换行符也要能完整读出来。
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut buf)
        .await?;
    if read == 0 {
        return Ok(None);
    }

    let mut truncated = false;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() as u64 >= limit {
        // 读满了上限还没看到换行：这一行超长。把剩下的字节读掉（不保存），
        // 否则下一次调用会从这一行的中间开始。
        truncated = true;
        loop {
            let mut tail = Vec::new();
            let n = (&mut *reader)
                .take(64 * 1024)
                .read_until(b'\n', &mut tail)
                .await?;
            if n == 0 || tail.last() == Some(&b'\n') {
                break;
            }
        }
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > max {
        buf.truncate(max);
        truncated = true;
    }

    Ok(Some(BoundedLine {
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_lines_and_strips_crlf() {
        let mut reader = tokio::io::BufReader::new(&b"a\r\nb\nc"[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 16).await.unwrap(),
            Some(BoundedLine {
                text: "a".to_owned(),
                truncated: false
            })
        );
        assert_eq!(
            read_bounded_line(&mut reader, 16).await.unwrap(),
            Some(BoundedLine {
                text: "b".to_owned(),
                truncated: false
            })
        );
        // 最后一行没有换行也要读到。
        assert_eq!(
            read_bounded_line(&mut reader, 16).await.unwrap(),
            Some(BoundedLine {
                text: "c".to_owned(),
                truncated: false
            })
        );
        assert_eq!(read_bounded_line(&mut reader, 16).await.unwrap(), None);
    }

    #[tokio::test]
    async fn line_of_exactly_the_limit_is_not_truncated() {
        let body = "x".repeat(16);
        let input = format!("{body}\n").into_bytes();
        let mut reader = tokio::io::BufReader::new(&input[..]);
        let line = read_bounded_line(&mut reader, 16).await.unwrap().unwrap();
        assert_eq!(line.text.len(), 16);
        assert!(!line.truncated);
    }

    #[tokio::test]
    async fn oversized_line_is_truncated_and_the_next_line_still_reads() {
        let body = "y".repeat(4096);
        let input = format!("{body}\nnext\n").into_bytes();
        let mut reader = tokio::io::BufReader::new(&input[..]);
        let first = read_bounded_line(&mut reader, 64).await.unwrap().unwrap();
        assert_eq!(first.text.len(), 64);
        assert!(first.truncated, "超长行要标记截断");
        // 关键：下一行从正确位置开始，而不是从超长行中间。
        let second = read_bounded_line(&mut reader, 64).await.unwrap().unwrap();
        assert_eq!(second.text, "next");
        assert!(!second.truncated);
    }

    #[tokio::test]
    async fn invalid_utf8_does_not_error() {
        let mut reader = tokio::io::BufReader::new(&b"ok\n\xff\xfe\n"[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 64).await.unwrap(),
            Some(BoundedLine {
                text: "ok".to_owned(),
                truncated: false
            })
        );
        // lossy 转换，不返回 Err（上层不会误判成"进程退出"）。
        let line = read_bounded_line(&mut reader, 64).await.unwrap().unwrap();
        assert!(line.text.contains('\u{fffd}'));
    }
}
