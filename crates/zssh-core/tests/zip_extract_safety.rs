//! 解压 opencode 压缩包的路径安全：条目名想逃出目标目录（`../`、绝对路径、盘符）时，
//! 解压必须当它不存在，什么都不能写到目标目录之外。
//!
//! 项目自己用 `enclosed_name` 逐条解压，不走 `ZipArchive::extract`；升级 zip 库之后
//! 这条行为不能变（见 `zssh_core::ai::agent::download::extract_exe`）。

// 集成测试在 crate 外面，clippy.toml 里「测试里允许 unwrap」的放行对它不生效；
// 测试失败就让它 panic。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;

use zssh_core::ai::agent::download::extract_exe;

fn write_zip(path: &Path, names: &[&str]) {
    let file = std::fs::File::create(path).expect("建 zip");
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for name in names {
        writer.start_file(*name, options).expect("写条目");
        writer.write_all(b"EVIL").expect("写内容");
    }
    writer.finish().expect("收尾");
}

fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("zssh-zip-{tag}-"))
        .tempdir()
        .expect("临时目录")
}

/// 目标目录之外不能多出任何文件：除了压缩包本身，临时根目录下只允许有 `sandbox`。
fn assert_nothing_outside(root: &Path, out_dir: &Path, evil: &str) {
    assert!(
        !out_dir
            .parent()
            .expect("上一级")
            .join("opencode.exe")
            .exists(),
        "{evil:?} 跑到了目标目录的上一级"
    );
    let stray: Vec<_> = std::fs::read_dir(root)
        .expect("列目录")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "evil.zip" && name != "sandbox")
        .collect();
    assert!(stray.is_empty(), "{evil:?} 在目标目录之外写出了 {stray:?}");
}

#[test]
fn entries_that_climb_out_of_the_target_directory_are_ignored() {
    for evil in [
        "../opencode.exe",
        r"..\opencode.exe",
        "a/../../opencode.exe",
        r"a\..\..\opencode.exe",
    ] {
        let root = temp_dir("escape");
        let zip_path = root.path().join("evil.zip");
        let out_dir = root.path().join("sandbox").join("out");
        write_zip(&zip_path, &[evil]);

        let err = extract_exe(&zip_path, &out_dir)
            .expect_err(&format!("{evil:?} 不是安全的条目名，应当当作没有 exe"));
        assert!(err.contains("没有 opencode.exe"), "{evil:?}: {err}");
        assert!(!out_dir.join("opencode.exe").exists(), "{evil:?}");
        assert_nothing_outside(root.path(), &out_dir, evil);
    }
}

/// 绝对路径 / 盘符开头的条目名：解压的目的地永远是 `<目标目录>/opencode.exe`（不用条目里的路径），
/// 不管库把这种名字当成不安全还是削成相对路径，都不能写到目标目录之外。
#[test]
fn absolute_entry_names_never_write_outside_the_target_directory() {
    for evil in [
        "/opencode.exe",
        r"\opencode.exe",
        r"C:\opencode.exe",
        "C:/Windows/opencode.exe",
    ] {
        let root = temp_dir("absolute");
        let zip_path = root.path().join("evil.zip");
        let out_dir = root.path().join("sandbox").join("out");
        write_zip(&zip_path, &[evil]);

        if let Ok(exe) = extract_exe(&zip_path, &out_dir) {
            assert_eq!(exe, out_dir.join("opencode.exe"), "{evil:?}");
        }
        assert_nothing_outside(root.path(), &out_dir, evil);
    }
}

#[test]
fn a_safe_entry_still_extracts_next_to_unsafe_ones() {
    let root = temp_dir("mixed");
    let zip_path = root.path().join("mixed.zip");
    let out_dir = root.path().join("out");
    write_zip(&zip_path, &["../opencode.exe", "bin/opencode.exe"]);

    let exe = extract_exe(&zip_path, &out_dir).expect("安全的那一条要能解出来");
    assert_eq!(exe, out_dir.join("opencode.exe"));
    assert!(!root.path().join("opencode.exe").exists());
}
