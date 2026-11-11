//! 远端压缩 / 解压的单测：命令拼装、名字、安全检查、冲突计算、`-k` 退出码，以及用内存实现
//! 走一遍压缩 / 解压的完整流程（成功、失败、取消、重名、回退）。

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use super::*;

/// 解压命令现在是"先在同一命令里复核条目，再真正解压"（见 [`validate_list_command`]）。
fn with_recheck(format: ArchiveFormat, archive: &str, worker: &str) -> String {
    guarded(&format!(
        "{} && {worker}",
        validate_list_command(format, archive)
    ))
}

// ---------------------------------------------------------------------------
// 内存实现
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeFs {
    nodes: Mutex<BTreeMap<String, CopyAttrs>>,
    removed: Mutex<Vec<String>>,
    renames: Mutex<Vec<(String, String)>>,
}

impl FakeFs {
    fn put_file(&self, path: &str, size: u64) {
        self.nodes.lock().unwrap().insert(
            path.to_owned(),
            CopyAttrs {
                is_dir: false,
                is_symlink: false,
                size: Some(size),
                mtime: Some(1_700_000_000),
                permissions: Some(0o644),
            },
        );
    }

    fn put_dir(&self, path: &str) {
        self.nodes.lock().unwrap().insert(
            path.to_owned(),
            CopyAttrs {
                is_dir: true,
                is_symlink: false,
                size: None,
                mtime: None,
                permissions: Some(0o755),
            },
        );
    }

    fn has(&self, path: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(path)
    }
}

impl ArchiveFs for FakeFs {
    fn stat_path<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<CopyAttrs, SftpError>> {
        Box::pin(async move {
            self.nodes
                .lock()
                .unwrap()
                .get(path)
                .copied()
                .ok_or_else(|| SftpError::NotFound {
                    path: path.to_owned(),
                })
        })
    }

    fn rename_new<'a>(
        &'a self,
        from: &'a str,
        to: &'a str,
    ) -> BoxFuture<'a, Result<(), SftpError>> {
        Box::pin(async move {
            let mut nodes = self.nodes.lock().unwrap();
            if nodes.contains_key(to) {
                return Err(SftpError::AlreadyExists {
                    path: to.to_owned(),
                });
            }
            let Some(node) = nodes.remove(from) else {
                return Err(SftpError::NotFound {
                    path: from.to_owned(),
                });
            };
            nodes.insert(to.to_owned(), node);
            self.renames
                .lock()
                .unwrap()
                .push((from.to_owned(), to.to_owned()));
            Ok(())
        })
    }

    fn remove_all<'a>(&'a self, path: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut nodes = self.nodes.lock().unwrap();
            nodes.retain(|key, _| key != path && !key.starts_with(&format!("{path}/")));
            self.removed.lock().unwrap().push(path.to_owned());
        })
    }
}

type Responder = Box<dyn Fn(&str) -> Result<ExecOutput, ArchiveError> + Send + Sync>;

/// 记录每条命令，按命令内容决定返回什么（可以顺手改内存文件系统，模拟命令的副作用）。
struct FakeRunner {
    commands: Mutex<Vec<String>>,
    responder: Responder,
}

impl FakeRunner {
    fn new(
        responder: impl Fn(&str) -> Result<ExecOutput, ArchiveError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            responder: Box::new(responder),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

impl ExecRunner for FakeRunner {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        _max_stream_bytes: usize,
        _cancel: &'a AtomicBool,
    ) -> BoxFuture<'a, Result<ExecOutput, ArchiveError>> {
        Box::pin(async move {
            self.commands.lock().unwrap().push(command.to_owned());
            (self.responder)(command)
        })
    }
}

fn ok(stdout: &str) -> ExecOutput {
    ExecOutput {
        exit_code: Some(0),
        stdout: stdout.to_owned(),
        ..ExecOutput::default()
    }
}

