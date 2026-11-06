//! 本地这一侧的兼容（M3-1 第 3 节）。
//!
//! 远端（POSIX）能有的名字，Windows 上不一定能落盘：`\ / : * ? " < > |`、以点或空格
//! 结尾、`CON` / `NUL` / `COM1` 这类保留名、大小写只差一个字母的两个文件、超过 260 个
//! 字符的路径。下载时要把这些处理好：**能改名的改名并在任务里注明**，不能含糊地丢一个
//! `os error 123` 出去。
//!
//! Linux / 其他 Unix 的本地文件系统和远端是同一套规则：只有 `/` 和 NUL 不能出现在名字里，
//! 区分大小写，`\` 是普通字符，也没有长路径前缀。所以上面这些 Windows 规则**只在 Windows 上
//! 生效**——两套实现都会编译（用 `cfg!(windows)` 选），测试在两个系统上各跑各自那一套。
//!
//! 这一层只做**纯计算**（名字规范化、长路径前缀、大小写冲突分配）和**本地树的遍历**，
//! 不碰网络，所以单测可以直接造临时目录验证。

use std::path::{Path, PathBuf};

use super::error::{from_io, SftpError};
use crate::error::{reason_keys, Reason};

/// Windows 文件名里不允许出现的字符。
const ILLEGAL_CHARS: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// 设备保留名（不区分大小写；带扩展名也算，例如 `CON.txt`）。
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 名字替换用的字符（和很多命令行工具一样用下划线）。
const REPLACEMENT: char = '_';

/// 一个远端名字落到 Windows 上要变成什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalName {
    /// 最终用的名字（已经规范化过；没有变化时等于原名）。
    pub name: String,
    /// 改过名的原因（`None` = 原名就能用）。界面把它们列出来。
    pub note: Option<String>,
    /// 同样的原因，结构化：英文界面按它们重新生成（`note` 是中文原文）。
    pub fixes: Vec<NameFix>,
}

impl LocalName {
    /// 不需要改名。
    pub fn plain(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            note: None,
            fixes: Vec::new(),
        }
    }

    /// 中文说明由修正们拼出来，`note` 和 `fixes` 永远一致。
    pub fn with_fixes(name: String, fixes: Vec<NameFix>) -> Self {
        let note = (!fixes.is_empty())
            .then(|| fixes.iter().map(NameFix::zh).collect::<Vec<_>>().join("；"));
        Self { name, note, fixes }
    }
}

/// 一个远端名字为了能在 Windows 上用而被改掉的一处。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameFix {
    /// 非法字符（`label` 是字符本身，控制字符写成 `\x07`）换成了 `REPLACEMENT`。
    IllegalChar { label: String },
    /// 结尾的点或空格去掉了。
    TrailingDotSpace,
    /// Windows 保留名，加了前缀。
    Reserved { name: String },
    /// 名字变成空的，用 `_` 代替。
    Empty,
    /// 和同一批的另一个条目同名（只差大小写），改成了 `candidate`。
    BatchClash { candidate: String },
    /// 本地已有同名的、类型不同的条目（`existing_is_dir`），改成了 `candidate`。
    KindClash {
        existing_is_dir: bool,
        candidate: String,
    },
}

impl NameFix {
    pub fn zh(&self) -> String {
        match self {
            Self::IllegalChar { label } => format!("非法字符 {label} 换成 {REPLACEMENT}"),
            Self::TrailingDotSpace => "结尾的点或空格去掉了".to_owned(),
            Self::Reserved { name } => format!("{name} 是 Windows 保留名，加了前缀"),
            Self::Empty => "名字变成空的，用 _ 代替".to_owned(),
            Self::BatchClash { candidate } => {
                format!("和同一批的另一个条目同名（只差大小写），改成 {candidate}")
            }
            Self::KindClash {
                existing_is_dir: false,
                candidate,
            } => format!("本地已有同名文件，改成 {candidate}"),
            Self::KindClash {
                existing_is_dir: true,
                candidate,
            } => format!("本地已有同名文件夹，改成 {candidate}"),
        }
    }

    /// 稳定的代码（界面据此出词）：`illegal:<字符>`、`trailing`、`reserved:<名字>`、`empty`。
    pub fn code(&self) -> String {
        match self {
            Self::IllegalChar { label } => format!("illegal:{label}"),
            Self::TrailingDotSpace => "trailing".to_owned(),
            Self::Reserved { name } => format!("reserved:{name}"),
            Self::Empty => "empty".to_owned(),
            Self::BatchClash { candidate } => format!("clash_batch:{candidate}"),
            Self::KindClash {
                existing_is_dir: false,
                candidate,
            } => format!("clash_file:{candidate}"),
            Self::KindClash {
                existing_is_dir: true,
                candidate,
            } => format!("clash_dir:{candidate}"),
        }
    }

    /// 一串修正合成一个参数值（`\u{1f}` 分隔）。
    pub fn codes(fixes: &[NameFix]) -> String {
        fixes
            .iter()
            .map(NameFix::code)
            .collect::<Vec<_>>()
            .join("\u{1f}")
    }
}

/// 这个名字能不能直接用（不能用的原因见 [`LocalName::note`]）。
///
/// Windows 按 Windows 文件名规则改（见 [`windows_local_name`]）；Linux / 其他 Unix 按 POSIX 规则
/// （见 [`posix_local_name`]）：绝大多数远端名字原样落盘。
pub fn local_name(remote_name: &str) -> LocalName {
    if cfg!(windows) {
        windows_local_name(remote_name)
    } else {
        posix_local_name(remote_name)
    }
}

/// 记一处"非法字符换成了下划线"，同一个字符只记一次。
fn note_illegal_char(fixes: &mut Vec<NameFix>, label: String) {
    if !fixes
        .iter()
        .any(|fix| matches!(fix, NameFix::IllegalChar { label: seen } if seen == &label))
    {
        fixes.push(NameFix::IllegalChar { label });
    }
}

