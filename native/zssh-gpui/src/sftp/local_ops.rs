//! 本地栏的文件系统操作：列目录（含“此电脑”盘符列表）、新建、改名、删除。
//!
//! 行为照 0.2.14 的 `sftp_local_*` 命令：路径一律用 `sys_path`（Windows 上是 `long_path`）包一层再交给系统（超长路径），
//! 给界面看的路径不带 `\\?\` 前缀；改名不覆盖，新建文件不截断已有文件；
//! 删除符号链接只删链接本身。

use std::path::{Path, PathBuf};

use zssh_core::sftp::local::{
    create_new_file, is_reparse_point, logical_drives, move_no_replace, replace_file,
};

use super::local_path::{sys_path, ui_path};
use super::model::{Entry, EntryKind};
use crate::i18n::t;

/// 列一个本地目录；Windows 上空路径是“此电脑”（盘符），Linux 上没有这一层，空路径就当 `/`。
pub(super) fn list_dir(path: &str) -> Result<Vec<Entry>, String> {
    let path = if path.is_empty() && !cfg!(windows) {
        "/"
    } else {
        path
    };
    if path.is_empty() {
        return Ok(drives());
    }
    // 给界面的路径不能带 `\\?\` 前缀：面包屑和“向上”按 `X:\` 的形状切路径，带前缀就全乱了。
    // 前缀只在真正调系统接口时加；传进来的路径本身带着前缀也先剥掉。
    let dir = ui_path(PathBuf::from(path));
    let listing = std::fs::read_dir(sys_path(&dir)).map_err(|err| err.to_string())?;
    let mut out = Vec::new();
    for item in listing {
        let item = item.map_err(|err| err.to_string())?;
        let name = item.file_name();
        out.push(entry_of(
            dir.join(&name),
            &name.to_string_lossy(),
            item.metadata().ok(),
        ));
    }
    Ok(out)
}

/// 一个本地条目转成 [`Entry`]（失败 / 拿不到的属性按缺省处理，不让一条坏条目连累整个目录）。
///
/// `own` 是条目本身的属性（不跟随链接）：普通文件和目录直接用它；符号链接、目录联接
/// 才再去读它指向的目标（大小、类型、时间都按目标显示）。
fn entry_of(path: PathBuf, name: &str, own: Option<std::fs::Metadata>) -> Entry {
    let own = own.or_else(|| std::fs::symlink_metadata(&path).ok());
    let is_link = own
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_symlink() || is_reparse_point(meta));
    let meta = match (&own, is_link) {
        (Some(meta), false) => Ok(meta.clone()),
        _ => std::fs::metadata(sys_path(&path)),
    };
    let (kind, size) = match &meta {
        Ok(meta) if meta.is_dir() => (EntryKind::Dir, None),
        Ok(meta) => (EntryKind::File, Some(meta.len())),
        Err(_) => (EntryKind::Other, None),
    };
    // 链接：记下它指向哪儿（列表里画成链接用，操作仍按跟随之后的类型走）。读不出路径的（比如 OneDrive
    // 的云文件占位符也是重解析点）不当链接画。
    let link_target = is_link
        .then(|| std::fs::read_link(sys_path(&path)).ok())
        .flatten()
        .map(|target| target.display().to_string());
    let hidden = own
        .as_ref()
        .and_then(is_hidden)
        .unwrap_or_else(|| name.starts_with('.'));
    let mtime = meta
        .as_ref()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    Entry {
        name: name.to_owned(),
        path: path.display().to_string(),
        kind,
        target_kind: None,
        link_target,
        size,
        mtime,
        permissions: None,
        owner: None,
        hidden,
        name_is_utf8: true,
    }
}

/// Windows 的“隐藏”属性（`FILE_ATTRIBUTE_HIDDEN`）；别的系统没有这个概念，返回 `None` 由调用方按点开头算。
#[cfg(windows)]
fn is_hidden(meta: &std::fs::Metadata) -> Option<bool> {
    use std::os::windows::fs::MetadataExt;
    Some(meta.file_attributes() & 0x2 != 0)
}

