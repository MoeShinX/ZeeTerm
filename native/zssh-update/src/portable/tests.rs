use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use super::*;

/// 每个用例一个全新的临时目录（进程号 + 名字 + 纳秒），用完不删也不影响别的用例。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zeeterm-portable-{}-{name}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 假的 Windows 程序：`MZ` 开头，后面是区分版本的内容。
fn fake_exe(tag: &str) -> Vec<u8> {
    let mut bytes = b"MZ".to_vec();
    bytes.extend_from_slice(tag.as_bytes());
    bytes
}

/// 一份"免安装版"目录：旧版本的主程序、辅助程序、许可文件、标记和数据目录。
fn install_old(dir: &Path) {
    fs::write(dir.join(MAIN_EXE), fake_exe("old-main")).expect("写主程序");
    fs::write(dir.join(HELPER_EXE), fake_exe("old-helper")).expect("写辅助程序");
    fs::write(dir.join("LICENSE"), "old license").expect("写 LICENSE");
    fs::write(dir.join("NOTICE"), "old notice").expect("写 NOTICE");
    fs::write(dir.join(MARKER_FILE), "").expect("写标记");
    fs::create_dir_all(dir.join(DATA_SUBDIR)).expect("建数据目录");
    fs::write(dir.join(DATA_SUBDIR).join("hosts.db"), "user data").expect("写数据");
}

/// 条目：(名字, 内容)。
fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = fs::File::create(path).expect("建 zip");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        writer.start_file(*name, options).expect("加条目");
        writer.write_all(bytes).expect("写条目");
    }
    writer.finish().expect("收尾");
}

/// 一份正常的新版本 zip（和 build.ps1 打出来的布局一致：平铺，带一个空的 portable 标记）。
fn good_zip(path: &Path) {
    build_zip(
        path,
        &[
            (MAIN_EXE, &fake_exe("new-main")),
            (HELPER_EXE, &fake_exe("new-helper")),
            ("LICENSE", b"new license"),
            ("NOTICE", b"new notice"),
            (MARKER_FILE, b""),
        ],
    );
}

fn layout_in(dir: &Path) -> PortableLayout {
    PortableLayout::new(&dir.join(MAIN_EXE))
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|err| panic!("读 {}：{err}", path.display()))
}

/// 目录里一个都不能多：没有 `*.old`、没有临时目录。
fn assert_no_leftovers(dir: &Path) {
    let names: Vec<String> = fs::read_dir(dir)
        .expect("读目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    for name in &names {
        assert!(!name.ends_with(OLD_SUFFIX), "残留 {name}：{names:?}");
        assert!(!name.starts_with(STAGE_PREFIX), "残留 {name}：{names:?}");
    }
}

fn assert_old_version_intact(dir: &Path) {
    assert_eq!(read(&dir.join(MAIN_EXE)), fake_exe("old-main"));
    assert_eq!(read(&dir.join(HELPER_EXE)), fake_exe("old-helper"));
    assert_eq!(read(&dir.join("LICENSE")), b"old license");
    assert_eq!(read(&dir.join("NOTICE")), b"old notice");
    assert_eq!(read(&dir.join(DATA_SUBDIR).join("hosts.db")), b"user data");
    assert!(dir.join(MARKER_FILE).is_file());
}

#[test]
fn the_marker_decides_portable_mode_and_the_data_dir_sits_beside_the_program() {
    let dir = temp_dir("marker");
    assert!(!has_marker(&dir), "没有标记文件就是安装版");
    assert_eq!(data_dir_for(&dir, false), None);

    fs::write(dir.join(MARKER_FILE), "").expect("写标记");
    assert!(has_marker(&dir));
    assert_eq!(
        data_dir_for(&dir, has_marker(&dir)),
        Some(dir.join(DATA_SUBDIR))
    );

    // 同名的目录不算标记。
    let other = temp_dir("marker-dir");
    fs::create_dir(other.join(MARKER_FILE)).expect("建同名目录");
    assert!(!has_marker(&other));
    // 目录不存在也不算，也不 panic。
    assert!(!has_marker(&dir.join("missing")));
}

