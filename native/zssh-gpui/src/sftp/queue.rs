//! 传输队列的显示规则（0.2.14 `SftpTabView` 里的 `TransferQueue` / `TransferRow` 和
//! `queueSummary`）：摘要、速度、剩余时间、完成后的对账一句话。

use std::time::Duration;

use zssh_core::sftp::transfer::TransferState;

use super::props::size_long;

/// 一行传输的显示数据（从 `TransferRow` 取出来，方便单测）。
#[derive(Debug, Clone)]
pub(super) struct RowFacts<'a> {
    pub(super) state: &'a TransferState,
    pub(super) total_files: u64,
    pub(super) done_files: u64,
    pub(super) total_bytes: u64,
    pub(super) done_bytes: u64,
}

/// 队列摘要："2 个任务进行中 45%"；没有进行中的任务返回 `None`。
/// 返回 (进行中的个数, 百分比, 文字)。
pub(super) fn queue_summary(rows: &[RowFacts<'_>]) -> Option<(usize, u32, String)> {
    let active: Vec<&RowFacts<'_>> = rows.iter().filter(|r| !r.state.is_terminal()).collect();
    if active.is_empty() {
        return None;
    }
    let total: u64 = active.iter().map(|r| r.total_bytes).sum();
    let done: u64 = active.iter().map(|r| r.done_bytes).sum();
    let percent = if total > 0 {
        ((done as f64 / total as f64) * 100.0).floor().min(100.0) as u32
    } else {
        0
    };
    Some((
        active.len(),
        percent,
        format!("{} 个任务进行中 {percent}%", active.len()),
    ))
}

/// 这一行的进度百分比（0~100）。
pub(super) fn row_percent(state: &TransferState, done: u64, total: u64) -> f32 {
    if total > 0 {
        ((done as f64 / total as f64) * 100.0).min(100.0) as f32
    } else if matches!(state, TransferState::Done) {
        100.0
    } else {
        0.0
    }
}

pub(super) fn speed_text(bytes_per_second: u64) -> String {
    format!("{}/s", size_long(bytes_per_second))
}

/// 剩余时间：不到 1 分钟写秒，之后分秒，再之后时分。
pub(super) fn eta_text(seconds: Option<u64>) -> String {
    match seconds {
        None => "—".to_owned(),
        Some(s) if s < 60 => format!("{} 秒", s.max(1)),
        Some(s) if s < 3600 => format!("{} 分 {} 秒", s / 60, s % 60),
        Some(s) => format!("{} 时 {} 分", s / 3600, s % 3600 / 60),
    }
}

/// 传完之后的对账一句话：几个文件、多少字节、和源是否一致、用时和平均速度。
/// 只在“已完成”的行上给；字节数对不上（引擎说完成了但数目不同）要明说。
pub(super) fn reconcile_summary(row: &RowFacts<'_>, elapsed: Option<Duration>) -> Option<String> {
    if !matches!(row.state, TransferState::Done) {
        return None;
    }
    let mut parts = Vec::new();
    if row.total_files > 1 {
        parts.push(if row.done_files == row.total_files {
            format!("{} 个文件", row.total_files)
        } else {
            format!(
                "文件数对不上：完成 {} / 共 {}",
                row.done_files, row.total_files
            )
        });
    }
    let bytes_ok = row.total_bytes == 0 || row.done_bytes == row.total_bytes;
    parts.push(if bytes_ok {
        format!(
            "{}，字节数与源一致",
            size_long(row.done_bytes.max(row.total_bytes))
        )
    } else {
        format!(
            "字节数对不上：已传 {} / 源 {}",
            size_long(row.done_bytes),
            size_long(row.total_bytes)
        )
    });
    if let Some(elapsed) = elapsed {
        let seconds = elapsed.as_secs_f64();
        if seconds < 1.0 {
            parts.push("用时 不到 1 秒".to_owned());
        } else if row.done_bytes > 0 {
            parts.push(format!(
                "用时 {} 秒，平均 {}",
                seconds.round() as u64,
                speed_text((row.done_bytes as f64 / seconds) as u64)
            ));
        } else {
            parts.push(format!("用时 {} 秒", seconds.round() as u64));
        }
    }
    Some(format!("已核对：{}", parts.join("；")))
}

/// 对账有没有出问题（界面用警示色画）。
pub(super) fn reconcile_is_mismatch(row: &RowFacts<'_>) -> bool {
    matches!(row.state, TransferState::Done)
        && ((row.total_files > 1 && row.done_files != row.total_files)
            || (row.total_bytes > 0 && row.done_bytes != row.total_bytes))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zssh_core::sftp::transfer::TransferState;

    use super::{
        RowFacts, eta_text, queue_summary, reconcile_is_mismatch, reconcile_summary, row_percent,
        speed_text,
    };

    fn facts(state: &TransferState, files: (u64, u64), bytes: (u64, u64)) -> RowFacts<'_> {
        RowFacts {
            state,
            done_files: files.0,
            total_files: files.1,
            done_bytes: bytes.0,
            total_bytes: bytes.1,
        }
    }

    #[test]
    fn the_summary_covers_only_unfinished_tasks() {
        let run = TransferState::Running;
        let queued = TransferState::Queued;
        let done = TransferState::Done;
        let rows = [
            facts(&run, (0, 1), (50, 100)),
            facts(&queued, (0, 1), (0, 100)),
            facts(&done, (1, 1), (500, 500)),
        ];
        let (active, percent, text) = queue_summary(&rows).expect("有进行中的");
        assert_eq!((active, percent), (2, 25));
        assert_eq!(text, "2 个任务进行中 25%");
        assert!(queue_summary(&[facts(&done, (1, 1), (5, 5))]).is_none());
        assert!(queue_summary(&[]).is_none());
        // 总量还不知道：0%，不除零。
        assert_eq!(
            queue_summary(&[facts(&run, (0, 0), (0, 0))]).map(|s| s.1),
            Some(0)
        );
    }

    #[test]
    fn percent_handles_unknown_totals() {
        assert_eq!(row_percent(&TransferState::Running, 50, 200), 25.0);
        assert_eq!(row_percent(&TransferState::Running, 0, 0), 0.0);
        assert_eq!(row_percent(&TransferState::Done, 0, 0), 100.0);
        assert_eq!(row_percent(&TransferState::Running, 300, 200), 100.0);
    }

    #[test]
    fn speed_and_eta_read_naturally() {
        assert_eq!(speed_text(2048), "2.0 KB/s");
        assert_eq!(eta_text(None), "—");
        assert_eq!(eta_text(Some(0)), "1 秒");
        assert_eq!(eta_text(Some(45)), "45 秒");
        assert_eq!(eta_text(Some(125)), "2 分 5 秒");
        assert_eq!(eta_text(Some(7300)), "2 时 1 分");
    }

    #[test]
    fn a_finished_big_transfer_reports_the_reconciliation() {
        let done = TransferState::Done;
        let row = facts(&done, (3, 3), (64 * 1024 * 1024, 64 * 1024 * 1024));
        let text = reconcile_summary(&row, Some(Duration::from_secs(8))).expect("完成才有");
        assert!(
            text.starts_with("已核对：3 个文件；64.0 MB，字节数与源一致"),
            "{text}"
        );
        assert!(text.contains("用时 8 秒，平均 8.0 MB/s"), "{text}");
        assert!(!reconcile_is_mismatch(&row));
    }

    #[test]
    fn a_count_mismatch_is_called_out() {
        let done = TransferState::Done;
        let row = facts(&done, (2, 3), (90, 100));
        let text = reconcile_summary(&row, None).expect("完成才有");
        assert!(text.contains("文件数对不上：完成 2 / 共 3"));
        assert!(text.contains("字节数对不上：已传 90 B / 源 100 B"));
        assert!(reconcile_is_mismatch(&row));
    }

    #[test]
    fn unfinished_or_failed_rows_have_no_reconciliation() {
        let failed = TransferState::Failed("x".into());
        assert!(reconcile_summary(&facts(&failed, (0, 1), (0, 1)), None).is_none());
        let run = TransferState::Running;
        assert!(reconcile_summary(&facts(&run, (0, 1), (0, 1)), None).is_none());
        assert!(!reconcile_is_mismatch(&facts(&run, (0, 1), (0, 1))));
    }

    #[test]
    fn very_fast_transfers_do_not_invent_a_speed() {
        let done = TransferState::Done;
        let row = facts(&done, (1, 1), (10, 10));
        let text = reconcile_summary(&row, Some(Duration::from_millis(200))).expect("完成");
        assert!(text.ends_with("用时 不到 1 秒"));
    }
}