/// POSIX 本地的名字规则：只有 `/` 和 NUL 放不进文件名，`.` / `..` 不能当名字；其余（`:`、`?`、
/// `\`、结尾的点和空格、`CON`……）都是合法名字，原样保留；大小写敏感。
///
/// 控制字符（C0、DEL、C1，即 [`char::is_control`]）**也换成 `_` 并注明**，不原样保留：
/// - 远端是不受信任的一方，名字里带 ESC 序列时，用户事后在终端里 `ls` 这个目录，终端会执行它
///   （改标题、盖掉前面的输出、伪造提示符）；带换行的名字会让 `ls | while read` 一类脚本拆错。
/// - 现实里没有合理的文件会用控制字符当名字，换掉它们几乎不会改到合法名字；而且改名是写进任务
///   说明的，用户看得见，不是悄悄改。
/// - 和 Windows 一侧的行为一致（那边本来就换），界面的"改名原因"也已经能显示 `\x07` 这种写法。
///
/// NUL 属于控制字符，所以 `/` 和 NUL 一定会被换掉。
fn posix_local_name(remote_name: &str) -> LocalName {
    let mut fixes: Vec<NameFix> = Vec::new();
    let mut name = String::with_capacity(remote_name.len());
    for ch in remote_name.chars() {
        if ch == '/' || ch.is_control() {
            name.push(REPLACEMENT);
            let label = if ch.is_control() {
                format!("\\x{:02x}", ch as u32)
            } else {
                ch.to_string()
            };
            note_illegal_char(&mut fixes, label);
        } else {
            name.push(ch);
        }
    }
    // `.` 和 `..` 是目录项的特殊名字，不能建成文件；没有更合适的说法，记成"非法名字换成 _"。
    if name == "." || name == ".." {
        note_illegal_char(&mut fixes, name.clone());
        name = REPLACEMENT.to_string();
    }
    if name.is_empty() {
        fixes.push(NameFix::Empty);
        name = REPLACEMENT.to_string();
    }
    LocalName::with_fixes(name, fixes)
}

/// Windows 本地的名字规则（`<>:"|?*`、控制字符、结尾的点和空格、`CON` 等保留名）。
/// 在两个系统上都编译，Linux 上只有测试在用。
fn windows_local_name(remote_name: &str) -> LocalName {
    let mut fixes: Vec<NameFix> = Vec::new();
    let mut name = String::with_capacity(remote_name.len());

    for ch in remote_name.chars() {
        if ILLEGAL_CHARS.contains(&ch) || (ch as u32) < 0x20 {
            name.push(REPLACEMENT);
            let label = if (ch as u32) < 0x20 {
                format!("\\x{:02x}", ch as u32)
            } else {
                ch.to_string()
            };
            note_illegal_char(&mut fixes, label);
        } else {
            name.push(ch);
        }
    }

    // 结尾的点和空格（Windows 会悄悄去掉，导致名字和预期不一致）。
    let trimmed = name.trim_end_matches(['.', ' ']);
    if trimmed.len() != name.len() {
        fixes.push(NameFix::TrailingDotSpace);
        name = trimmed.to_owned();
    }

    // 保留名：加个前缀（`CON` → `_CON`）。
    if is_reserved_name(&name) {
        fixes.push(NameFix::Reserved { name: name.clone() });
        name = format!("_{name}");
    }

    if name.is_empty() {
        fixes.push(NameFix::Empty);
        name = "_".to_owned();
    }

    LocalName::with_fixes(name, fixes)
}

/// 用户在对话框里输入的"同一目录下的新名字"（另存为、粘贴时重命名）：必须原样就是一个合法的
/// 本地文件名（Windows 上按 Windows 规则，Linux 上按 POSIX 规则：只拒绝含 `/`、NUL 或控制字符的
/// 名字），不改写、不猜。
///
/// 只挡 `/`、`\`、`.`、`..` 不够：`C:evil.txt` 带盘符，`join` 之后会把父目录整个换掉，写到进程
/// 当前目录去；`a:b` 会写进 `a` 的 NTFS 备用数据流（R-10 M1）。调用方拼出路径之后还应该再用
/// [`ensure_inside`] 核对一次。
pub fn check_child_name(name: &str) -> Result<String, SftpError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(SftpError::InvalidInput(Reason::new(
            reason_keys::SFTP_NAME_EMPTY,
            "名字不能为空",
        )));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(SftpError::InvalidInput(Reason::new(
            reason_keys::SFTP_NAME_DOTS,
            "名字不能是 . 或 ..",
        )));
    }
    let checked = local_name(trimmed);
    if let Some(note) = checked.note {
        return Err(SftpError::InvalidInput(
            Reason::new(
                reason_keys::SFTP_NAME_UNUSABLE,
                format!("「{trimmed}」不能用作文件名：{note}"),
            )
            .with("name", trimmed)
            .with("fixes", NameFix::codes(&checked.fixes)),
        ));
    }
    Ok(trimmed.to_owned())
}

/// 是不是 Windows 保留名（按主名判断，`CON.txt` 也算）。纯字符串判断，和当前系统无关；
/// 只有 Windows 的名字规则会用它（Linux 上 `CON.txt` 是普通文件名）。
pub fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.trim().to_ascii_uppercase();
    RESERVED_NAMES.contains(&upper.as_str())
}

/// 给本地路径加长路径前缀（超过 260 字符时 Windows 才认）。**只有 Windows 加**：Linux 等其他
/// 系统原样返回——`//x` 在 POSIX 上是合法路径，`\\?\` 在那里也只是个普通名字，按盘符 / UNC 的
/// 形状改写只会把路径改坏。
///
/// - `C:\a\b` → `\\?\C:\a\b`
/// - `\\server\share\a` → `\\?\UNC\server\share\a`
/// - 已经是 `\\?\` 开头的原样返回；相对路径不加（`\\?\` 只认绝对路径）。
pub fn long_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        windows_long_path(path)
    } else {
        path.to_path_buf()
    }
}

/// [`long_path`] 在 Windows 上的实现（纯字符串改写，两个系统上都编译，Linux 上只有测试在用）。
fn windows_long_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.starts_with("\\\\?\\") {
        return path.to_path_buf();
    }
    // `\\?\` 前缀下系统不再把 `/` 当分隔符（`D:/data` 会报 os error 123），先统一成 `\`。
    let text = text.replace('/', "\\");
    if let Some(rest) = text.strip_prefix("\\\\") {
        return PathBuf::from(format!("\\\\?\\UNC\\{rest}"));
    }
    // 盘符绝对路径（`C:\`）。
    let looks_absolute = text.len() >= 3
        && text.as_bytes()[1] == b':'
        && (text.as_bytes()[2] == b'\\' || text.as_bytes()[2] == b'/');
    if looks_absolute {
        return PathBuf::from(format!("\\\\?\\{text}"));
    }
    path.to_path_buf()
}

/// 本地路径是不是该用长路径前缀（240 个字符就开始加，留点余量）。只有 Windows 有这个限制，
/// 其他系统永远是 `false`。
pub fn needs_long_path(path: &Path) -> bool {
    cfg!(windows) && path.to_string_lossy().chars().count() >= 240
}

/// 下载时的一份"本地名字分配"：同一个目录里落地的一批文件，处理改名和大小写冲突。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalNamePlan {
    /// 远端名字 → 本地名字。
    pub assignments: Vec<(String, LocalName)>,
}

impl LocalNamePlan {
    /// 给一批远端名字分本地名字（都按文件看待）。见 [`LocalNamePlan::plan_with_kinds`]。
    pub fn plan(remote_names: &[String], existing: &[String]) -> Self {
        let remote: Vec<(String, bool)> = remote_names
            .iter()
            .map(|name| (name.clone(), false))
            .collect();
        let existing: Vec<(String, bool)> =
            existing.iter().map(|name| (name.clone(), false)).collect();
        Self::plan_with_kinds(&remote, &existing)
    }