fn fail(code: u32, stderr: &str) -> ExecOutput {
    ExecOutput {
        exit_code: Some(code),
        stderr: stderr.to_owned(),
        ..ExecOutput::default()
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

/// 命令里 `marker` 后面第一个单引号包起来的内容（测试里的路径不含单引号）。
fn quoted_after(command: &str, marker: &str) -> String {
    let rest = &command[command.find(marker).expect("marker") + marker.len()..];
    let start = rest.find('\'').expect("open") + 1;
    let end = start + rest[start..].find('\'').expect("close");
    rest[start..end].to_owned()
}

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

// ---------------------------------------------------------------------------
// 后缀、名字
// ---------------------------------------------------------------------------

#[test]
fn detects_every_supported_suffix_case_insensitively() {
    let cases = [
        ("a.tar.gz", ArchiveFormat::TarGz),
        ("a.tgz", ArchiveFormat::TarGz),
        ("a.TAR.GZ", ArchiveFormat::TarGz),
        ("a.tar", ArchiveFormat::Tar),
        ("a.tar.xz", ArchiveFormat::TarXz),
        ("a.txz", ArchiveFormat::TarXz),
        ("a.tar.bz2", ArchiveFormat::TarBz2),
        ("a.tbz2", ArchiveFormat::TarBz2),
        ("a.zip", ArchiveFormat::Zip),
        ("备份 2026.ZIP", ArchiveFormat::Zip),
    ];
    for (name, expected) in cases {
        assert_eq!(ArchiveFormat::detect(name), Some(expected), "{name}");
    }
    for name in [
        "a.gz",
        "a.rar",
        "a.7z",
        "zip",
        ".zip",
        ".tar.gz",
        "a.tar.gz.bak",
        "a.tgzz",
    ] {
        assert_eq!(ArchiveFormat::detect(name), None, "{name}");
    }
}

#[test]
fn extract_folder_name_drops_the_archive_suffix() {
    assert_eq!(extract_folder_name("site.tar.gz").as_deref(), Some("site"));
    assert_eq!(extract_folder_name("site.tgz").as_deref(), Some("site"));
    assert_eq!(
        extract_folder_name("my site.ZIP").as_deref(),
        Some("my site")
    );
    assert_eq!(extract_folder_name("a.b.tar.xz").as_deref(), Some("a.b"));
    assert_eq!(extract_folder_name("中文包.tar").as_deref(), Some("中文包"));
    assert_eq!(
        extract_folder_name("..zip"),
        None,
        "主名是 .. 不能当文件夹名"
    );
    assert_eq!(extract_folder_name(".tar.gz"), None);
    assert_eq!(extract_folder_name("readme.txt"), None);
}

#[test]
fn archive_base_name_follows_the_selection_or_the_directory() {
    assert_eq!(archive_base_name(&names(&["logs"]), "/var/www"), "logs");
    assert_eq!(archive_base_name(&names(&["a.txt"]), "/var/www"), "a.txt");
    assert_eq!(archive_base_name(&names(&["a", "b"]), "/var/www"), "www");
    assert_eq!(archive_base_name(&names(&["a", "b"]), "/var/www/"), "www");
    assert_eq!(archive_base_name(&names(&["a", "b"]), "/"), "archive");
}

#[test]
fn numbered_names_keep_the_double_extension_intact() {
    assert_eq!(numbered_name("logs", ".tar.gz", 0), "logs.tar.gz");
    assert_eq!(numbered_name("logs", ".tar.gz", 1), "logs (1).tar.gz");
    assert_eq!(numbered_name("logs", ".zip", 2), "logs (2).zip");
}

#[tokio::test]
async fn first_free_name_skips_taken_names_and_never_reuses_one() {
    let fs = FakeFs::default();
    fs.put_file("/d/logs.tar.gz", 1);
    fs.put_dir("/d/logs (1).tar.gz");
    fs.put_file("/d/logs (2).tar.gz", 1);
    let name = first_free_name(&fs, "/d", "logs", ".tar.gz").await.unwrap();
    assert_eq!(name, "logs (3).tar.gz");
    let name = first_free_name(&fs, "/d", "other", ".zip").await.unwrap();
    assert_eq!(name, "other.zip");
}

#[test]
fn temp_names_are_hidden_siblings_with_the_owner_and_part_suffix() {
    let temp = temp_path("/d", "logs.tar.gz", "t1");
    assert_eq!(temp, "/d/.logs.tar.gz.t1.zssh-part");
    assert_eq!(temp_path("/", "a.zip", "t1"), "/.a.zip.t1.zssh-part");
}

// ---------------------------------------------------------------------------
// 命令拼装
// ---------------------------------------------------------------------------

#[test]
fn compress_command_quotes_every_awkward_name() {
    let list = names(&[
        "plain",
        "with space",
        "it's",
        "-dash",
        "中文 名",
        "$(touch x)",
        "`id`",
    ]);
    let command = compress_command(
        "/home/a b",
        "/home/a b/.t.zssh-part",
        CompressFormat::TarGz,
        &list,
    );
    assert_eq!(
        command,
        "cd '/home/a b' && exec tar -czf '/home/a b/.t.zssh-part' -- 'plain' 'with space' 'it'\\''s' \
         '-dash' '中文 名' '$(touch x)' '`id`'"
    );
    let zip = compress_command(
        "/d",
        "/d/.t.zssh-part",
        CompressFormat::Zip,
        &names(&["a", "-b"]),
    );
    assert_eq!(
        zip,
        "cd '/d' && exec zip -r -q -y '/d/.t.zssh-part' -- 'a' '-b'"
    );
}

#[test]
fn plan_command_asks_gnu_tar_for_literal_names_and_unzip_for_plain_ones() {
    let tar = plan_command(ArchiveFormat::TarGz, "/d/a b.tar.gz", "/d/a b", true);
    assert!(
        tar.contains("tar --quoting-style=literal -tzf '/d/a b.tar.gz'"),
        "{tar}"
    );
    assert!(tar.contains("cd '/d/a b'"), "{tar}");
    assert!(tar.contains("</dev/null"), "{tar}");
    let plain = plan_command(ArchiveFormat::TarXz, "/d/a.txz", "/d", false);
    assert!(plain.contains("tar -tJf '/d/a.txz'"), "{plain}");
    assert!(!plain.contains("quoting-style"), "{plain}");
    let zip = plan_command(ArchiveFormat::Zip, "/d/a.zip", "/d", true);
    assert!(zip.contains("unzip -Z1 '/d/a.zip'"), "{zip}");
    assert!(!zip.contains("quoting-style"), "{zip}");
}

#[test]
fn plan_command_never_falls_back_to_a_predictable_temp_name() {
    for format in [ArchiveFormat::TarGz, ArchiveFormat::Zip] {
        let script = plan_command(format, "/d/a.tar.gz", "/d", true);
        assert!(
            script.starts_with("T=$(mktemp 2>/dev/null) || {"),
            "{script}"
        );
        assert!(!script.contains("/tmp/"), "{script}");
        assert!(!script.contains("$$"), "{script}");
    }
}

#[test]
fn plan_command_checks_every_existing_parent_for_symlinks() {
    let tar = plan_command(ArchiveFormat::TarGz, "/d/a.tar.gz", "/d/out dir", true);
    // 逐级走路径：每一级 `[ -L ]`，命中就标成 l
    assert!(tar.contains("[ -L \"$pre\" ]"), "{tar}");
    assert!(tar.contains("k=l"), "{tar}");
    // 先进目标目录，没有这个目录就不查
    assert!(tar.contains("cd '/d/out dir' 2>/dev/null"), "{tar}");
    // tar 的条目原样读，zip 的才换反斜杠
    assert!(tar.contains("cat \"$T\" |"), "{tar}");
    assert!(!tar.contains("tr '"), "{tar}");
    let zip = plan_command(ArchiveFormat::Zip, "/d/a.zip", "/d/out dir", true);
    assert!(zip.contains("tr '\\\\' '/' <\"$T\" |"), "{zip}");
    assert!(zip.contains("[ -L \"$pre\" ]"), "{zip}");
    // 条目只经过 read -r 当数据用，命令里不会出现条目内容之外的拼接
    assert!(zip.contains("IFS= read -r p"), "{zip}");
    assert!(!zip.contains("@LIST@") && !zip.contains("@FEED@"), "{zip}");
}

/// `needle` 在脚本里的每一处是不是都在单引号里面（单引号里 shell 什么都不展开）。
/// 一处都没有也算不通过：名字应该原样出现在命令里。
fn always_inside_single_quotes(script: &str, needle: &str) -> bool {
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut found = false;
    for (index, ch) in script.char_indices() {
        if script[index..].starts_with(needle) {
            if !single {
                return false;
            }
            found = true;
        }
        if single {
            single = ch != '\'';
        } else if escaped {
            escaped = false;
        } else {
            match ch {
                '\\' => escaped = true,
                '"' => double = !double,
                '\'' if !double => single = true,
                _ => {}
            }
        }
    }
    found
}

/// 2026-11-11 审核：文件名里有字面量占位符时，逐个替换会把引号提前闭合，名字里的 `$(…)` 落到
/// 引号外被服务器执行。填进去的名字不能再被当成模板。
#[test]
fn placeholder_text_in_file_names_stays_quoted() {
    const PAYLOAD: &str = "$(touch marker)";
    for format in [ArchiveFormat::TarGz, ArchiveFormat::Zip] {
        for token in [
            "@LIST@", "@MARK@", "@DEST@", "@FEED@", "@CHECK@", "@AWK@", "@", "@@",
        ] {
            let stem = format!("x{token}{PAYLOAD}y{token}");
            let archive = format!("/srv/up/{stem}.pkg");
            // 「解压到同名文件夹」：目标目录就是压缩包的主名。
            let dest = format!("/srv/up/{stem}");

            let plan = plan_command(format, &archive, &dest, true);
            assert!(
                always_inside_single_quotes(&plan, PAYLOAD),
                "{token}：{plan}"
            );
            assert!(plan.contains(&shell_quote(&archive)), "{token}：{plan}");
            assert!(
                plan.contains(&format!("cd {} 2>/dev/null", shell_quote(&dest))),
                "{token}：{plan}"
            );

            let validate = validate_list_command(format, &archive);
            assert!(
                always_inside_single_quotes(&validate, PAYLOAD),
                "{token}：{validate}"
            );
            assert!(
                validate.contains(&shell_quote(&archive)),
                "{token}：{validate}"
            );
        }
    }
}

/// 名字里没有占位符时，脚本里不留任何没填的占位符，结构和以前一样。
#[test]
fn templates_are_filled_completely() {
    for format in [ArchiveFormat::TarGz, ArchiveFormat::Zip] {
        let plan = plan_command(format, "/d/a.pkg", "/d/out", true);
        let validate = validate_list_command(format, "/d/a.pkg");
        for script in [&plan, &validate] {
            assert!(!script.contains('@'), "{script}");
        }
        assert!(
            plan.contains(&format!("echo \"{LIST_EXIT_MARK}$rc\" >&2")),
            "{plan}"
        );
        assert!(validate.contains("| awk '{") || validate.contains("awk '{"));
    }
    assert_eq!(fill_template("a@X@b@", &[("@X@", "@Y@")]), "a@Y@b@");
    assert_eq!(
        fill_template("@A@@B@", &[("@A@", "@B@"), ("@B@", "2")]),
        "@B@2",
        "填进去的值不再被替换"
    );
    assert_eq!(fill_template("中@文@Z@", &[("@Z@", "值")]), "中@文值");
}

#[test]
fn extract_commands_carry_the_right_flags_per_format() {
    let ex = |format, mode, skip| extract_command(format, "/d/a.x", "/d/out", false, mode, skip);
    assert_eq!(
        ex(ArchiveFormat::TarGz, TarMode::Overwrite, false),
        "cd '/d/out' && exec tar -xzf '/d/a.x'"
    );
    assert_eq!(
        ex(ArchiveFormat::Tar, TarMode::Overwrite, false),
        "cd '/d/out' && exec tar -xf '/d/a.x'"
    );
    assert_eq!(
        ex(ArchiveFormat::TarXz, TarMode::SkipOldFiles, true),
        "cd '/d/out' && exec tar --skip-old-files -xJf '/d/a.x'"
    );
    assert_eq!(
        ex(ArchiveFormat::TarBz2, TarMode::KeepOld, true),
        "cd '/d/out' && exec tar -k -xjf '/d/a.x'"
    );
    assert_eq!(
        ex(ArchiveFormat::Zip, TarMode::Overwrite, false),
        "exec unzip -q -o '/d/a.x' -d '/d/out'"
    );
    assert_eq!(
        ex(ArchiveFormat::Zip, TarMode::Overwrite, true),
        "exec unzip -q -n '/d/a.x' -d '/d/out'"
    );
    assert_eq!(
        extract_command(
            ArchiveFormat::TarGz,
            "/d/a.x",
            "/d/.o.zssh-part",
            true,
            TarMode::Overwrite,
            false
        ),
        "mkdir '/d/.o.zssh-part' && cd '/d/.o.zssh-part' && exec tar -xzf '/d/a.x'"
    );
}

#[test]
fn validate_command_relists_before_extracting_without_a_predictable_temp_file() {
    let tar = validate_list_command(ArchiveFormat::TarGz, "/d/a b.tar.gz");
    assert!(tar.contains("mktemp"), "{tar}");
    assert!(!tar.contains("/tmp/"), "不能退到固定临时名：{tar}");
    assert!(tar.contains("tar -tzf '/d/a b.tar.gz'"), "{tar}");
    assert!(tar.contains("awk"), "{tar}");

    let zip = validate_list_command(ArchiveFormat::Zip, "/d/a.zip");
    assert!(zip.contains("unzip -Z1 '/d/a.zip'"), "{zip}");
    // zip 的条目先把反斜杠换成斜杠，再查 `..`，和 `entry_escapes_dir` 的规则一致。
    assert!(zip.contains(r"tr '\\' '/'"), "{zip}");
}

#[test]
fn validate_command_rejects_the_same_unsafe_paths_as_the_plan() {
    for format in [ArchiveFormat::TarGz, ArchiveFormat::Zip] {
        let script = validate_list_command(format, "/d/a");
        assert!(script.contains(r"/^\//"), "要拒绝绝对路径：{script}");
        assert!(
            script.contains(r"(^|\/)\.\.(\/|$)"),
            "要拒绝 .. 段：{script}"
        );
        // 校验失败要报明确原因、退出码非 0（extract 会按解压失败处理）。
        assert!(script.contains("已拒绝解压"), "{script}");
    }
}

// ---------------------------------------------------------------------------
// 安全检查
// ---------------------------------------------------------------------------

#[test]
fn rejects_absolute_and_parent_paths() {
    for bad in ["/etc/passwd", "../x", "a/../../x", "a/..", "/", "./../x"] {
        assert!(entry_escapes_dir(bad, false), "{bad}");
    }
    for fine in [
        "a/b.txt",
        "./a",
        "a/",
        "..a",
        "a..b/c",
        "a/.../b",
        "-x",
        "中文/文件.txt",
    ] {
        assert!(!entry_escapes_dir(fine, false), "{fine}");
    }
}

#[test]
fn zip_entries_also_treat_backslash_and_drive_letters_as_unsafe() {
    for bad in ["..\\evil", "a\\..\\..\\x", "\\abs", "C:\\x", "c:/x", "/x"] {
        assert!(entry_escapes_dir(bad, true), "{bad}");
    }
    assert!(!entry_escapes_dir("a\\b.txt", true));
    // tar 里反斜杠只是普通字符
    assert!(!entry_escapes_dir("a\\..\\b", false));
}

#[test]
fn decodes_gnu_tar_escapes() {
    assert_eq!(decode_tar_escapes("plain"), "plain");
    assert_eq!(decode_tar_escapes("\\344\\270\\255.txt"), "中.txt");
    assert_eq!(decode_tar_escapes("a\\nb\\tc\\\\d"), "a\nb\tc\\d");
    assert_eq!(decode_tar_escapes("end\\"), "end\\");
    assert_eq!(decode_tar_escapes("\\q"), "\\q");
}

#[test]
fn scan_counts_files_conflicts_and_keeps_a_short_sample() {
    let stdout = "dsite/\nnsite/a.txt\nxsite/b.txt\nxsite/c.txt\nnsite/d.txt\nxsite/e.txt\nxsite/f.txt\nxsite/g.txt\nxsite/h.txt\n";
    let scan = scan_plan_output(stdout, false).unwrap();
    assert_eq!(scan.total_entries, 9);
    assert_eq!(scan.total_files, 8);
    assert_eq!(scan.conflict_count, 6);
    assert_eq!(
        scan.conflict_sample,
        [
            "site/b.txt",
            "site/c.txt",
            "site/e.txt",
            "site/f.txt",
            "site/g.txt"
        ]
    );
}

#[test]
fn scan_refuses_the_whole_archive_when_any_entry_escapes() {
    for bad in [
        "n/etc/passwd\n",
        "nok\nn../evil\n",
        "da/../../b/\n",
        "nok/..\n",
    ] {
        assert_eq!(
            scan_plan_output(bad, false),
            Err(ArchiveError::UnsafeEntries),
            "{bad:?}"
        );
    }
    // 转义后才露出来的绝对路径 / .. 也要认出来（宁可多拒绝，不放过）
    assert_eq!(
        scan_plan_output("n\\057etc\\057passwd\n", false),
        Err(ArchiveError::UnsafeEntries),
        "转义后是 /etc/passwd"
    );
    assert_eq!(
        scan_plan_output("n\\056\\056/x\n", false),
        Err(ArchiveError::UnsafeEntries),
        "转义后是 ../x"
    );
    assert_eq!(
        scan_plan_output("n..\\evil\n", true),
        Err(ArchiveError::UnsafeEntries),
        "zip 的反斜杠分隔"
    );
}

#[test]
fn scan_refuses_the_whole_archive_when_a_destination_symlink_is_in_the_way() {
    // l：条目的某一级父目录（或它自己）在目标里已经是符号链接
    let stdout = "dsite/\nnsite/a.txt\nllink/a.txt\nllink2/b.txt\nxsite/c.txt\n";
    let err = scan_plan_output(stdout, false).unwrap_err();
    assert_eq!(err, ArchiveError::DestSymlink("link/a.txt".to_owned()));
    assert_eq!(
        err.to_string(),
        "目标目录里有符号链接（link/a.txt），解压可能写到目录外，已拒绝"
    );
    // zip 一样
    assert_eq!(
        scan_plan_output("llink/a.txt\n", true),
        Err(ArchiveError::DestSymlink("link/a.txt".to_owned()))
    );
    // 路径越界优先报（包本身就不对）
    assert_eq!(
        scan_plan_output("llink/a.txt\nn../evil\n", false),
        Err(ArchiveError::UnsafeEntries)
    );
}

#[test]
fn zip_backslash_entries_are_normalised_so_conflicts_are_not_missed() {
    // 脚本已经把 `\` 换成 `/` 再核对，所以 Windows 打的包里的 dir\file.txt 能被报成冲突；
    // 这里再验证解析一侧：就算输出里还留着反斜杠，zip 也按 `/` 处理和显示。
    let scan = scan_plan_output("ddir\\\nxdir\\file.txt\nndir\\other.txt\n", true).unwrap();
    assert_eq!(scan.total_entries, 3);
    assert_eq!(scan.conflict_count, 1);
    assert_eq!(scan.conflict_sample, ["dir/file.txt"]);
    // 反斜杠里的链接也认得出
    assert_eq!(
        scan_plan_output("ldir\\file.txt\n", true),
        Err(ArchiveError::DestSymlink("dir/file.txt".to_owned()))
    );
    // tar 里反斜杠只是普通字符，原样保留
    let tar = scan_plan_output("xdir\\file.txt\n", false).unwrap();
    assert_eq!(tar.conflict_sample, ["dir\\file.txt"]);
}

#[test]
fn scan_ignores_blank_lines_and_empty_archives() {
    let scan = scan_plan_output("\n\n", false).unwrap();
    assert_eq!(scan, PlanScan::default());
}

// ---------------------------------------------------------------------------
// 退出码判断
// ---------------------------------------------------------------------------

#[test]
fn keep_old_distinguishes_only_skipped_from_real_failures() {
    assert_eq!(classify_keep_old(Some(0), ""), Ok(0));
    let only_skips = "tar: a.txt: Cannot open: File exists\n\
                      tar: dir/b.txt: Cannot open: File exists\n\
                      tar: Exiting with failure status due to previous errors\n";
    assert_eq!(classify_keep_old(Some(2), only_skips), Ok(2));
    let mixed = "tar: a.txt: Cannot open: File exists\n\
                 tar: b.txt: Cannot write: No space left on device\n\
                 tar: Exiting with failure status due to previous errors\n";
    let err = classify_keep_old(Some(2), mixed).unwrap_err();
    assert!(err.contains("No space left on device"), "{err}");
    let none = classify_keep_old(
        Some(2),
        "tar: Exiting with failure status due to previous errors\n",
    );
    assert!(none.is_err(), "没有任何已存在提示的非 0 是真失败");
    assert!(classify_keep_old(None, "tar: a: Cannot open: File exists").is_err());
}

#[test]
fn compress_exit_codes() {
    assert_eq!(
        classify_compress(CompressFormat::TarGz, Some(0), ""),
        Ok(None)
    );
    let changed = "tar: a.log: file changed as we read it\n";
    let warning = classify_compress(CompressFormat::TarGz, Some(1), changed).unwrap();
    assert!(warning.is_some());
    let mixed = "tar: a.log: file changed as we read it\ntar: b: Cannot open: Permission denied\n";
    assert!(classify_compress(CompressFormat::TarGz, Some(1), mixed).is_err());
    assert!(classify_compress(CompressFormat::TarGz, Some(1), "").is_err());
    assert!(classify_compress(CompressFormat::Zip, Some(1), changed).is_err());
    let missing =
        classify_compress(CompressFormat::Zip, Some(127), "sh: zip: not found").unwrap_err();
    assert!(missing.contains("zip"), "{missing}");
    let full = classify_compress(
        CompressFormat::TarGz,
        Some(2),
        "tar: x: Wrote only 512 of 10240 bytes\ntar: No space left on device",
    )
    .unwrap_err();
    assert!(full.contains("No space left on device"), "{full}");
    assert!(classify_compress(CompressFormat::TarGz, None, "").is_err());
}

#[test]
fn stderr_tail_keeps_the_last_lines_and_cuts_long_text() {
    let many: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    let tail = stderr_tail(&many);
    assert!(
        tail.contains("line 10") && tail.contains("line 5") && !tail.contains("line 4"),
        "{tail}"
    );
    let long = "x".repeat(5000);
    assert!(stderr_tail(&long).chars().count() <= STDERR_CHARS + 1);
    assert_eq!(stderr_tail(&format!("{LIST_EXIT_MARK}0\n")), "");
}

#[test]
fn unsupported_option_detection() {
    assert!(is_unsupported_option(
        "tar: unrecognized option '--skip-old-files'\nTry 'tar --help'"
    ));
    assert!(is_unsupported_option("tar: invalid option -- 'k'"));
    assert!(is_unsupported_option(
        "tar: unknown option --quoting-style=literal"
    ));
    assert!(!is_unsupported_option(
        "tar: a: Cannot open: Permission denied"
    ));
}

// ---------------------------------------------------------------------------
// 能力探测
// ---------------------------------------------------------------------------

#[test]
fn caps_probe_command_asks_for_shell_and_the_three_tools_in_one_exec() {
    let command = build_caps_probe_command();
    assert!(command.starts_with("exec sh -c '"), "{command}");
    assert!(command.contains(SHELL_PROBE_MARKER));
    assert!(command.contains("for c in tar zip unzip"));
}

#[test]
fn parses_caps_probe_output() {
    let out = format!("{SHELL_PROBE_MARKER}/bin/bash\n{HAS_CMD_MARK}tar\n{HAS_CMD_MARK}unzip\n");
    assert_eq!(
        parse_caps_probe(&out),
        ArchiveCaps {
            posix: true,
            tar: true,
            zip: false,
            unzip: true
        }
    );
    let windows = format!("{SHELL_PROBE_MARKER}C:\\Windows\\System32\\cmd.exe\r\n");
    assert_eq!(parse_caps_probe(&windows), ArchiveCaps::default());
    let fish = format!("{SHELL_PROBE_MARKER}/usr/bin/fish\n{HAS_CMD_MARK}tar\n");
    let caps = parse_caps_probe(&fish);
    assert!(!caps.posix, "fish 不是 POSIX，不开放");
    assert_eq!(parse_caps_probe(""), ArchiveCaps::default());
}

// ---------------------------------------------------------------------------
// 压缩流程
// ---------------------------------------------------------------------------

fn compress_request<'a>(
    names: &'a [String],
    server: &'a str,
    format: CompressFormat,
) -> CompressRequest<'a> {
    CompressRequest {
        dir: "/d",
        names,
        format,
        owner: "t1",
        server,
    }
}