#[cfg(not(windows))]
fn is_hidden(_meta: &std::fs::Metadata) -> Option<bool> {
    None
}

/// “此电脑”：所有盘符。
fn drives() -> Vec<Entry> {
    let letters = logical_drives();
    if letters.is_empty() {
        // 不是 Windows（开发机上跑测试）：给一个根目录，免得“此电脑”是空的。
        return vec![Entry {
            name: "/".to_owned(),
            path: "/".to_owned(),
            kind: EntryKind::Dir,
            target_kind: None,
            link_target: None,
            size: None,
            mtime: None,
            permissions: None,
            owner: None,
            hidden: false,
            name_is_utf8: true,
        }];
    }
    letters
        .into_iter()
        .map(|letter| Entry {
            name: format!(
                "{}{letter}{}",
                t(super::SftpMsg::LocalDiskPrefix),
                t(super::SftpMsg::LocalDiskSuffix)
            ),
            path: format!("{letter}:\\"),
            kind: EntryKind::Drive,
            target_kind: None,
            link_target: None,
            size: None,
            mtime: None,
            permissions: None,
            owner: None,
            hidden: false,
            name_is_utf8: true,
        })
        .collect()
}

pub(super) fn mkdir(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(t(super::SftpMsg::DirNameEmpty).to_owned());
    }
    // 不用 create_dir_all：已经有同名的就报错，不静悄悄当成功。
    std::fs::create_dir(sys_path(Path::new(path))).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            t(super::SftpMsg::AlreadyExists).to_owned()
        } else {
            err.to_string()
        }
    })
}

pub(super) fn create_file(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(t(super::SftpMsg::FileNameEmpty).to_owned());
    }
    // 不先查再建：由系统在创建那一刻判断，已存在的同名文件不会被截断。
    create_new_file(Path::new(path)).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            t(super::SftpMsg::SameNameFile).to_owned()
        } else {
            err.to_string()
        }
    })
}

pub(super) fn rename(from: &str, to: &str) -> Result<(), String> {
    if from.trim().is_empty() || to.trim().is_empty() {
        return Err(t(super::SftpMsg::PathEmpty).to_owned());
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(t(super::SftpMsg::AlreadyExists).to_owned());
    }
    // 系统在改名那一刻判断目标是否存在（上面的检查只是为了给出好读的提示，不能代替它）。
    move_no_replace(&sys_path(Path::new(from)), &sys_path(Path::new(to))).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            t(super::SftpMsg::AlreadyExists).to_owned()
        } else {
            err.to_string()
        }
    })
}

/// 「移动到…」用的本地移动（V2-U23）：`overwrite` 为假时目标存在就报 `Exists`，绝不替换（由系统在改名那
/// 一刻判断）；为真时只用于“文件替换文件”（调用方保证目标不是文件夹）。跨磁盘不做复制，只说清楚。
pub(super) fn move_item(from: &Path, to: &Path, overwrite: bool) -> super::move_to::StepResult {
    use super::move_to::{StepResult, cross_device_note};

    if from.as_os_str().is_empty() || to.as_os_str().is_empty() {
        return StepResult::Failed(t(super::SftpMsg::PathEmpty).to_owned());
    }
    let (long_from, long_to) = (sys_path(from), sys_path(to));
    let result = if overwrite {
        replace_file(&long_from, &long_to)
    } else {
        move_no_replace(&long_from, &long_to)
    };
    match result {
        Ok(()) => StepResult::Moved,
        Err(err) if !overwrite && err.kind() == std::io::ErrorKind::AlreadyExists => {
            StepResult::Exists {
                target_is_dir: std::fs::symlink_metadata(&long_to).is_ok_and(|meta| meta.is_dir()),
            }
        }
        Err(err) if is_cross_device(&err) => StepResult::Failed(cross_device_note().to_owned()),
        Err(err) => StepResult::Failed(err.to_string()),
    }
}