#[test]
fn a_writable_directory_is_detected_and_a_missing_one_is_not() {
    let dir = temp_dir("writable");
    assert!(dir_is_writable(&dir));
    assert_no_leftovers(&dir);
    let names: Vec<_> = fs::read_dir(&dir).expect("读目录").flatten().collect();
    assert!(names.is_empty(), "探测用的文件要删掉");
    assert!(!dir_is_writable(&dir.join("missing")));
}

#[test]
fn the_instance_hash_is_short_stable_and_ignores_case_and_separators() {
    let a = instance_hash(Path::new(r"D:\Tools\ZeeTerm\data"));
    assert_eq!(a.len(), 16);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    // 大小写、斜杠方向、末尾分隔符、`\\?\` 前缀：同一个目录必须同一个哈希。
    for same in [
        r"d:\tools\zeeterm\DATA",
        r"D:/Tools/ZeeTerm/data",
        r"D:\Tools\ZeeTerm\data\",
        r"\\?\D:\Tools\ZeeTerm\data",
    ] {
        assert_eq!(instance_hash(Path::new(same)), a, "{same}");
    }
    // 不同目录哈希不同（两份免安装版可以同时开）。
    let b = instance_hash(Path::new(r"E:\Tools\ZeeTerm\data"));
    assert_ne!(a, b);
    // 路径里有中文和空格也行。
    assert_eq!(
        instance_hash(Path::new(r"C:\Users\张 三\ZeeTerm\data")).len(),
        16
    );
}

#[test]
fn the_plan_accepts_only_a_cached_zip_with_the_verified_hash() {
    let cache = temp_dir("plan-cache");
    let outside = temp_dir("plan-outside");
    let zip = cache.join("ZeeTerm-2.0.0-windows-x64-portable.zip");
    good_zip(&zip);
    let sha = crate::sha256_hex(&read(&zip));
    let request = |package: &Path, sha: &str| PortableInstallRequest {
        package: package.to_path_buf(),
        cache_root: cache.clone(),
        version: "2.0.0".to_owned(),
        sha256: sha.to_owned(),
    };

    let plan = plan_portable_install(&request(&zip, &sha)).expect("正常的计划");
    assert_eq!(plan.version, "2.0.0");
    assert_eq!(plan.sha256, sha);
    assert!(
        plan.package
            .starts_with(cache.canonicalize().expect("规范化"))
    );

    // 哈希对不上（验签之后被换了）、大写哈希也认。
    assert_eq!(
        plan_portable_install(&request(&zip, &"0".repeat(64))),
        Err(InstallError::HashMismatch)
    );
    assert!(plan_portable_install(&request(&zip, &sha.to_uppercase())).is_ok());
    // 不在缓存目录里。
    let elsewhere = outside.join("a.zip");
    good_zip(&elsewhere);
    assert_eq!(
        plan_portable_install(&request(&elsewhere, &crate::sha256_hex(&read(&elsewhere)))),
        Err(InstallError::OutsideCache)
    );
    // 扩展名不是 zip（安装包不能拿来"更新"免安装版）。
    let exe = cache.join("ZeeTerm-Setup-2.0.0.exe");
    fs::write(&exe, fake_exe("setup")).expect("写 exe");
    assert_eq!(
        plan_portable_install(&request(&exe, &crate::sha256_hex(&read(&exe)))),
        Err(InstallError::NotZip)
    );
    // 不存在 / 是目录 / 版本为空。
    assert_eq!(
        plan_portable_install(&request(&cache.join("missing.zip"), &sha)),
        Err(InstallError::MissingInstaller)
    );
    let dir_zip = cache.join("dir.zip");
    fs::create_dir(&dir_zip).expect("建目录");
    assert_eq!(
        plan_portable_install(&request(&dir_zip, &sha)),
        Err(InstallError::NotAFile)
    );
    let mut empty_version = request(&zip, &sha);
    empty_version.version = "  ".to_owned();
    assert_eq!(
        plan_portable_install(&empty_version),
        Err(InstallError::EmptyVersion)
    );
}

