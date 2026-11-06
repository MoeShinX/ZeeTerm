//! 守卫：发行版是 GUI 子系统，GUI 程序启动控制台子程序时系统会为每个子程序弹一个新的
//! 控制台窗口。所以产品代码里每个 `Command::new(` 所在的函数都必须调用 `hide_console`
//! （见 `zssh_core::process_util`），或者在下面的白名单里并写明理由（启动的是 GUI 程序）。
//!
//! 只扫产品代码：测试模块（`#[cfg(test)] mod`）、`tests/` 目录和只给测试用的文件不管，
//! 测试是在控制台里跑的。

use std::path::{Path, PathBuf};

/// (文件路径后缀, 理由)：这些地方启动的是 GUI 程序，不需要、也不该隐藏。
const ALLOWED_GUI_LAUNCHES: &[(&str, &str)] = &[
    (
        "crates/zssh-core/src/x11/local/system.rs",
        "启动 VcXsrv（本地 X 服务器，GUI 程序），窗口由它自己管",
    ),
    (
        "native/zssh-gpui/src/main_window/settings_page.rs",
        "用 explorer 打开日志目录：用户就是要看到资源管理器窗口",
    ),
    (
        "native/zssh-gpui/src/settings.rs",
        "macOS 用 osascript 弹系统对话框；没有可隐藏的控制台窗口",
    ),
];

/// 只给测试用的文件（对应的 `mod` 带 `#[cfg(test)]`）。
fn is_test_only_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.contains("/tests/")
        || rel.contains("/examples/")
        || name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name == "test_util.rs"
        || name == "live_support.rs"
        || name == "installer_manifest.rs"
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// 去掉文件里 `#[cfg(test)]` 紧跟 `mod` 的测试模块（它总在文件末尾）。
fn production_part(text: &str) -> &str {
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "#[cfg(test)]" {
            if let Some(next) = lines.peek() {
                let next = next.trim_start();
                if next.starts_with("mod ") || next.starts_with("pub mod ") {
                    return &text[..offset];
                }
            }
        }
        offset += line.len();
    }
    text
}

fn is_fn_line(line: &str) -> bool {
    let t = line.trim_start();
    let t = t.strip_prefix("pub(crate) ").unwrap_or(t);
    let t = t.strip_prefix("pub ").unwrap_or(t);
    let t = t.strip_prefix("async ").unwrap_or(t);
    t.starts_with("fn ")
}

/// `Command::new(` 所在的函数（从上一行 `fn` 到下一行 `fn`）里有没有 `hide_console`。
fn function_around(lines: &[&str], at: usize) -> String {
    let start = (0..=at).rev().find(|&i| is_fn_line(lines[i])).unwrap_or(0);
    let end = (at + 1..lines.len())
        .find(|&i| is_fn_line(lines[i]))
        .unwrap_or(lines.len());
    lines[start..end].join("\n")
}

fn product_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    let Ok(crates) = std::fs::read_dir(root.join("crates")) else {
        return Vec::new();
    };
    for c in crates.flatten() {
        collect(&c.path().join("src"), &mut files);
    }
    let Ok(native) = std::fs::read_dir(root.join("native")) else {
        return Vec::new();
    };
    for n in native.flatten() {
        collect(&n.path().join("src"), &mut files);
    }
    let mut out = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let rel = rel.trim_start_matches("../").to_owned();
        let rel = rel.trim_start_matches("./").to_owned();
        if is_test_only_file(&rel) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push((rel, text));
        }
    }
    out
}

#[test]
fn every_spawned_console_program_hides_its_window() {
    let sources = product_sources();
    assert!(
        sources
            .iter()
            .any(|(rel, _)| rel.ends_with("ai/agent/discovery.rs")),
        "没扫到产品源码，守卫失效了"
    );
    let mut bad = Vec::new();
    let mut allowed_seen = vec![false; ALLOWED_GUI_LAUNCHES.len()];
    for (rel, text) in &sources {
        let body = production_part(text);
        let lines: Vec<&str> = body.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if !code.contains("Command::new(") {
                continue;
            }
            if let Some(idx) = ALLOWED_GUI_LAUNCHES
                .iter()
                .position(|(suffix, _)| rel.ends_with(suffix))
            {
                allowed_seen[idx] = true;
                continue;
            }
            if !function_around(&lines, i).contains("hide_console") {
                bad.push(format!("{rel}:{}：{}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "这些地方启动了子进程却没调用 process_util::hide_console（发行版会弹控制台窗口）；\
         启动的是 GUI 程序就加进 ALLOWED_GUI_LAUNCHES 并写明理由：\n{}",
        bad.join("\n")
    );
    for (seen, (suffix, _)) in allowed_seen.iter().zip(ALLOWED_GUI_LAUNCHES) {
        assert!(
            *seen,
            "白名单里的 {suffix} 已经没有 Command::new 了，请删掉这一项"
        );
    }
}

#[test]
fn production_part_cuts_the_test_module() {
    let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n";
    assert_eq!(production_part(text), "fn a() {}\n");
    let no_tests = "fn a() {}\n#[cfg(test)]\nfn only_test() {}\n";
    assert_eq!(production_part(no_tests), no_tests);
}

#[test]
fn the_guard_flags_a_function_without_the_call() {
    let lines = vec![
        "fn good() {",
        "    let mut c = Command::new(\"x\");",
        "    hide_console(&mut c);",
        "}",
        "fn bad() {",
        "    Command::new(\"y\");",
        "}",
    ];
    assert!(function_around(&lines, 1).contains("hide_console"));
    assert!(!function_around(&lines, 5).contains("hide_console"));
}
