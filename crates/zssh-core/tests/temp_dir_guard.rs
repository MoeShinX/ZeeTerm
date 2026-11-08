//! 守卫：测试代码里的临时目录必须用 `tempfile`（随机唯一的名字，守卫 drop 时整棵删掉）。
//!
//! 为什么：
//! 1. 目录名只用 `std::process::id()` 拼：Windows 会复用进程号，后来的测试进程会落进上次残留的目录，
//!    打开上次的 `zssh.db`（里面可能已经设了锁屏密码等），导致偶发失败；而且一般没人删，系统临时目录里
//!    会越积越多（曾经攒到三万多个）。
//! 2. 在 `Drop` 里 `remove_dir_all` 一个同时握着 `Database` 的结构体：`Drop::drop` 的函数体先于字段析构执行，
//!    删目录时库文件还开着，Windows 上删不掉，`let _ =` 又把错误吞了。要让 `db` 字段排在 `tempfile::TempDir`
//!    字段前面（字段按声明顺序析构），自己不写 `Drop`。
//!
//! 只扫测试代码：`tests/`、`examples/` 目录、`*_tests.rs` / `tests.rs` / `test_*.rs` 这类只给测试用的文件，
//! 以及其它文件里 `#[cfg(test)] mod` 开始的尾部。产品代码（更新下载、安装包工作目录等）用 `temp_dir()` 是正当的，不管。

use std::path::{Path, PathBuf};

/// (文件路径后缀, 理由)：确实需要按进程号拼固定路径、又没法换成随机目录的测试。目前没有。
/// 加条目时必须写明理由；条目对应的文件一旦不再命中，守卫会要求删掉这一项。
const ALLOWED_PID_TEMP_DIRS: &[(&str, &str)] = &[];

/// 只给测试用的文件（对应的 `mod` 带 `#[cfg(test)]`，或本来就在 `tests/`、`examples/` 里）。
fn is_test_only_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = name.strip_suffix(".rs").unwrap_or(name);
    rel.contains("/tests/")
        || rel.contains("/examples/")
        || stem == "tests"
        || stem.ends_with("_tests")
        || stem.ends_with("_test")
        || stem.starts_with("test_")
        || stem == "live_support"
        || stem == "installer_manifest"
}

/// 文件里 `#[cfg(test)]` 紧跟 `mod` 的测试模块（它总在文件末尾）；没有就返回 `None`。
fn test_part(text: &str) -> Option<&str> {
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        if line.trim() == "#[cfg(test)]" {
            if let Some(next) = lines.peek() {
                let next = next.trim_start();
                if next.starts_with("mod ") || next.starts_with("pub mod ") {
                    return Some(&text[offset..]);
                }
            }
        }
        offset += line.len();
    }
    None
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

/// 全部 Rust 源码：(相对仓库根的路径, 内容)。
fn all_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for group in ["crates", "native"] {
        let Ok(members) = std::fs::read_dir(root.join(group)) else {
            continue;
        };
        for member in members.flatten() {
            for sub in ["src", "tests", "examples"] {
                collect(&member.path().join(sub), &mut files);
            }
        }
    }
    let mut out = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let rel = rel.trim_start_matches("../").trim_start_matches("./");
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push((rel.to_owned(), text));
        }
    }
    out
}

/// 测试代码部分：整份（只给测试用的文件）或 `#[cfg(test)] mod` 的尾部。
fn test_code<'a>(rel: &str, text: &'a str) -> Option<&'a str> {
    if is_test_only_file(rel) {
        Some(text)
    } else {
        test_part(text)
    }
}

/// 行号（从 1 开始）。
fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