#[test]
fn an_update_swaps_the_program_files_and_leaves_the_data_and_the_marker_alone() {
    let dir = temp_dir("apply");
    install_old(&dir);
    // 磁盘上的标记里放点东西：zip 里的 `portable` 是空的，要是被解出来就会把它覆盖掉。
    fs::write(dir.join(MARKER_FILE), "keep me").expect("写标记");
    let zip = dir.join("update.zip");
    good_zip(&zip);

    apply_portable_update_system(&zip, &layout_in(&dir)).expect("更新成功");

    assert_eq!(read(&dir.join(MAIN_EXE)), fake_exe("new-main"));
    assert_eq!(read(&dir.join(HELPER_EXE)), fake_exe("new-helper"));
    assert_eq!(read(&dir.join("LICENSE")), b"new license");
    assert_eq!(read(&dir.join("NOTICE")), b"new notice");
    assert_eq!(read(&dir.join(MARKER_FILE)), b"keep me", "标记文件不能动");
    assert_eq!(
        read(&dir.join(DATA_SUBDIR).join("hosts.db")),
        b"user data",
        "数据目录不能动"
    );
    assert_no_leftovers(&dir);
}

#[test]
fn only_the_whitelisted_top_level_files_are_unpacked() {
    let dir = temp_dir("whitelist");
    install_old(&dir);
    let zip = dir.join("update.zip");
    build_zip(
        &zip,
        &[
            (MAIN_EXE, &fake_exe("new-main")),
            (HELPER_EXE, &fake_exe("new-helper")),
            // 包里夹带的别的东西：不解压，也不报错（以后的包可能带新文件）。
            ("data/hosts.db", b"overwritten!"),
            ("sub/ZeeTerm.exe", b"MZnested"),
            ("evil.dll", b"MZ"),
        ],
    );
    apply_portable_update_system(&zip, &layout_in(&dir)).expect("更新成功");
    assert_eq!(read(&dir.join(MAIN_EXE)), fake_exe("new-main"));
    assert_eq!(read(&dir.join(DATA_SUBDIR).join("hosts.db")), b"user data");
    assert!(!dir.join("evil.dll").exists());
    // 包里没带许可文件：旧的原样留着。
    assert_eq!(read(&dir.join("LICENSE")), b"old license");
    assert_no_leftovers(&dir);
}

#[test]
fn a_renamed_program_is_replaced_in_place_under_its_own_name() {
    let dir = temp_dir("renamed");
    install_old(&dir);
    fs::rename(dir.join(MAIN_EXE), dir.join("MyTerm.exe")).expect("改名");
    let zip = dir.join("update.zip");
    good_zip(&zip);

    apply_portable_update_system(&zip, &PortableLayout::new(&dir.join("MyTerm.exe")))
        .expect("更新成功");
    assert_eq!(read(&dir.join("MyTerm.exe")), fake_exe("new-main"));
    assert!(!dir.join(MAIN_EXE).exists(), "不会凭空多出一个 ZeeTerm.exe");
    assert_eq!(read(&dir.join(HELPER_EXE)), fake_exe("new-helper"));
    assert_no_leftovers(&dir);
}

#[test]
fn a_stale_old_file_from_an_earlier_update_does_not_block_the_next_one() {
    let dir = temp_dir("stale-old");
    install_old(&dir);
    fs::write(dir.join("ZeeTerm.exe.old"), "older").expect("写旧文件");
    let zip = dir.join("update.zip");
    good_zip(&zip);
    apply_portable_update_system(&zip, &layout_in(&dir)).expect("更新成功");
    assert_eq!(read(&dir.join(MAIN_EXE)), fake_exe("new-main"));
    assert_no_leftovers(&dir);
}

