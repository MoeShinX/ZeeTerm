//! W06：目标原本不存在时，提交前不能被别人同时新建的同名文件覆盖。
//! 独占创建（占位）把"检查不存在"和"提交"之间的缝合上；占位失败 / 服务器不认独占标志都要明确失败。
//! 走 AI 的 `write_file`（`execute_write` → `replace_via_temp_checked`）；编辑器另存 / 冲突另存 /
//! 覆盖已删除的远端走同一个函数（见 `sftp/edit/upload_tests.rs`）。

use super::testfs::MemFs;
use super::*;

const FRESH: &str = "/home/u/fresh.txt";
const THEIRS: &[u8] = b"created by another client\n";
const NEW: &str = "new content\n";

fn leftovers(fs: &MemFs) -> Vec<String> {
    fs.names_in("/home/u")
        .into_iter()
        .filter(|name| name.contains("zssh-tmp") || name.contains("zssh-old"))
        .collect()
}

#[tokio::test]
async fn a_new_file_is_claimed_before_anything_is_written_and_renamed_over_the_claim() {
    let fs = MemFs::new();
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    let result = execute_write(&fs, &plan, true, 5, "r", 5).await.unwrap();
    assert!(result.created);
    assert_eq!(fs.get(FRESH).unwrap(), NEW.as_bytes());
    assert!(leftovers(&fs).is_empty(), "{:?}", leftovers(&fs));
    let log = fs.log();
    let claim = log
        .iter()
        .position(|l| l == &format!("create-excl {FRESH}"));
    let temp_write = log
        .iter()
        .position(|l| l.starts_with("write ") && l.contains("zssh-tmp"));
    let rename = log.iter().position(|l| l.starts_with("rename "));
    assert!(
        claim.is_some() && claim < temp_write && temp_write < rename,
        "{log:?}"
    );
}

/// 复现 W06：最后一次检查之后、提交之前，别的客户端新建了同名文件。
/// 以前改名是覆盖语义，对方的内容被静默覆盖；现在占位挡在前面，对方的内容保留、我方报错。
#[tokio::test]
async fn a_file_created_by_someone_else_at_the_last_moment_is_never_overwritten() {
    let fs = MemFs::new();
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    *fs.external_create_before_claim.lock().unwrap() = Some((FRESH.to_owned(), THEIRS.to_vec()));
    let err = execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert!(err.contains("刚刚被创建了"), "{err}");
    assert_eq!(fs.get(FRESH).unwrap(), THEIRS, "对方的内容必须保留");
    assert!(leftovers(&fs).is_empty());
    assert!(
        fs.log().iter().all(|l| !l.starts_with("write ")),
        "占位失败就不该写任何东西：{:?}",
        fs.log()
    );
}

/// 占位成功之后，别的客户端往这个文件里写了内容（独占创建挡不住对已存在文件的写入）：
/// 提交前的复查发现占位不再是空的，放弃，不覆盖对方，也不删对方的文件。
#[tokio::test]
async fn content_written_into_the_claim_by_someone_else_is_not_overwritten_or_deleted() {
    let fs = MemFs::new();
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    *fs.external_write_during_write.lock().unwrap() =
        Some(("zssh-tmp".to_owned(), FRESH.to_owned(), THEIRS.to_vec()));
    let err = execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert!(err.contains("别人改动"), "{err}");
    assert_eq!(fs.get(FRESH).unwrap(), THEIRS);
    assert!(leftovers(&fs).is_empty(), "临时文件要清掉");
}

#[tokio::test]
async fn a_server_that_ignores_the_exclusive_flag_fails_safe_without_writing() {
    let fs = MemFs::new();
    *fs.exclusive_unsupported.lock().unwrap() = true;
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    let err = execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert!(err.contains("不支持原子地新建"), "{err}");
    assert_eq!(fs.get(FRESH), None, "不能退回非原子写法");
    assert!(leftovers(&fs).is_empty());
    assert!(fs.log().iter().all(|l| !l.starts_with("write ")));
}

