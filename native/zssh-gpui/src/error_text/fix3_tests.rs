//! V2-I18N-FIX3 的英文整句测试：错误都从真实触发路径构造，断言英文界面下整句是英文、
//! 参数（路径、文件名、服务器原文）原样在，中文界面逐字等于核心的 `Display`。

use zssh_core::sftp::archive::{ArchiveError, CompressFormat, classify_compress};
use zssh_core::sftp::error::{SftpError, timeout_error};
use zssh_core::sftp::local::{check_child_name, ensure_inside};
use zssh_core::sftp::server_copy::{CpFailure, classify_cp_result};
use zssh_core::sftp::transfer::plan::plan_upload;

use super::{error_text_in, reason_text_in};
use crate::i18n::Lang;

fn has_han(text: &str) -> bool {
    text.chars()
        .any(|ch| (0x4E00..=0x9FFF).contains(&(ch as u32)))
}

/// 英文整句里不能有汉字（参数是数据，这里的参数都是 ASCII）。
fn assert_english(text: &str) {
    assert!(!has_han(text), "英文界面出现了中文：{text}");
}

#[test]
fn sftp_invalid_input_names_are_english_with_parameters() {
    // 保留名：核心给的是 `NameFix` 代码，英文界面重新生成说明。
    let err = check_child_name("CON").expect_err("保留名不能直接用");
    let en = error_text_in(Lang::En, &err);
    assert_english(&en);
    assert!(en.contains("\"CON\" cannot be used as a file name"), "{en}");
    assert!(en.contains("reserved Windows name"), "{en}");
    // 中文逐字等于核心的 Display。
    assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());

    let empty = check_child_name("  ").expect_err("空名字");
    assert_eq!(
        error_text_in(Lang::En, &empty),
        "Invalid input: The name cannot be empty"
    );

    let dots = check_child_name("..").expect_err("点点");
    assert_eq!(
        error_text_in(Lang::En, &dots),
        "Invalid input: The name cannot be . or .."
    );
}

#[test]
fn sftp_target_outside_keeps_both_paths() {
    let err = ensure_inside(
        std::path::Path::new("C:/zssh-v2-data/dl"),
        std::path::Path::new("C:/zssh-v2-data/other/a.txt"),
    )
    .expect_err("目标在所选目录外");
    let en = error_text_in(Lang::En, &err);
    assert_english(&en);
    assert!(en.starts_with("Invalid input: Target path "), "{en}");
    assert!(en.contains("other"), "{en}");
    assert!(en.contains("dl"), "{en}");
    assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());
}

#[test]
fn sftp_remote_timeout_is_a_full_english_sentence() {
    let err = timeout_error(30);
    assert_eq!(
        error_text_in(Lang::En, &err),
        "Remote error: The server did not respond for 30 seconds (SFTP request timed out)"
    );
    assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());
}

#[test]
fn archive_failures_are_english_and_nest_the_cause() {
    let missing = classify_compress(CompressFormat::Zip, Some(127), "").expect_err("没有 zip");
    let err = ArchiveError::Failed(missing);
    let en = error_text_in(Lang::En, &err);
    assert_eq!(en, "The zip command was not found on the server");
    assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());

    // 退出码和服务器 stderr 原文作为参数保留。
    let failed = classify_compress(CompressFormat::TarGz, Some(2), "tar: boom: No space left")
        .expect_err("tar 失败");
    let en = error_text_in(Lang::En, &ArchiveError::Failed(failed));
    assert_eq!(
        en,
        "Compression failed (exit code 2): tar: boom: No space left"
    );

    // 内层的 SFTP 错误也按当前语言生成。
    let wrapped = ArchiveError::from(SftpError::PermissionDenied {
        path: "/srv/a".into(),
    });
    let en = error_text_in(Lang::En, &wrapped);
    assert_eq!(en, "Permission denied: /srv/a");
    assert_eq!(error_text_in(Lang::Zh, &wrapped), wrapped.to_string());
    assert_eq!(
        error_text_in(Lang::En, &ArchiveError::UnsafeEntries),
        "The archive contains paths that point outside the target directory; extraction was refused"
    );
}

#[test]
fn server_side_copy_failures_are_english() {
    let failure =
        classify_cp_result(Some(1), " cp: cannot stat 'x' ", false).expect_err("cp 退出码 1");
    assert!(matches!(failure, CpFailure::Failed(_)));
    let en = error_text_in(Lang::En, &failure);
    assert_eq!(
        en,
        "cp on the server failed (exit code 1): cp: cannot stat 'x'"
    );
    assert_eq!(error_text_in(Lang::Zh, &failure), failure.to_string());

    let not_found = classify_cp_result(Some(127), "", false).expect_err("没有 cp");
    let CpFailure::NotAvailable(reason) = &not_found else {
        panic!("127 应该是用不了");
    };
    assert_eq!(
        reason_text_in(Lang::En, reason),
        "The cp command was not found on the server"
    );
}

#[test]
fn transfer_plan_notes_are_english_sentences() {
    // 一个带空子目录的本地目录：上传计划里有「空目录」和「共 N 个文件」两条说明。
    let root = std::env::temp_dir().join(format!("zssh-fix3-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("empty-dir")).expect("建临时目录");
    std::fs::write(root.join("a.txt"), b"x").expect("写临时文件");
    let plan = plan_upload(&root, "/srv/up", true).expect("展开上传计划");
    let _ = std::fs::remove_dir_all(&root);

    let notes: Vec<String> = plan
        .notes
        .iter()
        .map(|note| reason_text_in(Lang::En, note))
        .collect();
    assert!(
        notes.contains(&"Empty directory empty-dir will be created too".to_owned()),
        "{notes:?}"
    );
    assert!(notes.contains(&"1 files in total".to_owned()), "{notes:?}");
    for note in &notes {
        assert_english(note);
    }
    // 中文逐字等于核心的原文。
    for note in &plan.notes {
        assert_eq!(reason_text_in(Lang::Zh, note), note.zh());
    }
}