/// 第 `fail_at` 次 `rename` 失败（从 1 数起）：不论卡在哪一步，磁盘上必须回到旧版本。
#[test]
fn a_failure_at_any_step_rolls_everything_back() {
    // 四个文件，每个先"旧文件改名"再"新文件移上去"：8 次 rename。
    for fail_at in 1..=8 {
        let dir = temp_dir(&format!("rollback-{fail_at}"));
        install_old(&dir);
        let zip = dir.join("update.zip");
        good_zip(&zip);

        let calls = Cell::new(0);
        let rename = |from: &Path, to: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == fail_at {
                Err(std::io::Error::other("simulated failure"))
            } else {
                fs::rename(from, to)
            }
        };
        let err =
            apply_portable_update(&zip, &layout_in(&dir), &PortableContext { rename: &rename })
                .expect_err("注入了失败");

        match &err {
            PortableError::Io {
                step,
                message,
                rolled_back,
            } => {
                let expected = if fail_at % 2 == 1 {
                    PortableStep::Backup
                } else {
                    PortableStep::Replace
                };
                assert_eq!(*step, expected, "第 {fail_at} 次");
                assert!(message.contains("simulated failure"));
                assert!(*rolled_back, "第 {fail_at} 次：应该完整回滚");
            }
            other => panic!("第 {fail_at} 次：{other:?}"),
        }
        assert!(err.to_string().contains("已恢复成旧版本"));
        assert_old_version_intact(&dir);
        assert_no_leftovers(&dir);
    }
}

#[test]
fn a_program_that_was_missing_before_is_removed_again_on_rollback() {
    let dir = temp_dir("rollback-missing");
    install_old(&dir);
    fs::remove_file(dir.join(HELPER_EXE)).expect("删辅助程序");
    let zip = dir.join("update.zip");
    good_zip(&zip);

    // 辅助程序原来不存在：它只有"移上去"一次 rename；让主程序的"移上去"失败。
    let calls = Cell::new(0);
    let rename = |from: &Path, to: &Path| {
        calls.set(calls.get() + 1);
        // LICENSE(2) + NOTICE(2) + 辅助程序(1) + 主程序备份(1) = 6，下一次是主程序移上去。
        if calls.get() == 7 {
            Err(std::io::Error::other("simulated failure"))
        } else {
            fs::rename(from, to)
        }
    };
    let err = apply_portable_update(&zip, &layout_in(&dir), &PortableContext { rename: &rename })
        .expect_err("注入了失败");
    assert!(matches!(
        err,
        PortableError::Io {
            step: PortableStep::Replace,
            rolled_back: true,
            ..
        }
    ));
    assert!(
        !dir.join(HELPER_EXE).exists(),
        "原来没有的文件回滚后仍然没有"
    );
    assert_eq!(read(&dir.join(MAIN_EXE)), fake_exe("old-main"));
    assert_eq!(read(&dir.join("LICENSE")), b"old license");
    assert_no_leftovers(&dir);
}

#[test]
fn an_unwritable_directory_means_manual_download_not_an_error() {
    let dir = temp_dir("unwritable");
    let zip = dir.join("update.zip");
    good_zip(&zip);
    // 程序所在目录不存在：建不了临时目录，和 Program Files / 只读盘上的表现一样。
    let missing = dir.join("no-such-dir");
    let err = apply_portable_update_system(&zip, &layout_in(&missing)).expect_err("不可写");
    assert!(matches!(err, PortableError::NotWritable(_)), "{err:?}");
    assert!(err.is_manual());
    assert!(
        !PortableError::BadArchive(String::new()).is_manual(),
        "别的错误不是手动下载"
    );
}

fn assert_rejected_untouched(
    name: &str,
    entries: &[(&str, &[u8])],
    check: impl Fn(&PortableError),
) {
    let dir = temp_dir(name);
    install_old(&dir);
    let zip = dir.join("update.zip");
    build_zip(&zip, entries);
    let err = apply_portable_update_system(&zip, &layout_in(&dir)).expect_err(name);
    check(&err);
    assert_old_version_intact(&dir);
    assert_no_leftovers(&dir);
}