/// 占位之后任何一步失败：临时文件和自己建的空占位都要清掉，目标回到"不存在"。
#[tokio::test]
async fn failures_after_the_claim_remove_the_temp_file_and_the_empty_claim() {
    // 写临时文件失败
    let fs = MemFs::new();
    *fs.fail_write_containing.lock().unwrap() = Some("zssh-tmp".to_owned());
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    let err = execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert!(err.contains("写临时文件失败"), "{err}");
    assert_eq!(fs.get(FRESH), None);
    assert!(leftovers(&fs).is_empty());

    // 临时文件写了一半（大小对不上）
    let fs = MemFs::new();
    *fs.half_write_containing.lock().unwrap() = Some("zssh-tmp".to_owned());
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert_eq!(fs.get(FRESH), None);
    assert!(leftovers(&fs).is_empty());

    // 改名被服务器拒绝（原文件 = 占位没动）
    let fs = MemFs::new();
    *fs.fail_rename.lock().unwrap() = true;
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    let err = execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert!(err.contains("提交前失败"), "{err}");
    assert_eq!(fs.get(FRESH), None);
    assert!(leftovers(&fs).is_empty());

    // 检查点（停止按钮）在提交前拦下
    let fs = MemFs::new();
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    let err = execute_write_checked(&fs, &plan, true, 5, "r", 5, &|| Err("已停止".to_owned()))
        .await
        .unwrap_err();
    assert!(err.contains("已停止"), "{err}");
    assert_eq!(fs.get(FRESH), None);
    assert!(leftovers(&fs).is_empty());
}

/// 改名结果不确定（连接断了）时，占位和临时文件都不能删：改名可能已经发生。
#[tokio::test]
async fn an_uncertain_rename_keeps_everything_for_the_user_to_inspect() {
    let fs = MemFs::new();
    *fs.no_posix_rename.lock().unwrap() = true;
    *fs.disconnect_rename_call.lock().unwrap() = Some((2, true));
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    let err = execute_write(&fs, &plan, true, 5, "r", 5)
        .await
        .unwrap_err();
    assert!(err.contains("不确定"), "{err}");
    assert!(
        fs.get(FRESH).is_some(),
        "目标可能已经是新内容，不能被清理掉"
    );
}

/// 直接用核心函数（编辑器保存 / 另存都走这里）：`existed = false` 同样先占位。
#[tokio::test]
async fn the_shared_replace_function_claims_a_new_target_too() {
    let fs = MemFs::new();
    *fs.external_create_before_claim.lock().unwrap() = Some((FRESH.to_owned(), THEIRS.to_vec()));
    let err = replace_via_temp(&fs, FRESH, b"mine", &ReplaceBase::default(), "x")
        .await
        .unwrap_err();
    assert!(err.contains("刚刚被创建了"), "{err}");
    assert_eq!(fs.get(FRESH).unwrap(), THEIRS);

    let fs = MemFs::new();
    *fs.exclusive_unsupported.lock().unwrap() = true;
    let err = replace_via_temp(&fs, FRESH, b"mine", &ReplaceBase::default(), "x")
        .await
        .unwrap_err();
    assert!(err.contains("不支持原子地新建"), "{err}");
    assert_eq!(fs.get(FRESH), None);
}

/// 已存在文件的授权覆盖路径不受影响：不占位、照旧备份并改名覆盖。
#[tokio::test]
async fn overwriting_an_existing_file_does_not_claim_anything() {
    let fs = MemFs::new();
    *fs.exclusive_unsupported.lock().unwrap() = true;
    fs.put(FRESH, b"old\n", 0o644);
    let plan = prepare_write(&fs, "fresh.txt", NEW, false).await.unwrap();
    execute_write(&fs, &plan, false, 5, "r", 5).await.unwrap();
    assert_eq!(fs.get(FRESH).unwrap(), NEW.as_bytes());
    assert!(fs.log().iter().all(|l| !l.starts_with("create-excl")));
}

// ---------- Z-A2：真实路径解析失败 ----------

#[tokio::test]
async fn an_existing_file_whose_real_path_cannot_be_resolved_is_refused() {
    let fs = MemFs::new();
    fs.put(FRESH, b"old\n", 0o644);
    *fs.fail_realpath_for.lock().unwrap() = Some(FRESH.to_owned());
    let err = prepare_write(&fs, "fresh.txt", NEW, false)
        .await
        .unwrap_err();
    assert!(err.contains("解析不出"), "{err}");
    assert_eq!(fs.get(FRESH).unwrap(), b"old\n");
}

/// 目标本来不存在时 realpath 失败是正常的（路径必须存在才能解析），照常计划新建。
#[tokio::test]
async fn a_missing_target_is_planned_normally() {
    let fs = MemFs::new();
    *fs.fail_realpath_for.lock().unwrap() = Some(FRESH.to_owned());
    let plan = prepare_write(&fs, "fresh.txt", NEW, true).await.unwrap();
    assert!(plan.is_new());
    assert_eq!(plan.target, FRESH);
}