#[tokio::test]
async fn compress_writes_a_temp_file_then_renames_it_to_the_final_name() {
    let fs = std::sync::Arc::new(FakeFs::default());
    fs.put_dir("/d");
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("tar -czf") {
            fs_for_runner.put_file(&quoted_after(command, "tar -czf "), 2048);
        }
        Ok(ok(""))
    });
    let list = names(&["logs", "a b.txt"]);
    let outcome = compress(
        &runner,
        fs.as_ref(),
        &compress_request(&list, "srv-c1", CompressFormat::TarGz),
        &never(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome.name, "d.tar.gz", "选多个用当前目录名");
    assert_eq!(outcome.path, "/d/d.tar.gz");
    assert!(fs.has("/d/d.tar.gz"));
    assert!(
        !fs.has("/d/.d.tar.gz.t1.zssh-part"),
        "临时文件改名后不应该还在"
    );
    let commands = runner.commands();
    assert_eq!(commands.len(), 2, "清理旧临时文件 + 压缩：{commands:?}");
    assert!(commands[1]
        .contains("cd '/d' && exec tar -czf '/d/.d.tar.gz.t1.zssh-part' -- 'logs' 'a b.txt'"));
    assert!(
        commands[1].contains("read -r _ <&3; kill -TERM $pid"),
        "要由看守盯着：{}",
        commands[1]
    );
}