    /// 给一批远端条目分本地名字。`remote` 和 `existing` 都是 `(名字, 是不是目录)`。
    ///
    /// 规则：先做 [`local_name`] 规范化；同一批里规范化之后（或本来）**只差大小写**的两个名字
    /// 会撞在一起（Windows 不区分大小写），后面的加 ` (2)`、` (3)` 序号。Linux 上区分大小写，
    /// `File` 和 `file` 是两个文件，只有**完全相同**的名字才算撞。
    ///
    /// 和本地**已有的同类条目**同名不改名：文件同名是"目标已存在"，交给冲突对话框（覆盖 /
    /// 跳过 / 续传 / 重命名）；目录同名就下载进去、里面的文件各自走冲突处理。不能悄悄换成带
    /// 序号的新名字——重新下载同一个文件夹时用户就选不了覆盖或续传了（R-9 F13）。
    /// 已有条目类型不同（远端是目录、本地是同名文件，或者反过来）写不进去，照旧加序号。
    /// 加序号时挑的新名字要避开所有已有条目，免得撞上另一份无关的文件。
    pub fn plan_with_kinds(remote: &[(String, bool)], existing: &[(String, bool)]) -> Self {
        let existing: Vec<(String, bool)> = existing
            .iter()
            .map(|(name, is_dir)| (fold_case(name), *is_dir))
            .collect();
        // 这一批里已经分出去的名字。
        let mut taken: Vec<String> = Vec::new();
        let mut assignments = Vec::with_capacity(remote.len());
        for (remote_name, is_dir) in remote {
            let base = local_name(remote_name);
            let mut candidate = base.name.clone();
            let mut fixes = base.fixes.clone();
            let lower = fold_case(&candidate);
            let batch_clash = taken.contains(&lower);
            let kind_clash = existing
                .iter()
                .any(|(name, existing_dir)| *name == lower && existing_dir != is_dir);
            if batch_clash || kind_clash {
                let mut dedup = 1;
                loop {
                    dedup += 1;
                    candidate = add_suffix(&base.name, dedup);
                    let lower = fold_case(&candidate);
                    if !taken.contains(&lower) && !existing.iter().any(|(name, _)| *name == lower) {
                        break;
                    }
                }
                fixes.push(if batch_clash {
                    NameFix::BatchClash {
                        candidate: candidate.clone(),
                    }
                } else {
                    // 远端是目录就说明本地撞上的是文件，反之亦然。
                    NameFix::KindClash {
                        existing_is_dir: !*is_dir,
                        candidate: candidate.clone(),
                    }
                });
            }
            taken.push(fold_case(&candidate));
            assignments.push((remote_name.clone(), LocalName::with_fixes(candidate, fixes)));
        }
        Self { assignments }
    }
}

/// 比较两个本地名字用的写法：Windows 不分大小写，折成小写；Linux 区分大小写，原样。
fn fold_case(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_owned()
    }
}

/// `photo.jpg` + 2 → `photo (2).jpg`。
fn add_suffix(name: &str, index: usize) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({index}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({index})"),
    }
}

/// 本地树里的一个文件（上传时用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    /// 本地绝对路径。
    pub path: PathBuf,
    /// 相对根的路径（用 `/` 拼，直接拼到远端目标目录后面）。
    pub relative: String,
    pub size: u64,
}

/// 本地树的遍历结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTree {
    pub files: Vec<LocalFile>,
    /// 跳过的符号链接 / 目录联接（默认不跟着走，列出来给用户看）。
    pub skipped_links: Vec<String>,
    /// 空目录（远端的目录结构要照搬，空目录也算）。
    pub empty_dirs: Vec<String>,
}

/// 遍历本地目录（上传用）：**不跟着符号链接 / 目录联接走**，空目录也记下来。
pub fn walk_local_tree(root: &Path) -> Result<LocalTree, SftpError> {
    let mut tree = LocalTree {
        files: Vec::new(),
        skipped_links: Vec::new(),
        empty_dirs: Vec::new(),
    };
    if !root.is_dir() {
        return Err(SftpError::NotFound {
            path: root.display().to_string(),
        });
    }
    walk_dir(root, root, &mut tree, 0)?;
    Ok(tree)
}

fn walk_dir(root: &Path, dir: &Path, tree: &mut LocalTree, depth: usize) -> Result<(), SftpError> {
    // 深度上限：正常人不会传 100 层，防的是病态目录结构（也可能藏着链接环）。
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        tree.skipped_links.push(format!(
            "{}（目录太深，超过 {MAX_DEPTH} 层）",
            dir.display()
        ));
        return Ok(());
    }
    // 注意：只在真正调系统接口时才套长路径前缀，列表里的路径保持"原样"，
    // 否则 `\?\` 会混进相对路径和给用户的提示里。
    let listing = std::fs::read_dir(long_path(dir))
        .map_err(|err| from_io(err, &dir.display().to_string()))?;
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|err| from_io(err, &dir.display().to_string()))?;
        names.push(entry.file_name());
    }
    names.sort();
    if names.is_empty() && dir != root {
        tree.empty_dirs.push(relative_slash(root, dir));
    }
    for name in names {
        let child = dir.join(&name);
        let meta = match std::fs::symlink_metadata(long_path(&child)) {
            Ok(meta) => meta,
            Err(err) => return Err(from_io(err, &child.display().to_string())),
        };
        // 符号链接 / 目录联接（reparse point）：默认跳过，不跟着递归。
        if is_reparse_point(&meta) || meta.file_type().is_symlink() {
            tree.skipped_links.push(child.display().to_string());
            continue;
        }
        if meta.is_dir() {
            walk_dir(root, &child, tree, depth + 1)?;
        } else if meta.is_file() {
            tree.files.push(LocalFile {
                relative: relative_slash(root, &child),
                path: child,
                size: meta.len(),
            });
        }
        // 其它类型（设备文件等）忽略：Windows 上少见，也不该传。
    }
    Ok(())
}

/// 是不是 reparse point（符号链接或目录联接）。
#[cfg(windows)]
pub fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// 相对路径，用 `/` 拼（远端路径规则）。
fn relative_slash(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rest) => rest
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.display().to_string(),
    }
}

