//! 远端压缩 / 解压的提示文字（0.2.14 `state/sftpArchive.ts`）：正在做什么、结束时的一句话、
//! 冲突对话框里“…等 N 个”那一行。

use super::props::size_long;
use crate::i18n::t;

/// 提示条里正在做什么。复制粘贴（G08b）以后也共用这一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BusyKind {
    Compress,
    Scan,
    Extract,
    /// 复制粘贴（文案另走 `clip::copying_text`，这里只占一个位置）。
    Paste,
}

/// 压缩 / 检查 / 解压的提示条文案。
pub(super) fn busy_text(kind: BusyKind, elapsed_seconds: u64, bytes: Option<u64>) -> String {
    let elapsed = format!(
        "{}{elapsed_seconds}{}",
        t(super::SftpMsg::ArchiveElapsedPrefix),
        t(super::SftpMsg::ArchiveElapsedSuffix)
    );
    match kind {
        BusyKind::Compress => match bytes {
            Some(bytes) => format!(
                "{}{}{}{}{}",
                t(super::SftpMsg::ArchiveCompressingWrittenPrefix),
                size_long(bytes),
                t(super::SftpMsg::ArchiveCompressingMiddle),
                elapsed,
                t(super::SftpMsg::ArchiveCompressingSuffix)
            ),
            None => format!(
                "{}{elapsed}{}",
                t(super::SftpMsg::ArchiveCompressingPrefix),
                t(super::SftpMsg::ArchiveCompressingSuffix)
            ),
        },
        BusyKind::Scan => format!(
            "{}{elapsed}{}",
            t(super::SftpMsg::ArchiveScanningPrefix),
            t(super::SftpMsg::ArchiveCompressingSuffix)
        ),
        BusyKind::Extract => format!(
            "{}{elapsed}{}",
            t(super::SftpMsg::ArchiveExtractingPrefix),
            t(super::SftpMsg::ArchiveCompressingSuffix)
        ),
        BusyKind::Paste => format!(
            "{}{elapsed}{}",
            t(super::SftpMsg::ArchivePastingPrefix),
            t(super::SftpMsg::ArchiveCompressingSuffix)
        ),
    }
}

/// 一句结束提示；`success` 决定是绿色的“成功”还是普通的“提示”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Notice {
    pub(super) success: bool,
    pub(super) text: String,
}

pub(super) fn compress_notice(cancelled: bool, name: &str, warning: Option<&str>) -> Notice {
    if cancelled {
        return Notice {
            success: false,
            text: t(super::SftpMsg::ArchiveCompressCancelled).to_owned(),
        };
    }
    let base = format!(
        "{}{name}{}",
        t(super::SftpMsg::ArchiveCompressedPrefix),
        t(super::SftpMsg::ArchiveCompressedSuffix)
    );
    Notice {
        success: true,
        text: match warning {
            Some(warning) => format!(
                "{base}{}{warning}{}",
                t(super::SftpMsg::ParenOpen),
                t(super::SftpMsg::ParenClose)
            ),
            None => base,
        },
    }
}

/// 解压结束的一句话：解出多少、跳过多少。
/// `in_place`：解到了已经存在的文件夹里——取消时已经解出的文件没法区分新旧，保留不动，要说清楚。
pub(super) fn extract_notice(
    cancelled: bool,
    extracted: u64,
    skipped: u64,
    warning: Option<&str>,
    dest_name: &str,
    in_place: bool,
) -> Notice {
    if cancelled {
        return Notice {
            success: false,
            text: if in_place {
                format!(
                    "{}{dest_name}{}",
                    t(super::SftpMsg::ArchiveExtractCancelledInPlacePrefix),
                    t(super::SftpMsg::ArchiveExtractCancelledInPlaceSuffix)
                )
            } else {
                t(super::SftpMsg::ArchiveExtractCancelled).to_owned()
            },
        };
    }
    let mut text = format!(
        "{}{dest_name}{}{extracted}{}",
        t(super::SftpMsg::ArchiveExtractedPrefix),
        t(super::SftpMsg::ArchiveExtractedMiddle),
        t(super::SftpMsg::ArchiveExtractedSuffix)
    );
    if skipped > 0 {
        text.push_str(&format!(
            "{}{skipped}{}",
            t(super::SftpMsg::ArchiveSkippedPrefix),
            t(super::SftpMsg::ArchiveSkippedSuffix)
        ));
    }
    Notice {
        success: true,
        text: match warning {
            Some(warning) => format!(
                "{text}{}{warning}{}",
                t(super::SftpMsg::ParenOpen),
                t(super::SftpMsg::ParenClose)
            ),
            None => text,
        },
    }
}

/// 冲突对话框里「…等 N 个」那一行；名字全列出来了就没有。
pub(super) fn conflict_more_text(conflict_count: u64, sample_len: usize) -> Option<String> {
    (conflict_count > sample_len as u64).then(|| {
        format!(
            "{}{conflict_count}{}",
            t(super::SftpMsg::ArchiveConflictMorePrefix),
            t(super::SftpMsg::ArchiveConflictMoreSuffix)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{BusyKind, busy_text, compress_notice, conflict_more_text, extract_notice};

    #[test]
    fn busy_text_says_what_is_going_on() {
        assert_eq!(
            busy_text(BusyKind::Scan, 3, None),
            "检查压缩包中…（已 3 秒）"
        );
        assert_eq!(busy_text(BusyKind::Extract, 0, None), "解压中…（已 0 秒）");
        assert_eq!(busy_text(BusyKind::Compress, 5, None), "压缩中…（已 5 秒）");
        assert_eq!(
            busy_text(BusyKind::Compress, 5, Some(1_572_864)),
            "压缩中…（已写入 1.5 MB · 已 5 秒）"
        );
    }

    #[test]
    fn compress_endings() {
        let done = compress_notice(false, "logs.tar.gz", None);
        assert!(done.success);
        assert_eq!(done.text, "已压缩为「logs.tar.gz」");
        let warned = compress_notice(false, "a.zip", Some("有文件在压缩时变了"));
        assert_eq!(warned.text, "已压缩为「a.zip」（有文件在压缩时变了）");
        let cancelled = compress_notice(true, "", None);
        assert!(!cancelled.success);
        assert!(cancelled.text.contains("没有留下临时文件"));
    }

    #[test]
    fn extract_endings_say_how_many_were_extracted_or_skipped() {
        let done = extract_notice(false, 12, 0, None, "site", false);
        assert_eq!(done.text, "已解压到「site」：解出 12 个文件");
        let skipped = extract_notice(false, 10, 2, Some("警告"), "site", true);
        assert_eq!(
            skipped.text,
            "已解压到「site」：解出 10 个文件，跳过 2 个已有文件（警告）"
        );
    }

    #[test]
    fn a_cancelled_extract_into_an_existing_folder_keeps_what_was_extracted() {
        let fresh = extract_notice(true, 0, 0, None, "site", false);
        assert_eq!(fresh.text, "已取消解压，没有留下半成品");
        let in_place = extract_notice(true, 0, 0, None, "site", true);
        assert!(in_place.text.contains("保留在「site」里"));
        assert!(!in_place.success);
    }

    #[test]
    fn the_more_line_only_shows_when_the_sample_is_short() {
        assert_eq!(conflict_more_text(7, 5).as_deref(), Some("等 7 个"));
        assert_eq!(conflict_more_text(5, 5), None);
        assert_eq!(conflict_more_text(0, 0), None);
    }
}