#[tokio::test]
async fn compress_never_overwrites_an_existing_archive() {
    let fs = std::sync::Arc::new(FakeFs::default());
    fs.put_dir("/d");
    fs.put_file("/d/logs.tar.gz", 5);
    fs.put_file("/d/logs (1).tar.gz", 5);
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("tar -czf") {
            fs_for_runner.put_file(&quoted_after(command, "tar -czf "), 100);
        }
        Ok(ok(""))
    });
    let list = names(&["logs"]);
    let outcome = compress(
        &runner,
        fs.as_ref(),
        &compress_request(&list, "srv-c2", CompressFormat::TarGz),
        &never(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome.name, "logs (2).tar.gz");
    assert_eq!(
        fs.nodes.lock().unwrap().get("/d/logs.tar.gz").unwrap().size,
        Some(5),
        "原来的包不动"
    );
}

#[tokio::test]
async fn compress_failure_removes_only_its_own_temp_file_and_reports_stderr() {
    let fs = std::sync::Arc::new(FakeFs::default());
    fs.put_dir("/d");
    fs.put_file("/d/keep.tar.gz", 5);
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("tar -czf") {
            // 命令写了一半就失败
            fs_for_runner.put_file(&quoted_after(command, "tar -czf "), 10);
            return Ok(fail(
                2,
                "tar: x: Wrote only 512 of 10240 bytes\ntar: No space left on device\n",
            ));
        }
        // rm -rf：模拟 shell 真的删掉
        if command.starts_with("rm -rf -- ") {
            fs_for_runner.remove_all_blocking(&quoted_after(command, "rm -rf -- "));
        }
        Ok(ok(""))
    });
    let list = names(&["keep"]);
    let err = compress(
        &runner,
        fs.as_ref(),
        &compress_request(&list, "srv-c3", CompressFormat::TarGz),
        &never(),
        None,
    )
    .await
    .unwrap_err();
    let ArchiveError::Failed(message) = err else {
        panic!("应该是失败");
    };
    assert!(message.contains("No space left on device"), "{message}");
    assert!(!fs.has("/d/.keep (1).tar.gz.t1.zssh-part"));
    assert!(fs.has("/d/keep.tar.gz"), "别人的文件不动");
    let commands = runner.commands();
    assert!(
        commands
            .last()
            .unwrap()
            .starts_with("rm -rf -- '/d/.keep (1).tar.gz.t1.zssh-part'"),
        "{commands:?}"
    );
    assert!(fs
        .nodes
        .lock()
        .unwrap()
        .keys()
        .all(|key| !key.ends_with(".zssh-part")));
}

