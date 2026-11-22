//! 显示输出的投递顺序：先确认被过滤的字节，再交给入口显示。
//!
//! 这段策略原来在 `src-tauri` 的 `emit_display` 里，和 Tauri 无关，
//! 抽出来让两个入口共用同一套 ACK 顺序。

pub struct DisplayBatch {
    /// 要显示给用户的字节。
    pub data: Vec<u8>,
    /// 被显示过滤吞掉、需要由过滤侧确认的字节数。
    pub suppressed: usize,
}

pub trait AckSink {
    fn ack(&mut self, bytes: usize);
}

/// 先 ack 被过滤的字节，再把显示数据交给 `emit`。
///
/// - `emit` 失败时返回错误；被过滤的字节已经确认，显示数据由调用方决定是否确认。
/// - 空数据不调用 `emit`，但仍然确认 `suppressed`。
pub fn deliver<A: AckSink>(
    batch: DisplayBatch,
    ack: &mut A,
    emit: impl FnOnce(Vec<u8>) -> Result<(), String>,
) -> Result<(), String> {
    if batch.suppressed > 0 {
        ack.ack(batch.suppressed);
    }
    if !batch.data.is_empty() {
        emit(batch.data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Recorder<'a>(&'a RefCell<Vec<String>>);

    impl AckSink for Recorder<'_> {
        fn ack(&mut self, bytes: usize) {
            self.0.borrow_mut().push(format!("ack:{bytes}"));
        }
    }

    #[test]
    fn filtered_bytes_are_acked_before_display() {
        let log = RefCell::new(Vec::new());
        deliver(
            DisplayBatch {
                data: b"visible".to_vec(),
                suppressed: 7,
            },
            &mut Recorder(&log),
            |data| {
                log.borrow_mut()
                    .push(format!("emit:{}", String::from_utf8_lossy(&data)));
                Ok(())
            },
        )
        .expect("投递应成功");
        assert_eq!(*log.borrow(), ["ack:7", "emit:visible"]);
    }

    #[test]
    fn empty_display_still_acks_filtered_bytes() {
        let log = RefCell::new(Vec::new());
        deliver(
            DisplayBatch {
                data: Vec::new(),
                suppressed: 3,
            },
            &mut Recorder(&log),
            |_| panic!("空数据不应触发 emit"),
        )
        .unwrap();
        assert_eq!(*log.borrow(), ["ack:3"]);
    }

    #[test]
    fn emit_failure_is_returned_after_ack() {
        let log = RefCell::new(Vec::new());
        let err = deliver(
            DisplayBatch {
                data: b"x".to_vec(),
                suppressed: 1,
            },
            &mut Recorder(&log),
            |_| Err("channel closed".to_owned()),
        )
        .unwrap_err();
        assert_eq!(err, "channel closed");
        assert_eq!(*log.borrow(), ["ack:1"]);
    }
}