#[test]
fn a_bad_archive_is_rejected_before_anything_is_touched() {
    let main = fake_exe("new-main");
    let helper = fake_exe("new-helper");

    // 路径穿越 / 绝对路径。
    assert_rejected_untouched(
        "traversal",
        &[
            (MAIN_EXE, &main),
            (HELPER_EXE, &helper),
            ("../escape.exe", b"MZ"),
        ],
        |err| assert!(matches!(err, PortableError::UnsafeEntry(_)), "{err:?}"),
    );
    assert_rejected_untouched(
        "absolute",
        &[
            (MAIN_EXE, &main),
            (HELPER_EXE, &helper),
            ("/abs/ZeeTerm.exe", b"MZ"),
        ],
        |err| assert!(matches!(err, PortableError::UnsafeEntry(_)), "{err:?}"),
    );
    // 缺主程序 / 缺辅助程序。
    assert_rejected_untouched("no-main", &[(HELPER_EXE, &helper)], |err| {
        assert_eq!(*err, PortableError::MissingFile(MAIN_EXE.to_owned()));
    });
    assert_rejected_untouched("no-helper", &[(MAIN_EXE, &main)], |err| {
        assert_eq!(*err, PortableError::MissingFile(HELPER_EXE.to_owned()));
    });
    // 空文件、不是 Windows 程序。
    assert_rejected_untouched(
        "empty-exe",
        &[(MAIN_EXE, b""), (HELPER_EXE, &helper)],
        |err| assert_eq!(*err, PortableError::BadFile(MAIN_EXE.to_owned())),
    );
    assert_rejected_untouched(
        "not-pe",
        &[(MAIN_EXE, &main), (HELPER_EXE, b"#!/bin/sh\n")],
        |err| assert_eq!(*err, PortableError::BadFile(HELPER_EXE.to_owned())),
    );
}

#[test]
fn too_many_entries_and_non_zip_files_are_rejected() {
    let dir = temp_dir("bad-zip");
    install_old(&dir);
    let layout = layout_in(&dir);

    let garbage = dir.join("garbage.zip");
    fs::write(&garbage, b"this is not a zip file").expect("写垃圾");
    assert!(matches!(
        apply_portable_update_system(&garbage, &layout),
        Err(PortableError::BadArchive(_))
    ));
    assert!(matches!(
        apply_portable_update_system(&dir.join("missing.zip"), &layout),
        Err(PortableError::BadArchive(_))
    ));

    let many = dir.join("many.zip");
    let names: Vec<String> = (0..=MAX_ENTRIES).map(|n| format!("f{n}.txt")).collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    build_zip(&many, &entries);
    assert!(matches!(
        apply_portable_update_system(&many, &layout),
        Err(PortableError::UnsafeEntry(_))
    ));

    assert_old_version_intact(&dir);
    assert_no_leftovers(&dir);
}

#[test]
fn a_symlink_entry_is_rejected() {
    let dir = temp_dir("symlink");
    install_old(&dir);
    let zip = dir.join("update.zip");
    let file = fs::File::create(&zip).expect("建 zip");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file(MAIN_EXE, options).expect("主程序");
    writer.write_all(&fake_exe("new-main")).expect("写主程序");
    writer.start_file(HELPER_EXE, options).expect("辅助程序");
    writer
        .write_all(&fake_exe("new-helper"))
        .expect("写辅助程序");
    writer
        .add_symlink("link", "C:\\Windows\\System32\\cmd.exe", options)
        .expect("软链接");
    writer.finish().expect("收尾");

    let err = apply_portable_update_system(&zip, &layout_in(&dir)).expect_err("软链接");
    assert!(matches!(err, PortableError::UnsafeEntry(_)), "{err:?}");
    assert_old_version_intact(&dir);
    assert_no_leftovers(&dir);
}