impl FakeFs {
    fn remove_all_blocking(&self, path: &str) {
        self.nodes
            .lock()
            .unwrap()
            .retain(|key, _| key != path && !key.starts_with(&format!("{path}/")));
    }
}

#[tokio::test]
async fn compress_cancel_cleans_up_and_reports_cancelled() {
    let fs = std::sync::Arc::new(FakeFs::default());
    fs.put_dir("/d");
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("zip -r -q -y") {
            fs_for_runner.put_file(&quoted_after(command, "zip -r -q -y "), 10);
            return Ok(ExecOutput {
                cancelled: true,
                ..ExecOutput::default()
            });
        }
        if command.starts_with("rm -rf -- ") {
            fs_for_runner.remove_all_blocking(&quoted_after(command, "rm -rf -- "));
        }
        Ok(ok(""))
    });
    let list = names(&["x"]);
    let err = compress(
        &runner,
        fs.as_ref(),
        &compress_request(&list, "srv-c4", CompressFormat::Zip),
        &never(),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err, ArchiveError::Cancelled);
    assert!(fs
        .nodes
        .lock()
        .unwrap()
        .keys()
        .all(|key| !key.ends_with(".zssh-part")));
    assert!(!fs.has("/d/x.zip"));
}

#[tokio::test]
async fn compress_refuses_bad_names_before_running_anything() {
    let fs = FakeFs::default();
    fs.put_dir("/d");
    let runner = FakeRunner::new(|_| panic!("不该执行任何命令"));
    for bad in [
        names(&[]),
        names(&["a/b"]),
        names(&[".."]),
        names(&[""]),
        names(&["ok", "."]),
    ] {
        let err = compress(
            &runner,
            &fs,
            &compress_request(&bad, "srv-c5", CompressFormat::TarGz),
            &never(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ArchiveError::Unsupported(_)),
            "{bad:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn compress_tar_exit_one_for_a_changing_file_is_a_warning_not_a_failure() {
    let fs = std::sync::Arc::new(FakeFs::default());
    fs.put_dir("/d");
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("tar -czf") {
            fs_for_runner.put_file(&quoted_after(command, "tar -czf "), 100);
            return Ok(fail(1, "tar: app.log: file changed as we read it\n"));
        }
        Ok(ok(""))
    });
    let list = names(&["app.log"]);
    let outcome = compress(
        &runner,
        fs.as_ref(),
        &compress_request(&list, "srv-c6", CompressFormat::TarGz),
        &never(),
        None,
    )
    .await
    .unwrap();
    assert!(outcome.warning.is_some());
    assert!(fs.has("/d/app.log.tar.gz"));
}

// ---------------------------------------------------------------------------
// 解压流程
// ---------------------------------------------------------------------------

fn plan_runner(stdout: &'static str) -> FakeRunner {
    FakeRunner::new(move |_| {
        Ok(ExecOutput {
            exit_code: Some(0),
            stdout: stdout.to_owned(),
            stderr: format!("{LIST_EXIT_MARK}0\n"),
            ..ExecOutput::default()
        })
    })
}

fn archive_fs() -> FakeFs {
    let fs = FakeFs::default();
    fs.put_dir("/d");
    fs.put_file("/d/site.tar.gz", 4096);
    fs
}

#[tokio::test]
async fn plan_reports_conflicts_for_an_existing_destination() {
    let fs = archive_fs();
    fs.put_dir("/d/site");
    let runner = plan_runner("dsite/\nxsite/a.txt\nnsite/b.txt\n");
    let plan = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap();
    assert!(plan.dest_exists);
    assert_eq!(plan.total_files, 2);
    assert_eq!(plan.conflict_count, 1);
    assert_eq!(plan.conflict_sample, ["site/a.txt"]);
    assert_eq!(plan.fingerprint.size, Some(4096));
    assert_eq!(runner.commands().len(), 1);
    assert!(runner.commands()[0].contains("--quoting-style=literal"));
}

#[tokio::test]
async fn plan_for_a_new_folder_has_no_conflicts_to_ask_about() {
    let fs = archive_fs();
    let runner = plan_runner("nsite/a.txt\n");
    let plan = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap();
    assert!(!plan.dest_exists);
    assert_eq!(plan.conflict_count, 0);
}

