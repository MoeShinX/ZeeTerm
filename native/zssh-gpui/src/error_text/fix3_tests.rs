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

#[test]
fn sync_package_errors_are_english_with_parameters() {
    use zssh_core::sync::crypto::open;
    use zssh_core::sync::error::SyncError;

    // 不是加密包：serde 的原文作为参数保留，前缀按类型出词。
    let err = open(b"{}", "pw").expect_err("空对象不是加密包");
    let en = error_text_in(Lang::En, &err);
    assert!(
        en.starts_with("Invalid sync package: Not a valid encrypted package: "),
        "{en}"
    );
    assert_english(&en);
    assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());

    // 限流：等待时间按语言进位出词，中文逐字等于核心的 Display。
    let limited = SyncError::RateLimited {
        service: "GitHub".into(),
        retry_after_secs: 90,
    };
    assert_eq!(
        error_text_in(Lang::En, &limited),
        "GitHub is rate limiting requests; retry in about 2 minutes"
    );
    assert_eq!(error_text_in(Lang::Zh, &limited), limited.to_string());

    assert_eq!(
        error_text_in(Lang::En, &SyncError::WrongPassword),
        "Wrong master password (or the file header was modified)"
    );
}

#[test]
fn storage_backend_errors_are_english_and_nested() {
    use zssh_core::sync::error::SyncError;
    use zssh_core::sync::github::{GithubApi, GithubConfig};
    use zssh_core::sync::webdav::{WebDavConfig, WebDavRaw};

    // GitHub：没填令牌（表单校验），经 SyncError 包一层之后仍是整句英文。
    let storage = GithubApi::new(
        &GithubConfig {
            api_base: String::new(),
            token: String::new(),
            gist_id: String::new(),
            allow_insecure_http: false,
        },
        None,
        false,
    )
    .err()
    .expect("没有令牌不能创建");
    let en = error_text_in(Lang::En, &storage);
    assert_eq!(en, "Storage read/write failed: Enter a GitHub token");
    assert_eq!(error_text_in(Lang::Zh, &storage), storage.to_string());
    let wrapped = SyncError::from(storage);
    assert_eq!(
        error_text_in(Lang::En, &wrapped),
        "Storage error: Storage read/write failed: Enter a GitHub token"
    );
    assert_eq!(error_text_in(Lang::Zh, &wrapped), wrapped.to_string());

    // WebDAV：地址协议不对，协议名作为参数保留。
    let config = WebDavConfig {
        url: "ftp://dav.example.com/".into(),
        directory: String::new(),
        username: "u".into(),
        password: "p".into(),
        allow_insecure_http: false,
    };
    let err = WebDavRaw::new(&config, None).err().expect("ftp 不支持");
    let en = error_text_in(Lang::En, &err);
    assert_eq!(
        en,
        "Storage read/write failed: The WebDAV address only supports http(s), got ftp://"
    );
    assert_english(&en);
}

#[test]
fn host_key_change_names_the_record_and_the_advice_in_english() {
    use std::path::PathBuf;
    use zssh_core::error::{HostKeyMismatch, ZsshError};
    use zssh_core::ssh::known_hosts::StoredIn;

    let build = |stored_in: StoredIn| {
        ZsshError::HostKeyChanged(Box::new(HostKeyMismatch {
            host: "web-1".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: stored_in.describe(),
            advice: stored_in.advice("web-1", 22),
            source: stored_in.source("web-1", 22),
        }))
    };

    // 旧记录在只读的系统文件里：英文给出文件路径和用户自己执行的命令。
    let system = build(StoredIn::System(PathBuf::from("/etc/ssh/ssh_known_hosts")));
    let en = error_text_in(Lang::En, &system);
    assert_english(&en);
    assert!(
        en.contains("(recorded in the system file /etc/ssh/ssh_known_hosts (read-only for us))"),
        "{en}"
    );
    assert!(en.contains("ssh-keygen -R \"web-1\""), "{en}");
    assert!(en.contains("Saved: SHA256:old"), "{en}");
    assert_eq!(error_text_in(Lang::Zh, &system), system.to_string());

    // 旧记录在自己的 known_hosts 里：英文指向主机的「Advanced」设置。
    let own = build(StoredIn::Our);
    let en = error_text_in(Lang::En, &own);
    assert_english(&en);
    assert!(en.contains("recorded in ZeeTerm's own known_hosts"), "{en}");
    assert!(en.contains("Clear Saved Fingerprint"), "{en}");
    assert_eq!(error_text_in(Lang::Zh, &own), own.to_string());
}