#[test]
fn startup_cleanup_removes_old_files_but_keeps_everything_else() {
    let dir = temp_dir("cleanup");
    install_old(&dir);
    for old in [
        "ZeeTerm.exe.old",
        "zssh-mcp.exe.old",
        "LICENSE.old",
        "NOTICE.old",
    ] {
        fs::write(dir.join(old), "leftover").expect("写旧文件");
    }
    // 别人的东西不能删：同样以 .old 结尾的无关文件，和一个刚建的（可能正在更新的）临时目录。
    fs::write(dir.join("notes.txt.old"), "mine").expect("写无关文件");
    let fresh_stage = dir.join(format!("{STAGE_PREFIX}-fresh"));
    fs::create_dir(&fresh_stage).expect("建临时目录");

    let removed = cleanup_leftovers(&layout_in(&dir));
    assert_eq!(removed.len(), 4, "{removed:?}");
    for old in [
        "ZeeTerm.exe.old",
        "zssh-mcp.exe.old",
        "LICENSE.old",
        "NOTICE.old",
    ] {
        assert!(!dir.join(old).exists(), "{old}");
    }
    assert!(dir.join("notes.txt.old").is_file());
    assert!(fresh_stage.is_dir());
    assert_eq!(read(&dir.join(MAIN_EXE)), fake_exe("old-main"));
    assert_eq!(read(&dir.join(DATA_SUBDIR).join("hosts.db")), b"user data");

    // 没有东西可清理时什么都不做；目录不存在也不 panic。
    assert!(cleanup_leftovers(&layout_in(&dir)).is_empty());
    assert!(cleanup_leftovers(&layout_in(&dir.join("missing"))).is_empty());
}

#[test]
fn a_renamed_programs_old_file_is_cleaned_up_too() {
    let dir = temp_dir("cleanup-renamed");
    install_old(&dir);
    fs::write(dir.join("MyTerm.exe"), fake_exe("x")).expect("写程序");
    fs::write(dir.join("MyTerm.exe.old"), "leftover").expect("写旧文件");
    let removed = cleanup_leftovers(&PortableLayout::new(&dir.join("MyTerm.exe")));
    assert_eq!(removed, vec![dir.join("MyTerm.exe.old")]);
}

#[test]
fn the_restart_arguments_make_the_new_process_wait_for_the_old_one() {
    let args = restart_args(4242, &["--foo".to_owned(), "bar".to_owned()]);
    assert_eq!(
        args,
        vec![
            OsString::from("--zeeterm-wait-pid=4242"),
            OsString::from("--foo"),
            OsString::from("bar")
        ]
    );
    // 新进程能从自己的命令行里读出来（第一个是程序名）。
    let mut command_line = vec![OsString::from("ZeeTerm.exe")];
    command_line.extend(args);
    assert_eq!(wait_pid_from_args(command_line), Some(4242));

    // 重启两次不会积累：旧的等待参数被换掉。
    let again = restart_args(
        99,
        &["--zeeterm-wait-pid=4242".to_owned(), "--foo".to_owned()],
    );
    assert_eq!(
        again,
        vec![
            OsString::from("--zeeterm-wait-pid=99"),
            OsString::from("--foo")
        ]
    );
    assert_eq!(
        without_wait_arg(&["--zeeterm-wait-pid=1".to_owned(), "x".to_owned()]),
        vec!["x".to_owned()]
    );

    // 读不出来的情况都当没有：没有、不是数字、0、只有名字没有等号、第一个参数（程序名）。
    for bad in [
        vec!["ZeeTerm.exe"],
        vec!["ZeeTerm.exe", "--zeeterm-wait-pid=abc"],
        vec!["ZeeTerm.exe", "--zeeterm-wait-pid=0"],
        vec!["ZeeTerm.exe", "--zeeterm-wait-pid"],
        vec!["ZeeTerm.exe", "--zeeterm-wait-pid=-5"],
        vec!["--zeeterm-wait-pid=7"],
    ] {
        assert_eq!(
            wait_pid_from_args(bad.iter().map(OsString::from)),
            None,
            "{bad:?}"
        );
    }
}