#[tokio::test]
async fn plan_refuses_archives_that_escape_the_destination() {
    let fs = archive_fs();
    let runner = plan_runner("nok.txt\nn../../etc/cron.d/evil\n");
    let err = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap_err();
    assert_eq!(err, ArchiveError::UnsafeEntries);
    assert_eq!(err.to_string(), "压缩包里有指向目录外的路径，已拒绝解压");
}

#[tokio::test]
async fn plan_refuses_archives_when_the_destination_has_a_symlink_in_the_way() {
    let fs = archive_fs();
    fs.put_dir("/d/site");
    let runner = plan_runner("dsite/\nnsite/ok.txt\nllink/a.txt\n");
    let err = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap_err();
    assert_eq!(err, ArchiveError::DestSymlink("link/a.txt".to_owned()));
    assert!(err.to_string().contains("符号链接（link/a.txt）"));
}

#[tokio::test]
async fn plan_retries_without_the_gnu_only_option_on_busybox() {
    let fs = archive_fs();
    let runner = FakeRunner::new(|command| {
        if command.contains("--quoting-style") {
            Ok(ExecOutput {
                exit_code: Some(0),
                stderr: format!(
                    "tar: unrecognized option '--quoting-style=literal'\n{LIST_EXIT_MARK}1\n"
                ),
                ..ExecOutput::default()
            })
        } else {
            Ok(ExecOutput {
                exit_code: Some(0),
                stdout: "nsite/a.txt\n".to_owned(),
                stderr: format!("{LIST_EXIT_MARK}0\n"),
                ..ExecOutput::default()
            })
        }
    });
    let plan = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap();
    assert_eq!(plan.total_files, 1);
    let commands = runner.commands();
    assert_eq!(commands.len(), 2);
    assert!(!commands[1].contains("--quoting-style"));
}

#[tokio::test]
async fn plan_reports_a_corrupt_archive_with_the_tail_of_stderr() {
    let fs = archive_fs();
    let runner = FakeRunner::new(|_| {
        Ok(ExecOutput {
            exit_code: Some(0),
            stderr: format!("gzip: stdin: not in gzip format\ntar: Child returned status 1\n{LIST_EXIT_MARK}2\n"),
            ..ExecOutput::default()
        })
    });
    let err = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap_err();
    let ArchiveError::Failed(message) = err else {
        panic!("应该是失败");
    };
    assert!(message.contains("not in gzip format"), "{message}");
}

#[tokio::test]
async fn plan_refuses_a_truncated_listing() {
    let fs = archive_fs();
    let runner = FakeRunner::new(|_| {
        Ok(ExecOutput {
            exit_code: Some(0),
            stdout: "nsite/a\n".to_owned(),
            stderr: format!("{LIST_EXIT_MARK}0\n"),
            stdout_truncated: true,
            ..ExecOutput::default()
        })
    });
    let err = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap_err();
    assert!(matches!(err, ArchiveError::Failed(_)));
}

#[tokio::test]
async fn plan_rejects_unknown_formats_and_a_file_in_the_way() {
    let fs = archive_fs();
    fs.put_file("/d/readme.txt", 1);
    fs.put_file("/d/site", 1);
    let runner = FakeRunner::new(|_| panic!("不该执行任何命令"));
    let err = plan_extract(&runner, &fs, "/d/readme.txt", "/d/readme", &never())
        .await
        .unwrap_err();
    assert!(matches!(err, ArchiveError::Unsupported(_)));
    let err = plan_extract(&runner, &fs, "/d/site.tar.gz", "/d/site", &never())
        .await
        .unwrap_err();
    let ArchiveError::Failed(message) = err else {
        panic!("应该是失败");
    };
    assert!(message.contains("是个文件"), "{message}");
}

fn extract_request<'a>(
    policy: ConflictPolicy,
    server: &'a str,
    dest_exists: bool,
    conflicts: u64,
) -> ExtractRequest<'a> {
    ExtractRequest {
        archive: "/d/site.tar.gz",
        dest: "/d/site",
        policy,
        plan_fingerprint: ArchiveFingerprint {
            size: Some(4096),
            mtime: Some(1_700_000_000),
        },
        plan_dest_exists: dest_exists,
        plan_total_files: 10,
        plan_conflict_count: conflicts,
        owner: "t1",
        server,
    }
}

#[tokio::test]
async fn extracting_into_a_new_folder_goes_through_a_temp_folder_and_a_rename() {
    let fs = std::sync::Arc::new(archive_fs());
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("mkdir ") {
            fs_for_runner.put_dir(&quoted_after(command, "mkdir "));
        }
        Ok(ok(""))
    });
    let outcome = extract(
        &runner,
        fs.as_ref(),
        &extract_request(ConflictPolicy::Overwrite, "srv-e1", false, 0),
        &never(),
    )
    .await
    .unwrap();
    assert_eq!((outcome.extracted, outcome.skipped), (10, 0));
    assert!(fs.has("/d/site"));
    assert!(!fs.has("/d/.site.t1.zssh-part"));
    let commands = runner.commands();
    assert!(commands.last().unwrap().contains("mkdir '/d/.site.t1.zssh-part' && cd '/d/.site.t1.zssh-part' && exec tar -xzf '/d/site.tar.gz'"));
}

#[tokio::test]
async fn a_failed_extract_into_a_new_folder_leaves_no_folder_behind() {
    let fs = std::sync::Arc::new(archive_fs());
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("mkdir ") {
            fs_for_runner.put_dir(&quoted_after(command, "mkdir "));
            return Ok(fail(2, "tar: x: Cannot write: No space left on device\n"));
        }
        if command.starts_with("rm -rf -- ") {
            fs_for_runner.remove_all_blocking(&quoted_after(command, "rm -rf -- "));
        }
        Ok(ok(""))
    });
    let err = extract(
        &runner,
        fs.as_ref(),
        &extract_request(ConflictPolicy::Overwrite, "srv-e2", false, 0),
        &never(),
    )
    .await
    .unwrap_err();
    let ArchiveError::Failed(message) = err else {
        panic!("应该是失败");
    };
    assert!(message.contains("No space left on device"), "{message}");
    assert!(!fs.has("/d/site") && !fs.has("/d/.site.t1.zssh-part"));
}

#[tokio::test]
async fn cancelling_an_extract_into_a_new_folder_removes_the_temp_folder() {
    let fs = std::sync::Arc::new(archive_fs());
    let fs_for_runner = std::sync::Arc::clone(&fs);
    let runner = FakeRunner::new(move |command| {
        if command.contains("mkdir ") {
            fs_for_runner.put_dir(&quoted_after(command, "mkdir "));
            return Ok(ExecOutput {
                cancelled: true,
                ..ExecOutput::default()
            });
        }
        if command.starts_with("rm -rf -- ") {
            fs_for_runner.remove_all_blocking(&quoted_after(command, "rm -rf -- "));
        }
        Ok(ok(""))
    });
    let err = extract(
        &runner,
        fs.as_ref(),
        &extract_request(ConflictPolicy::Overwrite, "srv-e3", false, 0),
        &never(),
    )
    .await
    .unwrap_err();
    assert_eq!(err, ArchiveError::Cancelled);
    assert!(!fs.has("/d/site") && !fs.has("/d/.site.t1.zssh-part"));
}

#[tokio::test]
async fn overwrite_into_an_existing_folder_extracts_in_place() {
    let fs = archive_fs();
    fs.put_dir("/d/site");
    let runner = FakeRunner::new(|_| Ok(ok("")));
    let outcome = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::Overwrite, "srv-e4", true, 3),
        &never(),
    )
    .await
    .unwrap();
    assert_eq!((outcome.extracted, outcome.skipped), (10, 0));
    assert_eq!(
        runner.commands(),
        [with_recheck(
            ArchiveFormat::TarGz,
            "/d/site.tar.gz",
            "cd '/d/site' && exec tar -xzf '/d/site.tar.gz'"
        )]
    );
    assert!(fs.renames.lock().unwrap().is_empty(), "就地解压不改名");
}