/// 顶层下载的本地落点：`local_dir` + 远端路径最后一段（按 POSIX 规则取名，再按本地系统的规则
/// 规范化：Windows 按 Windows 文件名规则，Linux 只换掉 `/`、NUL 和控制字符）。顶层文件和
/// 递归下载的子项走同一套名字处理；最后还会检查结果确实在 `local_dir` 直接下面，不在就报错、
/// 什么都不写。
pub fn download_target(local_dir: &Path, remote_source: &str) -> Result<PathBuf, SftpError> {
    let trimmed = remote_source.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or_default();
    if name.is_empty() {
        return Err(SftpError::InvalidInput(
            Reason::new(
                reason_keys::SFTP_DOWNLOAD_NO_NAME,
                format!("{remote_source:?} 没有可用的文件名，没法下载到本地"),
            )
            .with("path", format!("{remote_source:?}")),
        ));
    }
    let target = local_dir.join(local_name(name).name);
    ensure_inside(local_dir, &target)?;
    Ok(target)
}

/// 检查 `target` 在 `root` 的里面（不含 `root` 自己）。比较前先做词法规范化（处理 `.`、`..`，
/// Windows 上不分大小写）；`..` 逃出去的都拒绝，Windows 上带盘符、名字里有 `:` 或 `\` 的也拒绝
/// （Linux 上 `:` 和 `\` 只是普通的名字字符）。
pub fn ensure_inside(root: &Path, target: &Path) -> Result<(), SftpError> {
    let outside = || {
        SftpError::InvalidInput(
            Reason::new(
                reason_keys::SFTP_TARGET_OUTSIDE,
                format!(
                    "目标路径 {} 不在所选目录 {} 里，已拒绝",
                    target.display(),
                    root.display()
                ),
            )
            .with("target", target.display())
            .with("root", root.display()),
        )
    };
    // 先规范化（8.3 短路径 / 长路径、大小写、符号链接）：目标还不存在时只解析它已存在的父目录。
    let root_parts = lexical_parts(&crate::pathnorm::normalize(root)).ok_or_else(outside)?;
    let target_parts = lexical_parts(&crate::pathnorm::normalize(target)).ok_or_else(outside)?;
    if target_parts.len() <= root_parts.len() || !target_parts.starts_with(&root_parts) {
        return Err(outside());
    }
    let bad_char = |part: &String| {
        if cfg!(windows) {
            part.contains([':', '\\', '/'])
        } else {
            part.contains('/')
        }
    };
    if target_parts[root_parts.len()..].iter().any(bad_char) {
        return Err(outside());
    }
    Ok(())
}

/// 比较用的键：规范化（`.`、`..`，Windows 上还有大小写）之后的路径。
pub fn lexical_key(path: &Path) -> String {
    match lexical_parts(&crate::pathnorm::normalize(path)) {
        Some(parts) => parts.join("/"),
        None => fold_case(&path.to_string_lossy()),
    }
}

/// 路径拆成段（`.` 去掉、`..` 抵消上一段；`..` 越过开头返回 `None`）。Windows 上折成小写；
/// Linux 上保持原样（`File` 和 `file` 是两个文件，不能共用一把传输锁，也不能当成同一个位置）。
fn lexical_parts(path: &Path) -> Option<Vec<String>> {
    use std::path::Component;
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                parts.push(fold_case(&prefix.as_os_str().to_string_lossy()))
            }
            Component::RootDir => parts.push("\\".to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(name) => parts.push(fold_case(&name.to_string_lossy())),
        }
    }
    Some(parts)
}

/// 用 `from` 原子地替换 `to`（`to` 已存在也行）：任何失败下旧文件或新文件至少有一个完整存在。
#[cfg(windows)]
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, true)
}

/// 把 `from` 改名成 `to`，**`to` 已存在就失败**（`AlreadyExists`），绝不替换。
/// 由系统在改名那一刻判断，不是先检查再改名。
#[cfg(windows)]
pub fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, false)
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, replace: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let wide = |path: &Path| -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (from, to) = (wide(from), wide(to));
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    // SAFETY: 两个缓冲区都以 0 结尾，调用期间一直活着。
    unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) }
        .map_err(|err| std::io::Error::from_raw_os_error(err.code().0 & 0xFFFF))
}

#[cfg(not(windows))]
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    // POSIX 的 rename 本身就是原子替换。
    std::fs::rename(from, to)
}

/// Linux：`renameat2(RENAME_NOREPLACE)`，文件和文件夹都行，由内核在改名那一刻判断目标在不在
/// （同 Windows 的 `MoveFileExW` 不带替换标志）。直接发系统调用，不依赖 libc 里有没有这个封装；
/// 文件系统不支持（`EINVAL`）或内核太老（`ENOSYS`）时退到 [`move_no_replace_fallback`]。
#[cfg(target_os = "linux")]
pub fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    // `<linux/fs.h>` 里的 RENAME_NOREPLACE。
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let as_c = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
    };
    let (c_from, c_to) = (as_c(from)?, as_c(to)?);
    // SAFETY: 两个路径都是以 0 结尾的 C 字符串，调用期间一直活着；其余参数是常量。
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS) => move_no_replace_fallback(from, to),
        _ => Err(err),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    move_no_replace_fallback(from, to)
}

/// 没有 `renameat2` 时的做法。文件：硬链接在目标存在时失败，成功后再删掉原名，没有"先检查再改名"的空窗。
/// 文件夹不能硬链接，只能先看一眼目标在不在再改名——这中间有个很窄的空窗，
/// 但 POSIX 的 `rename` 对「空文件夹目标」会直接替换、对非空的才失败，所以先看是必须的。
#[cfg(unix)]
fn move_no_replace_fallback(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(from)?.is_dir() {
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        return std::fs::rename(from, to);
    }
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

/// 新建一个空文件，**同名的东西已存在就失败**（`AlreadyExists`），绝不截断它。
/// 由系统在创建那一刻判断：先查 metadata 再 `File::create` 中间有空窗，
/// 空窗里别的程序建的同名文件会被清空；查询本身失败也不能当成"不存在"。
pub fn create_new_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(long_path(path))
        .map(|_| ())
}

/// 把 `data` 写成 `target`：先写同目录的临时文件（刷盘），再原子替换原文件。
/// 任何一步失败，原文件保持原样、临时文件被清掉。`target` 不存在也行（等于新建）。
pub fn write_atomic(target: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let tmp = target.with_file_name(format!(
        ".{name}.zssh-tmp-{}-{}",
        std::process::id(),
        // 同一个进程里连着写两次也不会撞名。
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(long_path(&tmp))?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write {
        let _ = std::fs::remove_file(long_path(&tmp));
        return Err(err);
    }
    // 原文件的只读属性 / 权限不用管：替换的是目录项，和编辑器保存的行为一致。
    if let Err(err) = replace_file(&long_path(&tmp), &long_path(target)) {
        let _ = std::fs::remove_file(long_path(&tmp));
        return Err(err);
    }
    Ok(())
}

/// 已分配的盘符（"此电脑"列出来用），一次系统调用拿全。
///
/// 只说明盘符存在，不保证此刻能访问（光驱没盘、网络盘断开）：不在这里逐个探测——
/// 以前对每个盘符做一次 metadata，断开的网络盘会让打开"此电脑"等好几秒。点进去
/// 访问不了时再报错。
#[cfg(windows)]
pub fn logical_drives() -> Vec<char> {
    // SAFETY：没有参数，只返回位掩码。
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    drives_from_mask(mask)
}

#[cfg(not(windows))]
pub fn logical_drives() -> Vec<char> {
    Vec::new()
}

/// GetLogicalDrives 的位掩码 → 盘符（第 0 位是 A:）。
pub fn drives_from_mask(mask: u32) -> Vec<char> {
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| (b'A' + bit) as char)
        .collect()
}

/// 本地路径拼一段远端相对路径（`C:\a` + `x/y` → `C:\a\x\y`）。
pub fn join_local(root: &Path, relative: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in relative.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        out.push(part);
    }
    out
}