/// 去掉 `//` 注释，避免文档里举例的写法被当成代码。
fn without_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| {
            // 不处理字符串里的 `//`（比如 URL）：宁可多留一点，守卫只会因此多看，不会漏看。
            match line.find("//") {
                Some(at) if !line[..at].contains('"') => &line[..at],
                _ => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `std::env::temp_dir()` 所在的语句（到下一个分号为止）里出现了 `process::id()`：目录名按进程号拼。
fn pid_named_temp_dirs(code: &str) -> Vec<usize> {
    let code = without_line_comments(code);
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(found) = code[from..].find("temp_dir()") {
        let at = from + found;
        let end = code[at..].find(';').map_or(code.len(), |semi| at + semi);
        if code[at..end].contains("process::id()") {
            hits.push(line_of(&code, at));
        }
        from = at + "temp_dir()".len();
    }
    hits
}

/// 同一个文件里 `impl Drop for X`、函数体删目录，而 `struct X` 里有 `Database`：Drop 先于字段析构，
/// 删目录时库还开着。
fn drop_removes_dir_while_holding_a_database(code: &str) -> Vec<usize> {
    let code = without_line_comments(code);
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(found) = code[from..].find("impl Drop for ") {
        let at = from + found;
        from = at + "impl Drop for ".len();
        let name: String = code[from..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        // 这个 impl 的函数体：到下一个 `impl ` 或再往后 600 个字符里先到的那个。
        let rest = &code[at..];
        let stop = rest[1..]
            .find("\nimpl ")
            .map_or(rest.len(), |next| next + 1)
            .min(600);
        let mut stop_at = stop;
        while !rest.is_char_boundary(stop_at) {
            stop_at -= 1;
        }
        if !rest[..stop_at].contains("remove_dir_all") {
            continue;
        }
        let needle = format!("struct {name}");
        let Some(def) = code.find(&needle) else {
            continue;
        };
        // 结构体定义：从 `struct X` 到第一个 `}`（元组结构体到 `;`）。
        let after = &code[def..];
        let close = after
            .find('}')
            .into_iter()
            .chain(after.find(';'))
            .min()
            .unwrap_or(after.len());
        if after[..close].contains("Database") {
            hits.push(line_of(&code, at));
        }
    }
    hits
}

#[test]
fn test_code_never_names_its_temp_dir_after_the_process_id() {
    let sources = all_sources();
    assert!(
        sources.iter().any(|(rel, _)| rel.ends_with("test_util.rs")),
        "没扫到源码，守卫失效了"
    );
    let mut bad = Vec::new();
    let mut allowed_seen = vec![false; ALLOWED_PID_TEMP_DIRS.len()];
    for (rel, text) in &sources {
        // 本文件的字符串样例里就写着要抓的写法。
        if rel.ends_with("tests/temp_dir_guard.rs") {
            continue;
        }
        let Some(code) = test_code(rel, text) else {
            continue;
        };
        let hits = pid_named_temp_dirs(code);
        if hits.is_empty() {
            continue;
        }
        if let Some(idx) = ALLOWED_PID_TEMP_DIRS
            .iter()
            .position(|(suffix, _)| rel.ends_with(suffix))
        {
            allowed_seen[idx] = true;
            continue;
        }
        // 行号相对测试部分的开头不好找，只报文件和次数，再让人 grep。
        bad.push(format!("{rel}（{} 处）", hits.len()));
    }
    assert!(
        bad.is_empty(),
        "这些测试用 std::env::temp_dir() + 进程号拼临时目录：Windows 会复用进程号，\
         后来的测试进程会落进上次残留的目录。请改用 \
         tempfile::Builder::new().prefix(\"zssh-<用途>-\").tempdir()（随机唯一、drop 时删掉）；\
         确实不行就加进 ALLOWED_PID_TEMP_DIRS 并写明理由：\n{}",
        bad.join("\n")
    );
    for (seen, (suffix, _)) in allowed_seen.iter().zip(ALLOWED_PID_TEMP_DIRS) {
        assert!(
            *seen,
            "白名单里的 {suffix} 已经没有按进程号拼的临时目录了，请删掉这一项"
        );
    }
}

#[test]
fn test_helpers_never_delete_a_directory_in_drop_while_holding_a_database() {
    let mut bad = Vec::new();
    for (rel, text) in &all_sources() {
        if rel.ends_with("tests/temp_dir_guard.rs") {
            continue;
        }
        let Some(code) = test_code(rel, text) else {
            continue;
        };
        for line in drop_removes_dir_while_holding_a_database(code) {
            bad.push(format!("{rel}（测试部分第 {line} 行起）"));
        }
    }
    assert!(
        bad.is_empty(),
        "这些测试辅助结构在 Drop 里删目录，而结构体里还握着 Database：Drop::drop 先于字段析构，\
         删的时候库文件还开着（Windows 上删不掉）。去掉自定义 Drop，字段改成 `db` 在前、\
         `_dir: tempfile::TempDir` 在后：\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_detector_flags_pid_named_temp_dirs() {
    let bad_one =
        "let dir = std::env::temp_dir().join(format!(\"zssh-x-{}\", std::process::id()));";
    assert_eq!(pid_named_temp_dirs(bad_one), vec![1]);
    let bad_multi_line = "fn a() {}\nlet dir =\n    std::env::temp_dir().join(format!(\n        \"zssh-x-{}-{name}\",\n        std::process::id()\n    ));\n";
    assert_eq!(pid_named_temp_dirs(bad_multi_line), vec![3]);
}

#[test]
fn the_detector_leaves_alone_what_is_fine() {
    // 随机目录。
    assert!(pid_named_temp_dirs(
        "let dir = tempfile::Builder::new().prefix(\"zssh-x-\").tempdir().unwrap();"
    )
    .is_empty());
    // 只取系统临时目录、不拼进程号。
    assert!(
        pid_named_temp_dirs("let dir = std::env::temp_dir();\nlet id = std::process::id();")
            .is_empty()
    );
    // 进程号用在别处（语句不同）。
    assert!(pid_named_temp_dirs(
        "let a = std::env::temp_dir().join(\"fixed\");\nlet b = format!(\"{}\", std::process::id());"
    )
    .is_empty());
    // 注释里举例的写法不算。
    assert!(pid_named_temp_dirs(
        "// 以前是 temp_dir().join(format!(\"x-{}\", std::process::id()));\nlet a = 1;"
    )
    .is_empty());
}

#[test]
fn the_detector_flags_a_drop_that_deletes_the_dir_under_an_open_database() {
    let bad = "struct TempDb {\n    dir: PathBuf,\n    db: Database,\n}\n\nimpl Drop for TempDb {\n    fn drop(&mut self) {\n        let _ = std::fs::remove_dir_all(&self.dir);\n    }\n}\n";
    assert_eq!(drop_removes_dir_while_holding_a_database(bad), vec![6]);
    // 没握着库：Drop 里删目录是可靠的。
    let fine = "struct Scratch(PathBuf);\n\nimpl Drop for Scratch {\n    fn drop(&mut self) {\n        let _ = std::fs::remove_dir_all(&self.0);\n    }\n}\n";
    assert!(drop_removes_dir_while_holding_a_database(fine).is_empty());
    // 握着库、但不在 Drop 里删目录（字段顺序负责）。
    let ordered = "struct TempDb {\n    db: Database,\n    _dir: tempfile::TempDir,\n}\n";
    assert!(drop_removes_dir_while_holding_a_database(ordered).is_empty());
}

#[test]
fn test_part_cuts_at_the_test_module_and_file_names_are_recognised() {
    let text = "fn a() { temp_dir(); }\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n";
    assert_eq!(
        test_part(text),
        Some("#[cfg(test)]\nmod tests {\n    fn b() {}\n}\n")
    );
    assert_eq!(test_part("fn a() {}\n#[cfg(test)]\nfn only() {}\n"), None);
    for name in [
        "crates/zssh-core/tests/x.rs",
        "native/zssh-gpui/src/sftp/tests.rs",
        "native/zssh-gpui/src/sync/stub_tests.rs",
        "native/zssh-gpui/src/main_window/host_test.rs",
        "native/zssh-gpui/src/test_sftp_server.rs",
        "native/zssh-gpui/src/live_support.rs",
    ] {
        assert!(is_test_only_file(name), "{name}");
    }
    for name in [
        "native/zssh-gpui/src/update.rs",
        "crates/zssh-core/src/sftp/local.rs",
    ] {
        assert!(!is_test_only_file(name), "{name}");
    }
}