#[tokio::test]
async fn skip_uses_skip_old_files_and_counts_the_planned_conflicts() {
    let fs = archive_fs();
    fs.put_dir("/d/site");
    let runner = FakeRunner::new(|_| Ok(ok("")));
    let outcome = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::SkipExisting, "srv-e5", true, 3),
        &never(),
    )
    .await
    .unwrap();
    assert_eq!((outcome.extracted, outcome.skipped), (7, 3));
    assert_eq!(
        runner.commands(),
        [with_recheck(
            ArchiveFormat::TarGz,
            "/d/site.tar.gz",
            "cd '/d/site' && exec tar --skip-old-files -xzf '/d/site.tar.gz'"
        )]
    );
}

#[tokio::test]
async fn skip_falls_back_to_k_and_tells_skipped_apart_from_real_errors() {
    // 只是跳过：-k 退出码 2，stderr 全是 File exists
    let fs = archive_fs();
    fs.put_dir("/d/site");
    let runner = FakeRunner::new(|command| {
        if command.contains("--skip-old-files") {
            Ok(fail(2, "tar: unrecognized option '--skip-old-files'\n"))
        } else {
            Ok(fail(
                2,
                "tar: site/a.txt: Cannot open: File exists\n\
                 tar: site/b.txt: Cannot open: File exists\n\
                 tar: Exiting with failure status due to previous errors\n",
            ))
        }
    });
    let outcome = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::SkipExisting, "srv-e6", true, 5),
        &never(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.skipped, 2, "以 -k 的 stderr 实际数出来的为准");
    assert_eq!(outcome.extracted, 8);
    let commands = runner.commands();
    assert_eq!(commands.len(), 2);
    assert!(commands[1].contains("exec tar -k -xzf"), "{commands:?}");

    // 夹着真错误：磁盘满
    let fs = archive_fs();
    fs.put_dir("/d/site");
    let runner = FakeRunner::new(|command| {
        if command.contains("--skip-old-files") {
            Ok(fail(2, "tar: unrecognized option '--skip-old-files'\n"))
        } else {
            Ok(fail(
                2,
                "tar: site/a.txt: Cannot open: File exists\n\
                 tar: site/c.txt: Cannot write: No space left on device\n\
                 tar: Exiting with failure status due to previous errors\n",
            ))
        }
    });
    let err = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::SkipExisting, "srv-e7", true, 5),
        &never(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("No space left on device"), "{err}");
}

#[tokio::test]
async fn skip_for_zip_uses_unzip_n_and_overwrite_uses_unzip_o() {
    let fs = FakeFs::default();
    fs.put_dir("/d");
    fs.put_dir("/d/pack");
    fs.put_file("/d/pack.zip", 100);
    let request = |policy| ExtractRequest {
        archive: "/d/pack.zip",
        dest: "/d/pack",
        policy,
        plan_fingerprint: ArchiveFingerprint {
            size: Some(100),
            mtime: Some(1_700_000_000),
        },
        plan_dest_exists: true,
        plan_total_files: 4,
        plan_conflict_count: 1,
        owner: "t1",
        server: "srv-e8",
    };
    let runner = FakeRunner::new(|_| Ok(ok("")));
    let skipped = extract(
        &runner,
        &fs,
        &request(ConflictPolicy::SkipExisting),
        &never(),
    )
    .await
    .unwrap();
    assert_eq!((skipped.extracted, skipped.skipped), (3, 1));
    let overwritten = extract(&runner, &fs, &request(ConflictPolicy::Overwrite), &never())
        .await
        .unwrap();
    assert_eq!((overwritten.extracted, overwritten.skipped), (4, 0));
    assert_eq!(
        runner.commands(),
        [
            with_recheck(
                ArchiveFormat::Zip,
                "/d/pack.zip",
                "exec unzip -q -n '/d/pack.zip' -d '/d/pack'"
            ),
            with_recheck(
                ArchiveFormat::Zip,
                "/d/pack.zip",
                "exec unzip -q -o '/d/pack.zip' -d '/d/pack'"
            )
        ]
    );
}

#[tokio::test]
async fn unzip_exit_one_is_a_warning_and_other_codes_are_failures() {
    let fs = FakeFs::default();
    fs.put_dir("/d");
    fs.put_dir("/d/pack");
    fs.put_file("/d/pack.zip", 100);
    let request = ExtractRequest {
        archive: "/d/pack.zip",
        dest: "/d/pack",
        policy: ConflictPolicy::Overwrite,
        plan_fingerprint: ArchiveFingerprint {
            size: Some(100),
            mtime: Some(1_700_000_000),
        },
        plan_dest_exists: true,
        plan_total_files: 1,
        plan_conflict_count: 0,
        owner: "t1",
        server: "srv-e9",
    };
    let warn = FakeRunner::new(|_| Ok(fail(1, "warning:  stripped absolute path spec\n")));
    let outcome = extract(&warn, &fs, &request, &never()).await.unwrap();
    assert!(outcome.warning.unwrap().contains("stripped"));
    let bad = FakeRunner::new(|_| Ok(fail(82, "skipping: x  incorrect password\n")));
    let err = extract(&bad, &fs, &request, &never()).await.unwrap_err();
    assert!(err.to_string().contains("incorrect password"), "{err}");
}

#[tokio::test]
async fn extract_refuses_when_the_archive_changed_after_the_check() {
    let fs = archive_fs();
    fs.put_dir("/d/site");
    fs.put_file("/d/site.tar.gz", 9999);
    let runner = FakeRunner::new(|_| panic!("检查之后被换掉的包不能解"));
    let err = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::Overwrite, "srv-e10", true, 0),
        &never(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("被改动"), "{err}");
}

#[tokio::test]
async fn extract_refuses_when_the_destination_changed_after_the_check() {
    let fs = archive_fs();
    fs.put_dir("/d/site"); // 检查时不存在，现在有了
    let runner = FakeRunner::new(|_| panic!("目标状态变了不能直接解"));
    let err = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::Overwrite, "srv-e11", false, 0),
        &never(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("状态"), "{err}");
}

#[tokio::test]
async fn cancel_flag_set_before_start_runs_nothing() {
    let fs = archive_fs();
    let runner = FakeRunner::new(|_| panic!("已经取消了，不该执行命令"));
    let cancel = AtomicBool::new(true);
    let err = extract(
        &runner,
        &fs,
        &extract_request(ConflictPolicy::Overwrite, "srv-e12", false, 0),
        &cancel,
    )
    .await
    .unwrap_err();
    assert_eq!(err, ArchiveError::Cancelled);
    let list = names(&["a"]);
    let err = compress(
        &runner,
        &fs,
        &compress_request(&list, "srv-e12", CompressFormat::TarGz),
        &cancel,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err, ArchiveError::Cancelled);
}

#[tokio::test]
async fn cleanup_never_touches_paths_that_are_not_our_temp_files() {
    let runner = FakeRunner::new(|_| panic!("不是临时文件，不该执行 rm"));
    let fs = FakeFs::default();
    fs.put_file("/d/precious", 1);
    cleanup_temp(&runner, &fs, "/d/precious").await;
    assert!(fs.has("/d/precious"));
}