/// 改名跨了磁盘（Windows `ERROR_NOT_SAME_DEVICE` = 17，POSIX `EXDEV` = 18）。
fn is_cross_device(err: &std::io::Error) -> bool {
    if cfg!(windows) {
        err.raw_os_error() == Some(17)
    } else {
        err.raw_os_error() == Some(18)
    }
}

/// 删除本地条目（可以一次多个；目录递归；链接只删链接）。返回每个的成败。
pub(super) fn delete(paths: &[String]) -> Vec<(String, Option<String>)> {
    paths
        .iter()
        .map(|path| {
            let long = sys_path(Path::new(path));
            let result = match std::fs::symlink_metadata(&long) {
                Ok(meta) if meta.file_type().is_symlink() || is_reparse_point(&meta) => {
                    // 链接只删链接本身；目录联接 / 目录符号链接要用 remove_dir。
                    std::fs::remove_file(&long).or_else(|_| std::fs::remove_dir(&long))
                }
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&long),
                Ok(_) => std::fs::remove_file(&long),
                Err(err) => Err(err),
            };
            (path.clone(), result.err().map(|err| err.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::model::EntryKind;
    use super::{create_file, delete, list_dir, mkdir, move_item, rename};

    /// 测试用的临时目录：Drop 时整个删掉，断言失败也不留垃圾。
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir().join(format!(
                "zssh-sftp-ops-{name}-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("建临时目录");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn child(&self, name: &str) -> String {
            self.0.join(name).display().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Linux：没有“此电脑”，空路径就是 `/`；条目路径是 POSIX 的；`//` 开头的合法路径不会被当成 UNC。
    #[cfg(unix)]
    #[test]
    fn linux_listing_has_no_this_pc_level_and_keeps_posix_paths() {
        let root = list_dir("").expect("空路径当作根目录");
        assert!(!root.is_empty());
        assert!(
            root.iter()
                .all(|row| row.path.starts_with('/') && row.kind != EntryKind::Drive)
        );
        assert_eq!(
            list_dir("/").expect("根目录").len(),
            root.len(),
            "空路径和 `/` 是同一个目录"
        );

        let dir = Scratch::new("posix");
        std::fs::create_dir(dir.path().join("下载 目录")).expect("中文目录");
        std::fs::create_dir(dir.path().join("a\\b")).expect("名字里带反斜杠的目录");
        std::fs::write(dir.path().join("Docs"), b"x").expect("大写文件");
        std::fs::write(dir.path().join("docs"), b"y").expect("小写文件");
        let shown = dir.path().display().to_string();
        let rows = list_dir(&shown).expect("列目录");
        let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Docs", "a\\b", "docs", "下载 目录"]);
        let backslash = rows
            .iter()
            .find(|row| row.name == "a\\b")
            .expect("反斜杠目录");
        assert_eq!(backslash.path, format!("{shown}/a\\b"));
        assert_eq!(backslash.kind, EntryKind::Dir);
        // 双斜杠开头也是普通的 POSIX 路径。
        let doubled = list_dir(&format!("/{shown}")).expect("双斜杠路径");
        assert_eq!(doubled.len(), rows.len());
    }

    /// Linux：新建 / 改名对名字里的 Windows 字符、反斜杠一视同仁，大小写不同的两个名字可以共存。
    #[cfg(unix)]
    #[test]
    fn linux_create_and_rename_accept_posix_names() {
        let dir = Scratch::new("posix-ops");
        for name in ["Docs", "docs", "a\\b", "x:y*z?", "报告 终稿"] {
            mkdir(&dir.child(name)).unwrap_or_else(|err| panic!("{name:?}: {err}"));
        }
        assert_eq!(
            list_dir(&dir.path().display().to_string())
                .expect("列目录")
                .len(),
            5
        );
        rename(&dir.child("Docs"), &dir.child("DOCS")).expect("只改大小写");
        assert!(dir.path().join("DOCS").is_dir() && dir.path().join("docs").is_dir());
        // 目标已经有同名（区分大小写）条目：不覆盖。
        assert_eq!(
            rename(&dir.child("DOCS"), &dir.child("docs"))
                .err()
                .as_deref(),
            Some("已经有同名的条目")
        );
    }

    /// Unix 上点开头的就是隐藏文件（没有 Windows 那种「隐藏」属性）；列表据此决定「显示隐藏文件」开关藏哪些。
    #[cfg(unix)]
    #[test]
    fn dot_files_are_hidden_on_unix_and_the_toggle_shows_them() {
        use super::super::model::{Filter, filter_entries, hidden_count};

        let dir = Scratch::new("hidden");
        std::fs::write(dir.path().join(".profile"), b"x").expect("写点文件");
        std::fs::create_dir(dir.path().join(".config")).expect("建点目录");
        std::fs::write(dir.path().join("visible.txt"), b"x").expect("写普通文件");
        std::fs::write(dir.path().join("trailing."), b"x").expect("写末尾带点的文件");
        let rows = list_dir(&dir.path().display().to_string()).expect("列目录");
        let hidden = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .map(|row| row.hidden)
        };
        assert_eq!(hidden(".profile"), Some(true));
        assert_eq!(hidden(".config"), Some(true));
        assert_eq!(hidden("visible.txt"), Some(false));
        assert_eq!(hidden("trailing."), Some(false), "只看开头的点");

        let names = |filter: &Filter| -> Vec<String> {
            let mut names: Vec<String> = filter_entries(&rows, filter)
                .into_iter()
                .map(|row| row.name)
                .collect();
            names.sort();
            names
        };
        let off = Filter {
            show_hidden: false,
            ..Filter::default()
        };
        let on = Filter {
            show_hidden: true,
            ..Filter::default()
        };
        assert_eq!(names(&off), ["trailing.", "visible.txt"]);
        assert_eq!(
            names(&on),
            [".config", ".profile", "trailing.", "visible.txt"]
        );
        assert_eq!(hidden_count(&rows, &off), 2, "提示「有 2 项被隐藏」");
        assert_eq!(hidden_count(&rows, &on), 0);
    }

    #[test]
    fn listing_reads_files_and_directories_with_sizes_and_times() {
        let dir = Scratch::new("list");
        std::fs::create_dir(dir.path().join("sub")).expect("建子目录");
        std::fs::write(dir.path().join("a.txt"), b"hello").expect("写文件");
        let mut rows = list_dir(&dir.path().display().to_string()).expect("列目录");
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "a.txt");
        assert_eq!(rows[0].kind, EntryKind::File);
        assert_eq!(rows[0].size, Some(5));
        assert!(rows[0].mtime.is_some_and(|t| t > 0));
        assert_eq!(rows[1].name, "sub");
        assert_eq!(rows[1].kind, EntryKind::Dir);
        assert_eq!(rows[1].size, None, "目录不显示大小");
        assert!(
            rows.iter()
                .all(|r| r.name_is_utf8 && r.permissions.is_none())
        );
    }

    #[test]
    fn listing_an_unreadable_path_reports_why() {
        let dir = Scratch::new("missing");
        let missing = dir.child("nope");
        assert!(list_dir(&missing).is_err());
    }

    #[test]
    fn the_empty_path_lists_drives() {
        let rows = list_dir("").expect("盘符");
        assert!(!rows.is_empty());
        #[cfg(windows)]
        assert!(
            rows.iter()
                .all(|r| r.kind == EntryKind::Drive && r.path.ends_with(":\\"))
        );
    }

    #[test]
    fn mkdir_and_create_refuse_existing_names_without_touching_them() {
        let dir = Scratch::new("create");
        let folder = dir.child("d");
        mkdir(&folder).expect("新建文件夹");
        assert_eq!(mkdir(&folder).err().as_deref(), Some("已经有同名的条目"));
        assert_eq!(mkdir("  ").err().as_deref(), Some("目录名不能为空"));

        let file = dir.child("f.txt");
        create_file(&file).expect("新建文件");
        std::fs::write(&file, b"keep me").expect("写内容");
        assert_eq!(
            create_file(&file).err().as_deref(),
            Some("已经有同名的文件")
        );
        assert_eq!(std::fs::read(&file).expect("读回来"), b"keep me");
    }

    #[test]
    fn rename_never_replaces_an_existing_entry() {
        let dir = Scratch::new("rename");
        let (a, b) = (dir.child("a.txt"), dir.child("b.txt"));
        std::fs::write(&a, b"A").expect("写 a");
        std::fs::write(&b, b"B").expect("写 b");
        assert_eq!(rename(&a, &b).err().as_deref(), Some("已经有同名的条目"));
        assert_eq!(std::fs::read(&b).expect("b"), b"B");
        let c = dir.child("c.txt");
        rename(&a, &c).expect("改名");
        assert!(!Path::new(&a).exists());
        assert_eq!(std::fs::read(&c).expect("c"), b"A");
        assert_eq!(rename("", &c).err().as_deref(), Some("路径不能为空"));
    }

    #[test]
    fn move_item_moves_files_and_whole_folders_with_awkward_names() {
        use super::super::move_to::StepResult;
        let dir = Scratch::new("move");
        let dest = dir.child("目标 文件夹");
        std::fs::create_dir(&dest).expect("建目标");
        // 空格、中文、引号、美元符、分号：都按原样当文件名。
        let odd = dir.child("报告 最终 'v2' $x;.txt");
        std::fs::write(&odd, b"body").expect("写文件");
        let to = Path::new(&dest).join("报告 最终 'v2' $x;.txt");
        assert_eq!(move_item(Path::new(&odd), &to, false), StepResult::Moved);
        assert!(!Path::new(&odd).exists());
        assert_eq!(std::fs::read(&to).expect("读回来"), b"body");

        let tree = dir.child("tree");
        std::fs::create_dir_all(Path::new(&tree).join("inner/deeper")).expect("建树");
        std::fs::write(Path::new(&tree).join("inner/deeper/x.txt"), b"x").expect("树里的文件");
        let tree_to = Path::new(&dest).join("tree");
        assert_eq!(
            move_item(Path::new(&tree), &tree_to, false),
            StepResult::Moved
        );
        assert!(!Path::new(&tree).exists());
        assert_eq!(
            std::fs::read(tree_to.join("inner/deeper/x.txt")).expect("整棵树都在"),
            b"x"
        );
    }

    #[test]
    fn move_item_reports_a_clash_with_the_kind_and_never_replaces_without_consent() {
        use super::super::move_to::StepResult;
        let dir = Scratch::new("move-clash");
        let dest = dir.child("dest");
        std::fs::create_dir(&dest).expect("建目标");
        std::fs::write(Path::new(&dest).join("a.txt"), b"old").expect("目标里的同名文件");
        std::fs::create_dir(Path::new(&dest).join("folder")).expect("目标里的同名文件夹");
        let a = dir.child("a.txt");
        std::fs::write(&a, b"new").expect("源文件");
        let folder = dir.child("folder");
        std::fs::create_dir(&folder).expect("源文件夹");

        assert_eq!(
            move_item(Path::new(&a), &Path::new(&dest).join("a.txt"), false),
            StepResult::Exists {
                target_is_dir: false
            }
        );
        assert_eq!(
            move_item(Path::new(&folder), &Path::new(&dest).join("folder"), false),
            StepResult::Exists {
                target_is_dir: true
            }
        );
        // 两个源都没被动过，目标里的旧内容也在。
        assert_eq!(std::fs::read(&a).expect("源还在"), b"new");
        assert!(Path::new(&folder).is_dir());
        assert_eq!(
            std::fs::read(Path::new(&dest).join("a.txt")).expect("旧内容还在"),
            b"old"
        );

        // 同意替换之后：文件替换文件。
        assert_eq!(
            move_item(Path::new(&a), &Path::new(&dest).join("a.txt"), true),
            StepResult::Moved
        );
        assert!(!Path::new(&a).exists());
        assert_eq!(
            std::fs::read(Path::new(&dest).join("a.txt")).expect("新内容"),
            b"new"
        );
    }

    #[test]
    fn move_item_failures_say_why_and_leave_things_alone() {
        use super::super::move_to::StepResult;
        let dir = Scratch::new("move-fail");
        let missing = dir.child("nope.txt");
        let to = dir.child("x.txt");
        let StepResult::Failed(reason) = move_item(Path::new(&missing), Path::new(&to), false)
        else {
            panic!("源不存在应该失败");
        };
        assert!(!reason.is_empty());
        assert!(!Path::new(&to).exists());
        // 目标文件夹不存在也是失败，不会替用户建。
        let src = dir.child("s.txt");
        std::fs::write(&src, b"s").expect("写源");
        let nowhere = Path::new(&dir.child("no-such-dir")).join("s.txt");
        assert!(matches!(
            move_item(Path::new(&src), &nowhere, false),
            StepResult::Failed(_)
        ));
        assert!(Path::new(&src).exists());
        assert_eq!(
            move_item(Path::new(""), Path::new(&to), false),
            StepResult::Failed("路径不能为空".to_owned())
        );
    }

    #[test]
    fn a_cross_device_error_is_recognised_per_platform() {
        let code = if cfg!(windows) { 17 } else { 18 };
        assert!(super::is_cross_device(&std::io::Error::from_raw_os_error(
            code
        )));
        assert!(!super::is_cross_device(&std::io::Error::from_raw_os_error(
            2
        )));
    }

    /// V2-U1 第 9 条：本地的链接（符号链接、目录联接）列表里仍是跟随之后的类型，同时带上目标路径，
    /// 界面据此画成「链接」。目录联接不需要管理员权限，能建就测，建不了（别的系统、策略不允许）跳过。
    #[test]
    fn a_local_junction_lists_with_its_target_path_and_a_plain_folder_does_not() {
        let dir = Scratch::new("link");
        let real = dir.child("real-dir");
        std::fs::create_dir_all(&real).expect("建目标目录");
        let link = dir.child("link-dir");
        let made = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J", &link, &real])
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        } else {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real, &link).is_ok()
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        if !made {
            println!("跳过：这里建不了目录联接 / 符号链接");
            return;
        }
        let rows = list_dir(&dir.path().display().to_string()).expect("列目录");
        let link_row = rows
            .iter()
            .find(|e| e.name == "link-dir")
            .expect("链接那行");
        assert_eq!(link_row.kind, EntryKind::Dir, "操作仍按跟随之后的类型走");
        let target = link_row.link_target.as_deref().expect("读到目标路径");
        assert!(target.ends_with("real-dir"), "{target}");
        let info = link_row.link_info().expect("按链接画");
        assert_eq!(info.base, super::super::model::LinkBase::Folder);
        assert!(!info.broken);
        let real_row = rows
            .iter()
            .find(|e| e.name == "real-dir")
            .expect("目标那行");
        assert!(real_row.link_info().is_none(), "普通文件夹不是链接");
    }
    #[test]
    fn delete_removes_files_and_whole_trees_and_reports_each_failure() {
        let dir = Scratch::new("delete");
        let file = dir.child("f.txt");
        std::fs::write(&file, b"x").expect("写文件");
        let tree = dir.child("tree");
        std::fs::create_dir_all(Path::new(&tree).join("inner/deeper")).expect("建树");
        std::fs::write(Path::new(&tree).join("inner/deeper/x"), b"x").expect("树里的文件");
        let missing = dir.child("missing");
        let results = delete(&[file.clone(), tree.clone(), missing.clone()]);
        assert_eq!(results.len(), 3);
        assert!(results[0].1.is_none() && results[1].1.is_none());
        assert!(results[2].1.is_some(), "不存在的要报出来");
        assert!(!Path::new(&file).exists() && !Path::new(&tree).exists());
    }
}