/// 需要建出来的本地父目录（下载多层目录时用）。
pub fn ensure_parent(path: &Path) -> Result<(), SftpError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(long_path(parent))
            .map_err(|err| from_io(err, &parent.display().to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn download_target_normalizes_and_stays_inside() {
        let dir = Path::new("C:\\Users\\x\\Downloads");
        for (source, expected) in [
            ("/srv/C:evil.txt", Some("C_evil.txt")),
            ("/srv/CON", Some("_CON")),
            ("/srv/a:b", Some("a_b")),
            ("/srv/..", Some("_")),
            ("/srv/a\\b.txt", Some("a_b.txt")),
            ("/srv/dir/", Some("dir")),
            ("/", None),
            ("", None),
        ] {
            match (download_target(dir, source), expected) {
                (Ok(path), Some(name)) => assert_eq!(path, dir.join(name), "{source}"),
                (Err(_), None) => {}
                (got, want) => panic!("{source:?}: {got:?} vs {want:?}"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn ensure_inside_rejects_escapes() {
        let root = Path::new("C:\\d\\out");
        assert!(ensure_inside(root, Path::new("C:\\d\\out\\a.txt")).is_ok());
        assert!(ensure_inside(root, Path::new("C:\\D\\OUT\\sub\\a.txt")).is_ok());
        for bad in [
            "C:\\d\\out",
            "C:\\d\\out\\..\\x",
            "C:\\d\\other\\a.txt",
            "D:\\d\\out\\a.txt",
            "C:evil.txt",
            "C:\\d\\outside.txt",
        ] {
            assert!(ensure_inside(root, Path::new(bad)).is_err(), "{bad}");
        }
    }

    /// 同一个目录的 8.3 短路径和长路径写法：判断结果必须一致（R-6b）。
    #[cfg(windows)]
    #[test]
    fn short_and_long_spellings_of_a_directory_agree() {
        let dir = crate::test_util::TestDir::new("local-a-rather-long-directory-name");
        let long_root = dir.join("Download Target Directory");
        std::fs::create_dir_all(&long_root).unwrap();
        let long_root = crate::pathnorm::normalize(&long_root);
        let Some(short_root) = crate::pathnorm::short_path(&long_root) else {
            eprintln!("跳过：这个卷上没有 8.3 短名字，造不出短路径写法");
            return;
        };
        assert_ne!(short_root, long_root);

        // 目标还不存在，用另一种写法的根。
        assert!(ensure_inside(&short_root, &long_root.join("a.txt")).is_ok());
        assert!(ensure_inside(&long_root, &short_root.join("a.txt")).is_ok());
        assert!(ensure_inside(&short_root, &long_root.join("sub").join("b.txt")).is_ok());
        // 根自己、根外面的都仍然拒绝。
        assert!(ensure_inside(&short_root, &long_root).is_err());
        assert!(ensure_inside(&short_root, &long_root.parent().unwrap().join("x.txt")).is_err());
        // 同一个目标两种写法 = 同一个键（互斥锁靠它）。
        assert_eq!(
            lexical_key(&short_root.join("same.bin")),
            lexical_key(&long_root.join("same.bin"))
        );
        // 顶层下载落点在两种写法的根下都成功。
        assert!(download_target(&short_root, "/srv/f.txt").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn illegal_characters_are_replaced_with_a_note() {
        let planned = local_name("a:b*c?d\"e<f>g|h\\i/j.txt");
        assert_eq!(planned.name, "a_b_c_d_e_f_g_h_i_j.txt");
        let note = planned.note.expect("要有说明");
        assert!(note.contains("非法字符"), "{note}");
    }

    #[cfg(windows)]
    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        let planned = local_name("报告. ");
        assert_eq!(planned.name, "报告");
        let _ = planned.note.expect("要有说明");
    }

    #[cfg(windows)]
    #[test]
    fn reserved_names_get_a_prefix() {
        let planned = local_name("CON");
        assert_eq!(planned.name, "_CON");
        assert!(planned.note.unwrap().contains("保留名"));
        // 带扩展名也算。
        assert_eq!(local_name("nul.txt").name, "_nul.txt");
        assert_eq!(local_name("Com1.log").name, "_Com1.log");
        // 不是保留名的不动。
        assert_eq!(local_name("console.txt").name, "console.txt");
        assert!(local_name("console.txt").note.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn control_characters_are_replaced() {
        let planned = local_name("bad\u{7}name.txt");
        assert_eq!(planned.name, "bad_name.txt");
        assert!(planned.note.unwrap().contains("\\x07"));
    }

    #[cfg(windows)]
    #[test]
    fn case_conflicts_get_numbered_names() {
        let plan = LocalNamePlan::plan(
            &["A.txt".to_owned(), "a.txt".to_owned(), "b.txt".to_owned()],
            &[],
        );
        assert_eq!(plan.assignments[0].1.name, "A.txt");
        assert_eq!(plan.assignments[1].1.name, "a (2).txt", "只差大小写要避让");
        assert!(plan.assignments[1].1.note.is_some());
        assert_eq!(plan.assignments[2].1.name, "b.txt");
        assert!(plan.assignments[2].1.note.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn a_child_name_must_be_a_plain_windows_file_name() {
        // R-10 M1：带盘符、冒号、分隔符、保留名、结尾点的名字都不能用（不改写，直接拒绝）。
        for bad in [
            "C:evil.txt",
            "D:x",
            "foo:stream",
            "a/b",
            r"a\b",
            ".",
            "..",
            "",
            "   ",
            "CON",
            "nul.txt",
            "trailing.",
            "q?.txt",
        ] {
            assert!(check_child_name(bad).is_err(), "{bad:?} 应该被拒绝");
        }
        assert_eq!(
            check_child_name(" 报告 (副本).txt ").unwrap(),
            "报告 (副本).txt"
        );
        assert_eq!(check_child_name(".bashrc").unwrap(), ".bashrc");
    }

    #[test]
    fn an_existing_local_file_keeps_its_name_for_the_conflict_dialog() {
        // R-9 F13：本地已经有同名文件是"目标已存在"，要走冲突对话框，不能悄悄改名。
        let plan = LocalNamePlan::plan(&["readme.md".to_owned()], &["readme.md".to_owned()]);
        assert_eq!(plan.assignments[0].1.name, "readme.md");
        assert!(plan.assignments[0].1.note.is_none());
        // 只差大小写在 Windows 上也是同一个文件：同样交给冲突处理。
        let plan = LocalNamePlan::plan(&["readme.md".to_owned()], &["README.MD".to_owned()]);
        assert_eq!(plan.assignments[0].1.name, "readme.md");
    }

    #[test]
    fn an_existing_entry_of_the_other_kind_still_gets_a_new_name() {
        // 远端是文件夹、本地是同名文件（或反过来）：写不进去，只能换名字。
        let plan = LocalNamePlan::plan_with_kinds(
            &[("logs".to_owned(), true), ("data".to_owned(), false)],
            &[("logs".to_owned(), false), ("data".to_owned(), true)],
        );
        assert_eq!(plan.assignments[0].1.name, "logs (2)");
        assert_eq!(plan.assignments[1].1.name, "data (2)");
        assert!(plan.assignments[0].1.note.is_some());
        // 同类的已有文件夹：直接下载进去（里面的文件各自走冲突处理）。
        let plan = LocalNamePlan::plan_with_kinds(
            &[("logs".to_owned(), true)],
            &[("logs".to_owned(), true)],
        );
        assert_eq!(plan.assignments[0].1.name, "logs");
    }

    #[cfg(windows)]
    #[test]
    fn numbered_names_skip_existing_files() {
        // 同一批里撞名要加序号，但序号挑出来的名字不能正好是另一份已有文件。
        let plan = LocalNamePlan::plan(
            &["A.txt".to_owned(), "a.txt".to_owned()],
            &["a (2).txt".to_owned()],
        );
        assert_eq!(plan.assignments[0].1.name, "A.txt");
        assert_eq!(plan.assignments[1].1.name, "a (3).txt");
    }

    #[cfg(windows)]
    #[test]
    fn forward_slashes_are_turned_into_backslashes_under_the_prefix() {
        assert_eq!(
            long_path(Path::new("D:/data/logs")).to_string_lossy(),
            "\\\\?\\D:\\data\\logs"
        );
        assert_eq!(
            long_path(Path::new("//server/share/a")).to_string_lossy(),
            "\\\\?\\UNC\\server\\share\\a"
        );
    }

    #[cfg(windows)]
    #[test]
    fn long_paths_get_the_prefix() {
        assert_eq!(
            long_path(Path::new("C:\\a\\b")).to_string_lossy(),
            "\\\\?\\C:\\a\\b"
        );
        assert_eq!(
            long_path(Path::new("\\\\server\\share\\a")).to_string_lossy(),
            "\\\\?\\UNC\\server\\share\\a"
        );
        // 已经有前缀的原样返回；相对路径不加。
        assert_eq!(
            long_path(Path::new("\\\\?\\C:\\a")).to_string_lossy(),
            "\\\\?\\C:\\a"
        );
        assert_eq!(long_path(Path::new("a\\b")).to_string_lossy(), "a\\b");
        assert!(needs_long_path(Path::new(&"x".repeat(250))));
        assert!(!needs_long_path(Path::new("C:\\short")));
    }

    #[test]
    fn create_new_file_never_truncates_an_existing_file() {
        let root = crate::test_util::TestDir::new("create-new");
        let path = root.join("a.txt");
        create_new_file(&path).expect("新建空文件");
        assert_eq!(std::fs::read(&path).unwrap(), b"");

        // 模拟"检查之后、创建之前"别的程序写进来的同名文件：必须报已存在，内容不能被清空。
        std::fs::write(&path, b"someone else").unwrap();
        let err = create_new_file(&path).expect_err("同名文件已存在");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else");

        // 同名目录也一样不能"建成功"。
        std::fs::create_dir(root.join("d")).unwrap();
        assert!(create_new_file(&root.join("d")).is_err());

        // 父目录不存在是 NotFound，不能被当成"已存在"或吞掉。
        let err = create_new_file(&root.join("missing").join("b.txt")).expect_err("父目录不存在");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

        // 超过 260 个字符的路径也能建（和其他本地操作一样过长路径前缀）。
        let deep = root.join("x".repeat(120)).join("y".repeat(120));
        std::fs::create_dir_all(long_path(&deep)).unwrap();
        let long_file = deep.join("z".repeat(40));
        create_new_file(&long_file).expect("长路径新建");
        assert!(std::fs::metadata(long_path(&long_file)).unwrap().is_file());
        assert_eq!(
            create_new_file(&long_file).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn local_tree_walk_collects_files_and_empty_dirs() {
        let root = crate::test_util::TestDir::new("local-tree");
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), b"world!").unwrap();
        std::fs::write(root.join("sub").join("deep").join("c.bin"), vec![0u8; 32]).unwrap();

        let tree = walk_local_tree(&root).expect("遍历本地目录");
        let mut relatives: Vec<&str> = tree.files.iter().map(|f| f.relative.as_str()).collect();
        relatives.sort();
        assert_eq!(relatives, vec!["a.txt", "sub/b.txt", "sub/deep/c.bin"]);
        assert!(tree.empty_dirs.contains(&"empty".to_owned()));
        assert!(tree.skipped_links.is_empty());

        // 相对路径拼回本地路径。
        assert_eq!(
            join_local(&root, "sub/b.txt"),
            root.join("sub").join("b.txt")
        );
    }

    #[test]
    fn local_tree_walk_skips_symlinks_or_junctions() {
        let root = crate::test_util::TestDir::new("local-link");
        let outside = crate::test_util::TestDir::new("local-out");
        std::fs::write(root.join("inside.txt"), b"in").unwrap();
        std::fs::write(outside.join("outside.txt"), b"out").unwrap();

        // Windows：目录联接（junction）不需要管理员权限就能建；Unix：符号链接。
        let junction = root.join("link-to-outside");
        #[cfg(windows)]
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&*outside, &junction).is_ok();
        if !created {
            println!("跳过：这台机器建不了 junction / 符号链接");
            return;
        }

        let tree = walk_local_tree(&root).expect("遍历本地目录");
        let relatives: Vec<&str> = tree.files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(relatives, vec!["inside.txt"], "不跟着联接走");
        assert_eq!(tree.skipped_links.len(), 1, "{:?}", tree.skipped_links);
        assert!(
            tree.skipped_links[0].contains("link-to-outside"),
            "{:?}",
            tree.skipped_links
        );
    }

    /// 移动不覆盖：文件和整个文件夹都能移，目标已存在（文件、文件夹、空文件夹）就失败而且两边都不动。
    #[cfg(unix)]
    #[test]
    fn move_no_replace_moves_files_and_folders_but_never_replaces() {
        let dir = crate::test_util::TestDir::new("move-noreplace");
        let dest = dir.join("dest");
        std::fs::create_dir(&dest).unwrap();

        let file = dir.join("a.txt");
        std::fs::write(&file, b"A").unwrap();
        move_no_replace(&file, &dest.join("a.txt")).expect("文件能移");
        assert!(!file.exists());
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"A");

        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("inner/deeper")).unwrap();
        std::fs::write(tree.join("inner/deeper/x.txt"), b"x").unwrap();
        move_no_replace(&tree, &dest.join("tree")).expect("整个文件夹能移");
        assert!(!tree.exists());
        assert_eq!(
            std::fs::read(dest.join("tree/inner/deeper/x.txt")).unwrap(),
            b"x"
        );

        // 目标已有同名文件：失败，源和目标都原样。
        let again = dir.join("a.txt");
        std::fs::write(&again, b"new").unwrap();
        let err = move_no_replace(&again, &dest.join("a.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&again).unwrap(), b"new");
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"A");

        // 目标已有同名文件夹（非空和空的都不能被替换）。
        let folder = dir.join("tree");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("keep.txt"), b"k").unwrap();
        let err = move_no_replace(&folder, &dest.join("tree")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(folder.join("keep.txt").exists());
        std::fs::create_dir(dest.join("empty")).unwrap();
        let other = dir.join("empty");
        std::fs::create_dir(&other).unwrap();
        let err = move_no_replace(&other, &dest.join("empty")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(other.exists());
    }

    /// 内核 / 文件系统不支持 `renameat2` 时的退路同样不替换、同样能移文件夹。
    #[cfg(unix)]
    #[test]
    fn the_fallback_also_refuses_to_replace_and_moves_folders() {
        let dir = crate::test_util::TestDir::new("move-fallback");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/f"), b"f").unwrap();
        move_no_replace_fallback(&src, &dir.join("moved")).expect("文件夹能移");
        assert!(!src.exists());
        assert_eq!(std::fs::read(dir.join("moved/sub/f")).unwrap(), b"f");
        let f = dir.join("f.txt");
        std::fs::write(&f, b"1").unwrap();
        std::fs::write(dir.join("g.txt"), b"2").unwrap();
        let err = move_no_replace_fallback(&f, &dir.join("g.txt")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&f).unwrap(), b"1");
        assert_eq!(std::fs::read(dir.join("g.txt")).unwrap(), b"2");
        let again = dir.join("moved");
        let err = move_no_replace_fallback(&again, &dir.join("moved")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn drive_letters_come_from_the_bitmask() {
        assert_eq!(drives_from_mask(0), Vec::<char>::new());
        assert_eq!(drives_from_mask(0b1), vec!['A']);
        assert_eq!(drives_from_mask(0b1100), vec!['C', 'D']);
        assert_eq!(drives_from_mask(1 << 25), vec!['Z']);
        assert_eq!(drives_from_mask(u32::MAX).len(), 26, "高位多出来的位不算");
    }

    // ---- Windows 一侧的规则：纯字符串计算，在哪个系统上都能验证，免得 Linux 上改坏了没人发现 ----

    #[test]
    fn the_windows_rules_still_hold_on_every_system() {
        let planned = windows_local_name("a:b*c?d\"e<f>g|h\\i/j.txt");
        assert_eq!(planned.name, "a_b_c_d_e_f_g_h_i_j.txt");
        assert!(planned.note.expect("要有说明").contains("非法字符"));
        let planned = windows_local_name("报告. ");
        assert_eq!(planned.name, "报告");
        assert_eq!(windows_local_name("CON").name, "_CON");
        assert_eq!(windows_local_name("nul.txt").name, "_nul.txt");
        assert_eq!(windows_local_name("console.txt").name, "console.txt");
        let planned = windows_local_name("bad\u{7}name.txt");
        assert_eq!(planned.name, "bad_name.txt");
        assert!(planned.note.unwrap().contains("\\x07"));
        assert_eq!(windows_local_name("..").name, "_");

        assert_eq!(
            windows_long_path(Path::new("C:\\a\\b")).to_string_lossy(),
            "\\\\?\\C:\\a\\b"
        );
        assert_eq!(
            windows_long_path(Path::new("D:/data/logs")).to_string_lossy(),
            "\\\\?\\D:\\data\\logs"
        );
        assert_eq!(
            windows_long_path(Path::new("//server/share/a")).to_string_lossy(),
            "\\\\?\\UNC\\server\\share\\a"
        );
        assert_eq!(
            windows_long_path(Path::new("\\\\?\\C:\\a")).to_string_lossy(),
            "\\\\?\\C:\\a"
        );
        assert_eq!(
            windows_long_path(Path::new("a\\b")).to_string_lossy(),
            "a\\b"
        );
    }

    // ---- Linux / 其他 Unix：POSIX 规则 ----

    /// `//x` 在 POSIX 上是合法路径（等同 `/x`），不能被改写成 `\\?\UNC\x`；任何路径都原样返回。
    #[cfg(unix)]
    #[test]
    fn long_path_leaves_posix_paths_alone() {
        for path in [
            "//x",
            "//server/share/a",
            "/home/me/data",
            "D:/data/logs",
            "C:\\a\\b",
            "\\\\server\\share\\a",
            "\\\\?\\C:\\a",
            "rel/ative",
            "a\\b",
        ] {
            assert_eq!(long_path(Path::new(path)), Path::new(path), "{path}");
        }
        assert!(!needs_long_path(Path::new(&"x".repeat(250))));
        assert!(!needs_long_path(Path::new(&format!(
            "/{}",
            "y".repeat(300)
        ))));
    }

    /// 真的能用 `//x` 这样的路径读写文件（不只是字符串没变）。
    #[cfg(unix)]
    #[test]
    fn double_slash_paths_work_for_real_io() {
        let root = crate::test_util::TestDir::new("local-double-slash");
        let doubled = format!("/{}", root.join("d").display());
        let doubled = Path::new(&doubled);
        ensure_parent(&doubled.join("f.txt")).expect("建父目录");
        create_new_file(&doubled.join("f.txt")).expect("新建文件");
        assert!(root.join("d").join("f.txt").is_file());
        assert!(std::fs::metadata(long_path(&doubled.join("f.txt"))).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn legal_posix_names_are_downloaded_as_they_are() {
        for name in [
            "a:b.log",
            "notes?.txt",
            "CON.txt",
            "nul",
            "name. ",
            "trailing.",
            "a\\b.txt",
            "q\"<>|*.txt",
            "报告 (1).txt",
            ".hidden",
            "...",
            "File",
            "file",
        ] {
            let planned = local_name(name);
            assert_eq!(planned.name, name, "{name:?} 应该原样落盘");
            assert!(planned.note.is_none(), "{name:?} 不该有改名说明");
            assert!(planned.fixes.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn slash_nul_and_control_characters_are_replaced() {
        let planned = local_name("a/b");
        assert_eq!(planned.name, "a_b");
        assert!(planned.note.expect("要有说明").contains("非法字符"));
        let planned = local_name("a\0b");
        assert_eq!(planned.name, "a_b");
        assert!(planned.note.unwrap().contains("\\x00"));
        // 控制字符（含 ESC、换行、DEL）：换成下划线并注明，理由见 `posix_local_name`。
        let planned = local_name("x\u{1b}[31my\nz\u{7f}");
        assert_eq!(planned.name, "x_[31my_z_");
        assert_eq!(planned.fixes.len(), 3, "三种不同的控制字符各记一次");
        let planned = local_name("bad\u{7}name.txt");
        assert_eq!(planned.name, "bad_name.txt");
        assert!(planned.note.unwrap().contains("\\x07"));
        // 同一个字符出现多次只记一次。
        assert_eq!(local_name("a//b/").fixes.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn dot_names_and_empty_names_are_never_used() {
        assert_eq!(local_name(".").name, "_");
        assert_eq!(local_name("..").name, "_");
        assert!(local_name("..").note.is_some());
        assert_eq!(local_name("").name, "_");
        // 三个点以上是普通名字。
        assert_eq!(local_name("...").name, "...");
        assert_eq!(local_name("..a").name, "..a");
    }

    #[cfg(unix)]
    #[test]
    fn download_target_keeps_posix_names_and_stays_inside() {
        let dir = Path::new("/tmp/zt-downloads");
        for (source, expected) in [
            ("/srv/C:evil.txt", Some("C:evil.txt")),
            ("/srv/CON", Some("CON")),
            ("/srv/a:b.log", Some("a:b.log")),
            ("/srv/notes?.txt", Some("notes?.txt")),
            ("/srv/a\\b.txt", Some("a\\b.txt")),
            ("/srv/..", Some("_")),
            ("/srv/.", Some("_")),
            ("/srv/dir/", Some("dir")),
            ("/srv/File", Some("File")),
            ("/", None),
            ("", None),
        ] {
            match (download_target(dir, source), expected) {
                (Ok(path), Some(name)) => assert_eq!(path, dir.join(name), "{source}"),
                (Err(_), None) => {}
                (got, want) => panic!("{source:?}: {got:?} vs {want:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn names_that_differ_only_by_case_do_not_clash() {
        let plan = LocalNamePlan::plan(
            &["File".to_owned(), "file".to_owned(), "FILE".to_owned()],
            &["Other".to_owned()],
        );
        for (remote, local) in &plan.assignments {
            assert_eq!(&local.name, remote, "{remote} 不该被改名");
            assert!(local.note.is_none());
        }
        // 完全相同的名字（同一批里重复）才算撞。
        let plan = LocalNamePlan::plan(&["a.txt".to_owned(), "a.txt".to_owned()], &[]);
        assert_eq!(plan.assignments[0].1.name, "a.txt");
        assert_eq!(plan.assignments[1].1.name, "a (2).txt");
        // 加序号时只避开完全相同的已有名字。
        let plan = LocalNamePlan::plan(
            &["a.txt".to_owned(), "a.txt".to_owned()],
            &["A (2).txt".to_owned(), "a (2).txt".to_owned()],
        );
        assert_eq!(plan.assignments[1].1.name, "a (3).txt");
        let plan = LocalNamePlan::plan(
            &["a.txt".to_owned(), "a.txt".to_owned()],
            &["A (2).txt".to_owned()],
        );
        assert_eq!(
            plan.assignments[1].1.name, "a (2).txt",
            "A (2).txt 是另一个文件"
        );
        // 本地已有的、类型不同的同名条目照旧要换名字（这和 Windows 一致）；大小写不同则不算。
        let plan = LocalNamePlan::plan_with_kinds(
            &[("logs".to_owned(), true), ("Data".to_owned(), true)],
            &[("logs".to_owned(), false), ("data".to_owned(), false)],
        );
        assert_eq!(plan.assignments[0].1.name, "logs (2)");
        assert_eq!(plan.assignments[1].1.name, "Data");
    }

    #[cfg(unix)]
    #[test]
    fn a_child_name_on_posix_only_rejects_slash_nul_and_dots() {
        for ok in [
            "a:b",
            "foo:stream",
            "C:evil.txt",
            r"a\b",
            "CON",
            "nul.txt",
            "trailing.",
            "q?.txt",
            ".bashrc",
        ] {
            assert_eq!(check_child_name(ok).as_deref().ok(), Some(ok), "{ok:?}");
        }
        assert_eq!(
            check_child_name(" 报告 (副本).txt ").unwrap(),
            "报告 (副本).txt"
        );
        for bad in ["a/b", "/", ".", "..", "", "   ", "a\0b"] {
            assert!(check_child_name(bad).is_err(), "{bad:?} 应该被拒绝");
        }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_inside_on_posix_is_case_sensitive_and_allows_colons() {
        let root = Path::new("/tmp/zt-out");
        for ok in [
            "/tmp/zt-out/a.txt",
            "/tmp/zt-out/a:b.log",
            "/tmp/zt-out/a\\b.txt",
            "/tmp/zt-out/sub/notes?.txt",
        ] {
            assert!(ensure_inside(root, Path::new(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "/tmp/zt-out",
            "/tmp/zt-out/../x",
            "/tmp/other/a.txt",
            "/tmp/ZT-OUT/a.txt",
            "/tmp/zt-outside.txt",
        ] {
            assert!(ensure_inside(root, Path::new(bad)).is_err(), "{bad}");
        }
        // 传输锁的键也区分大小写：`File` 和 `file` 不能共用一把锁。
        assert_ne!(
            lexical_key(Path::new("/tmp/zt-out/File")),
            lexical_key(Path::new("/tmp/zt-out/file"))
        );
        assert_eq!(
            lexical_key(Path::new("/tmp/zt-out/./a/../File")),
            lexical_key(Path::new("/tmp/zt-out/File"))
        );
    }

    /// 端到端：落到真实磁盘上，`a:b.log`、`File` 与 `file` 各是一个文件。
    #[cfg(unix)]
    #[test]
    fn posix_download_targets_are_distinct_files_on_disk() {
        let root = crate::test_util::TestDir::new("local-posix-names");
        let mut seen = std::collections::HashSet::new();
        for remote in ["/srv/a:b.log", "/srv/File", "/srv/file", "/srv/a\\b.txt"] {
            let target = download_target(&root, remote).expect("落点");
            create_new_file(&target).expect("两个只差大小写的名字不会撞");
            assert!(seen.insert(target));
        }
        assert_eq!(std::fs::read_dir(&*root).unwrap().count(), 4);
    }
}