// ---------------------------------------------------------------------------
// 包内符号链接的目标（2026-11-11 审查）：指向解压目录外面的整包拒绝
// ---------------------------------------------------------------------------

/// 手工拼一个 tar 条目（ustar 头 512 字节 + 数据补齐到 512）。`typeflag`：`0` 文件、`1` 硬链接、`2` 符号链接。
fn tar_entry(name: &str, typeflag: u8, linkname: &str, data: &[u8]) -> Vec<u8> {
    let mut header = [0u8; 512];
    let put = |header: &mut [u8; 512], at: usize, text: &str| {
        header[at..at + text.len()].copy_from_slice(text.as_bytes());
    };
    put(&mut header, 0, name);
    put(&mut header, 100, "0000777\0");
    put(&mut header, 108, "0000000\0");
    put(&mut header, 116, "0000000\0");
    put(&mut header, 124, &format!("{:011o}\0", data.len()));
    put(&mut header, 136, "00000000000\0");
    header[156] = typeflag;
    put(&mut header, 157, linkname);
    put(&mut header, 257, "ustar\0");
    put(&mut header, 263, "00");
    // 校验和按“校验和字段先当 8 个空格”算。
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    put(&mut header, 148, &format!("{sum:06o}\0 "));
    let mut out = header.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

fn tar_archive(entries: &[(&str, u8, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, typeflag, linkname) in entries {
        let data: &[u8] = if *typeflag == b'0' { b"x" } else { b"" };
        out.extend(tar_entry(name, *typeflag, linkname, data));
    }
    out.resize(out.len() + 1024, 0);
    out
}

fn zip_archive(entries: &[(&str, Option<&str>)]) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, link_target) in entries {
        match link_target {
            // 符号链接条目：Unix 权限位里的文件类型是 `S_IFLNK`，内容就是目标路径。
            Some(target) => writer.add_symlink(*name, *target, options).unwrap(),
            None => {
                writer.start_file(*name, options).unwrap();
                writer.write_all(b"content").unwrap();
            }
        }
    }
    writer.finish().unwrap().into_inner()
}

/// 本机有 `sh` 和脚本里用到的工具才能真跑；没有就跳过（Windows 上没装 Git 的 bash 就是这样）。
fn script_tools_available() -> bool {
    let probe = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v awk tar unzip mktemp sed head tr >/dev/null 2>&1 && for t in awk tar unzip mktemp sed head tr; do command -v $t >/dev/null 2>&1 || exit 1; done")
        .output();
    probe.is_ok_and(|out| out.status.success())
}

/// 在临时目录里真跑一次解压前复核；`None` = 本机跑不了，跳过。返回（是否放行, stderr）。
fn run_validate(format: ArchiveFormat, archive_name: &str, bytes: &[u8]) -> Option<(bool, String)> {
    if !script_tools_available() {
        eprintln!("跳过：本机没有 sh / awk / tar / unzip 等工具，没法真跑复核脚本");
        return None;
    }
    let dir = crate::test_util::TestDir::new("archive-links");
    std::fs::write(dir.join(archive_name), bytes).unwrap();
    // 用相对路径：tar 会把 `C:\...` 当成 `主机:路径`。
    let script = validate_list_command(format, archive_name);
    // 脚本从 stdin 喂给 `sh -s`：Windows 上经命令行参数传，msys 会吃掉反斜杠，脚本里的 `\` 就变了样。
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = std::process::Command::new("sh")
        .arg("-s")
        .current_dir(&*dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn assert_tar(entries: &[(&str, u8, &str)], allowed: bool, why: &str) {
    let Some((ok, stderr)) = run_validate(ArchiveFormat::Tar, "a.tar", &tar_archive(entries))
    else {
        return;
    };
    assert_eq!(ok, allowed, "{why}：{entries:?} stderr={stderr}");
    if !allowed {
        assert!(stderr.contains("符号链接"), "{why}：{stderr}");
    }
}

#[test]
fn tar_symlinks_that_stay_inside_are_allowed() {
    assert_tar(
        &[
            ("pkg/bin/tool.js", b'0', ""),
            // node_modules/.bin 这类常见写法。
            ("pkg/.bin/tool", b'2', "../bin/tool.js"),
            ("pkg/cur", b'2', "."),
            ("pkg/up", b'2', "../pkg"),
            // 名字里自己带 " -> "，目标本身是安全的相对路径。
            ("a b -> c", b'2', "x"),
        ],
        true,
        "留在目录里的链接",
    );
    assert_tar(
        &[("d/f", b'0', ""), ("d/hard", b'1', "d/f")],
        true,
        "硬链接指向包内成员",
    );
}

#[test]
fn tar_symlinks_pointing_outside_are_refused() {
    assert_tar(&[("l", b'2', "/etc")], false, "绝对路径");
    assert_tar(&[("d/l", b'2', "../../x")], false, "比所在深度多一级 ..");
    assert_tar(&[("l", b'2', "..")], false, "顶层链接指向上一级");
    assert_tar(&[("l", b'2', "a/../../x")], false, "中途退出目录");
    // 在安全的链接后面夹一个越界的：整包拒绝。
    assert_tar(
        &[("ok", b'2', "f"), ("f", b'0', ""), ("evil", b'2', "/root")],
        false,
        "混在正常链接里",
    );
    // 名字里带 " -> " 的链接，目标越界：每种切法都要查，不能只看最后一段。
    assert_tar(&[("a -> b", b'2', "/etc")], false, "名字里带箭头");
}

fn assert_zip(entries: &[(&str, Option<&str>)], allowed: bool, why: &str) {
    let Some((ok, stderr)) = run_validate(ArchiveFormat::Zip, "a.zip", &zip_archive(entries))
    else {
        return;
    };
    assert_eq!(ok, allowed, "{why}：{entries:?} stderr={stderr}");
    if !allowed {
        assert!(stderr.contains("符号链接"), "{why}：{stderr}");
    }
}

#[test]
fn zip_symlinks_that_stay_inside_are_allowed() {
    assert_zip(
        &[
            ("pkg/a.txt", None),
            ("pkg/l", Some("a.txt")),
            ("pkg/sub/up", Some("../a.txt")),
            // 名字里有 unzip 会当通配符的字符。
            ("pkg/[x]*?", Some("a.txt")),
        ],
        true,
        "留在目录里的链接",
    );
    assert_zip(&[("plain.txt", None)], true, "没有链接的包");
}

#[test]
fn zip_symlinks_pointing_outside_are_refused() {
    assert_zip(&[("abs", Some("/etc"))], false, "绝对路径");
    assert_zip(&[("d/l", Some("../../etc"))], false, "越界的 ..");
    assert_zip(
        &[("pkg/[x]*?", Some("../../etc")), ("pkg/ok", Some("a"))],
        false,
        "名字带通配符的越界链接",
    );
    // Windows 打的包常用反斜杠。
    assert_zip(&[("d\\l", Some("..\\..\\x"))], false, "反斜杠写法");
}

/// 脚本本身：填完之后没有残留占位符，压缩包路径（含占位符样子的名字）仍然整段在单引号里。
#[test]
fn the_link_check_is_part_of_the_pre_extract_script() {
    for format in [ArchiveFormat::Tar, ArchiveFormat::TarGz, ArchiveFormat::Zip] {
        let script = validate_list_command(format, "/d/a b.pkg");
        assert!(!script.contains('@'), "{script}");
        assert!(script.contains("mktemp -d"), "{script}");
        assert!(script.contains("function esc("), "{script}");
        assert!(script.contains("符号链接"), "{script}");
    }
}