#[test]
fn ssh_agent_failures_carry_the_concrete_reason_in_english() {
    use zssh_core::error::{Reason, ZsshError, reason_keys};

    let err = ZsshError::AllAuthFailed {
        tried: "ssh-agent（x）、密码".into(),
        remaining: "publickey".into(),
        tried_codes: "agent_error\u{1f}password".into(),
        remaining_codes: "publickey".into(),
        agent_error: Some(Box::new(ZsshError::AgentUnavailable(Reason::new(
            reason_keys::AGENT_PIPE_UNREACHABLE,
            "没连上 OpenSSH agent 的命名管道，也没连上 Pageant",
        )))),
    };
    let en = error_text_in(Lang::En, &err);
    assert_english(&en);
    assert!(
        en.contains("tried these methods: ssh-agent (ssh-agent is unavailable: "),
        "{en}"
    );
    assert!(en.contains("OpenSSH agent"), "{en}");
    assert!(en.contains("password"), "{en}");
}

#[test]
fn x11_failures_are_english_in_errors_and_in_the_terminal_notice() {
    use zssh_core::error::{Reason, ZsshError, reason_keys};
    use zssh_core::x11::X11Failure;

    // ServerNoReply 的 detail 是带 key 的原因（发送请求失败），整句英文，系统错误原文照留。
    let failure = X11Failure::ServerNoReply {
        detail: Reason::new(reason_keys::X11_SEND_FAILED, "发送请求失败：channel closed")
            .with("cause", "channel closed"),
    };
    let err = ZsshError::X11(failure.clone());
    let en = error_text_in(Lang::En, &err);
    assert_eq!(
        en,
        "Could not start X11 forwarding: The server did not reply to the X11 forwarding request: Failed to send the request: channel closed"
    );
    assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());

    let line = super::x11_advice_en(&X11Failure::NoLocalXServer {
        display: "127.0.0.1:6020".into(),
        installable: true,
    });
    assert_english(&line);
    assert!(
        line.contains("connecting to 127.0.0.1:6020 failed"),
        "{line}"
    );
    assert!(line.contains("install VcXsrv in one click"), "{line}");
    let line = super::x11_advice_en(&failure);
    assert!(
        line.contains("Failed to send the request: channel closed"),
        "{line}"
    );
    assert_english(&line);
}

#[test]
fn connection_test_results_are_english_sentences() {
    use zssh_core::error::ZsshError;
    use zssh_core::ssh::connection_test::{
        FailurePhase, HopRole, Interaction, TestStage, classify_failure, ssh_success_headline,
        untrusted_stop_headline,
    };

    let refused = ZsshError::Connect {
        host: "web-1".into(),
        port: 22,
        tried: "web-1:22".into(),
        source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
    };
    let failure = classify_failure(
        &refused,
        FailurePhase::Connect { via_proxy: false },
        HopRole::Target,
        "",
        Interaction::default(),
    );
    assert_eq!(failure.stage, TestStage::Connect);
    assert_eq!(
        reason_text_in(Lang::En, &failure.message),
        "Connection refused: nothing is listening on web-1:22, or a firewall rejected it"
    );
    assert_eq!(
        reason_text_in(Lang::Zh, &failure.message),
        failure.message.zh()
    );

    // 跳板机那一跳：标签 + 内层原因都按语言出。
    let jump = classify_failure(
        &ZsshError::AuthFailed,
        FailurePhase::Handshake,
        HopRole::Jump,
        "Jump host edge-1",
        Interaction::default(),
    );
    assert_eq!(jump.stage, TestStage::Jump);
    assert_eq!(
        reason_text_in(Lang::En, &jump.message),
        "Jump host edge-1: Authentication failed: wrong user name or password"
    );

    assert_eq!(
        reason_text_in(
            Lang::En,
            &ssh_success_headline(86, Some("SSH-2.0-OpenSSH_9.6"))
        ),
        "Connected in 86 ms, server SSH-2.0-OpenSSH_9.6"
    );
    let untrusted = untrusted_stop_headline("SHA256:abc", true, None);
    let en = reason_text_in(Lang::En, &untrusted);
    assert_english(&en);
    assert!(
        en.contains("SHA256:abc") && en.contains("other types"),
        "{en}"
    );
}
