//! Windows 免安装版（zip）的应用内更新：识别"程序旁边有 `portable` 标记"、检查目录能不能写、
//! 从已验签的 zip 里解出新程序并原地替换、失败回滚、下次启动清理 `*.old`、重启时的"等旧进程退出"参数。
//!
//! 整个模块不依赖 Windows 才有的 API，别的平台上也能编译、测试（用临时目录里的假 exe）。
//!
//! 替换的办法：Windows 不能覆盖正在运行的 exe，但可以给它改名。所以先把运行中的 `ZeeTerm.exe` 改名成
//! `ZeeTerm.exe.old`，再把新程序移到原来的位置；`zssh-mcp.exe`、`LICENSE`、`NOTICE` 同样处理。任何一步失败，
//! 已经换过的按相反顺序恢复（新文件删掉、`*.old` 改回原名）。换好之后旧文件尽量当场删（`zssh-mcp.exe`
//! 没在用时能删掉，正在运行的主程序删不掉），删不掉的由下次启动的 [`cleanup_leftovers`] 清理。
//!
//! 新程序放在程序目录里的临时目录 `.zeeterm-update-*` 解开（和目标同一个磁盘，`rename` 才不会变成跨盘复制），
//! 建不了这个目录就说明程序所在目录不可写（Program Files、只读盘……），这时不尝试替换，由调用方引导用户去发布页手动下载。
//!
//! 数据目录 `data` 和 `portable` 标记文件永远不碰：zip 里只有白名单上的文件会被解出来。

use std::ffi::OsString;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::{InstallError, check_file_hash};

/// 程序旁边有这个名字的文件，就是免安装版。
pub const MARKER_FILE: &str = "portable";
/// 免安装版的数据目录：程序所在目录下的这个子目录。
pub const DATA_SUBDIR: &str = "data";
/// zip 里主程序的文件名（和安装包里的一致）。
pub const MAIN_EXE: &str = "ZeeTerm.exe";
/// 内部辅助程序（AI 工具桥）；必须和主程序在同一个目录。
pub const HELPER_EXE: &str = "zssh-mcp.exe";
/// zip 里随程序带的许可文件；有就一起换新。
pub const DOC_FILES: &[&str] = &["LICENSE", "NOTICE"];
/// 被换下来的旧文件的后缀。
pub const OLD_SUFFIX: &str = ".old";
/// 临时解压目录的名字前缀（放在程序所在目录里）。
const STAGE_PREFIX: &str = ".zeeterm-update";
/// 重启参数：新进程先等这个进程号退出（释放单实例互斥体）再继续启动。
pub const WAIT_PID_ARG: &str = "--zeeterm-wait-pid";
/// 单个文件解压后的大小上限（防压缩炸弹）。主程序几十 MB，留足余量。
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// zip 里条目数上限：正常的包只有五个文件。
const MAX_ENTRIES: usize = 64;

// ---------------------------------------------------------------------------
// 识别免安装版、数据目录
// ---------------------------------------------------------------------------

/// 程序所在目录里有没有 `portable` 标记（必须是普通文件，目录或同名的东西不算）。
pub fn has_marker(exe_dir: &Path) -> bool {
    std::fs::metadata(exe_dir.join(MARKER_FILE)).is_ok_and(|meta| meta.is_file())
}

/// 免安装版的数据目录：`<程序所在目录>\data`。`marker_present` 为假（安装版）时返回 `None`。
pub fn data_dir_for(exe_dir: &Path, marker_present: bool) -> Option<PathBuf> {
    marker_present.then(|| exe_dir.join(DATA_SUBDIR))
}

/// 程序所在目录能不能写：试着建一个临时文件再删掉。只读盘、Program Files 里的普通用户都会失败。
pub fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(
        ".zeeterm-write-test-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    if created {
        let _ = std::fs::remove_file(&probe);
    }
    created
}

// ---------------------------------------------------------------------------
// 单实例名字
// ---------------------------------------------------------------------------

/// 数据目录路径的短哈希（16 个十六进制字符）：免安装版的互斥体名和窗口属性名用它，
/// 这样两份不同目录的免安装版、免安装版和安装版可以同时开；同一个目录再开一份仍然会被拦下。
///
/// 路径先规范成不区分大小写、统一用反斜杠、去掉末尾分隔符和 `\\?\` 前缀的形式：Windows 路径大小写不敏感，
/// 同一个目录的不同写法必须得到同一个哈希。
pub fn instance_hash(data_dir: &Path) -> String {
    let mut text = data_dir.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix("\\\\?\\") {
        text = rest.to_owned();
    }
    let text = text.trim_end_matches('\\').to_lowercase();
    crate::sha256_hex(text.as_bytes())[..16].to_owned()
}

// ---------------------------------------------------------------------------
// 安装计划（只做路径 / 哈希校验）
// ---------------------------------------------------------------------------

/// 要装的免安装版 zip。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableInstallRequest {
    pub package: PathBuf,
    /// 更新缓存根目录；zip 必须在这里面。
    pub cache_root: PathBuf,
    pub version: String,
    /// 验签通过的那份字节的 SHA-256（十六进制）。
    pub sha256: String,
}

/// 校验过的计划：包在缓存目录里、是 zip、内容还是验签时的那份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableInstallPlan {
    /// 规范化后的绝对路径。
    pub package: PathBuf,
    pub version: String,
    pub sha256: String,
}

/// 和安装包、Linux 包同样的防线：存在、是文件、扩展名是 `.zip`、在更新缓存目录里、内容的 SHA-256 等于验签时记下的那个。
pub fn plan_portable_install(
    request: &PortableInstallRequest,
) -> Result<PortableInstallPlan, InstallError> {
    if request.version.trim().is_empty() {
        return Err(InstallError::EmptyVersion);
    }
    if !request.package.exists() {
        return Err(InstallError::MissingInstaller);
    }
    if !request.package.is_file() {
        return Err(InstallError::NotAFile);
    }
    let is_zip = request
        .package
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
    if !is_zip {
        return Err(InstallError::NotZip);
    }
    let package = request
        .package
        .canonicalize()
        .map_err(|err| InstallError::Path(err.to_string()))?;
    let cache_root = request
        .cache_root
        .canonicalize()
        .map_err(|err| InstallError::Path(err.to_string()))?;
    if !package.starts_with(&cache_root) {
        return Err(InstallError::OutsideCache);
    }
    check_file_hash(&package, &request.sha256)?;
    Ok(PortableInstallPlan {
        package,
        version: request.version.trim().to_owned(),
        sha256: request.sha256.trim().to_ascii_lowercase(),
    })
}

// ---------------------------------------------------------------------------
// 原地替换
// ---------------------------------------------------------------------------

/// 本机免安装版的布局：正在运行的程序文件，以及它所在的目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableLayout {
    /// 正在运行的程序；被改名成 `*.old` 的就是它，新主程序放到这个路径上（用户改过 exe 文件名也照样对得上）。
    pub exe: PathBuf,
    /// 程序所在目录：`zssh-mcp.exe`、`LICENSE`、`NOTICE` 和临时目录都在这里。
    pub dir: PathBuf,
}

impl PortableLayout {
    pub fn new(exe: &Path) -> Self {
        Self {
            exe: exe.to_path_buf(),
            dir: exe.parent().map(Path::to_path_buf).unwrap_or_default(),
        }
    }
}

/// 替换过程中出错的那一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableStep {
    /// 把新文件解到临时目录。
    Extract,
    /// 清掉上一次留下的 `*.old`。
    ClearOld,
    /// 把旧文件改名成 `*.old`。
    Backup,
    /// 把新文件移到位。
    Replace,
}

impl std::fmt::Display for PortableStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Extract => "解开新文件",
            Self::ClearOld => "清理上次留下的旧文件",
            Self::Backup => "备份旧文件",
            Self::Replace => "替换程序文件",
        })
    }
}

/// 免安装版更新失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortableError {
    /// 程序所在目录写不进去：不是故障，改成手动下载。
    NotWritable(String),
    /// zip 打不开或已损坏。
    BadArchive(String),
    /// zip 里有绝对路径、`..` 之类不安全的条目，或条目多得不正常。
    UnsafeEntry(String),
    /// zip 里缺必要的文件。
    MissingFile(String),
    /// 必要的文件是空的、不是 Windows 程序，或大得离谱。
    BadFile(String),
    /// 文件操作失败。`rolled_back` 说明已经换上的文件是不是都恢复成了旧版本。
    Io {
        step: PortableStep,
        message: String,
        rolled_back: bool,
    },
}

impl PortableError {
    /// 是不是"改成手动下载"而不是报错。
    pub fn is_manual(&self) -> bool {
        matches!(self, Self::NotWritable(_))
    }
}

impl std::fmt::Display for PortableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWritable(err) => write!(f, "程序所在目录不可写：{err}"),
            Self::BadArchive(err) => write!(f, "压缩包损坏或格式不对：{err}"),
            Self::UnsafeEntry(name) => write!(f, "压缩包里有不安全的条目：{name}"),
            Self::MissingFile(name) => write!(f, "压缩包里没有 {name}"),
            Self::BadFile(name) => write!(f, "{name} 是空的、不是 Windows 程序，或者大得不正常"),
            Self::Io {
                step,
                message,
                rolled_back,
            } => {
                write!(f, "{step}失败：{message}")?;
                if *rolled_back {
                    f.write_str("（已恢复成旧版本）")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PortableError {}

/// 替换用到的外部能力；测试里换成会中途失败的假实现。
pub struct PortableContext<'a> {
    /// 把新文件换到位 / 把旧文件改名的 `rename`。
    pub rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
}

/// 产品用的 `rename`。
pub fn rename_system(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// 产品用的入口。
pub fn apply_portable_update_system(
    archive: &Path,
    layout: &PortableLayout,
) -> Result<(), PortableError> {
    apply_portable_update(
        archive,
        layout,
        &PortableContext {
            rename: &rename_system,
        },
    )
}

/// 一个要换的文件：解好的新文件 → 目标位置。
struct Entry {
    source: PathBuf,
    dest: PathBuf,
}

/// 把 zip 里的新版本换到 `layout` 指的位置。成功后新文件到位、旧文件尽量已删（删不掉的叫 `*.old` 留着）；
/// 任何一步失败，已经换过的都回滚，旧版本原样留着。
///
/// 步骤：在程序目录里建临时目录（建不了 = 目录不可写 = [`PortableError::NotWritable`]）→ 查 zip 条目 →
/// 只把白名单上的文件解到临时目录 → 依次"旧文件改名成 `*.old`、新文件移到位"（许可文件在前，`zssh-mcp.exe` 倒数第二，
/// 主程序最后）→ 清理。
pub fn apply_portable_update(
    archive: &Path,
    layout: &PortableLayout,
    ctx: &PortableContext<'_>,
) -> Result<(), PortableError> {
    let stage = unique_name(&layout.dir, STAGE_PREFIX);
    std::fs::create_dir(&stage).map_err(|err| PortableError::NotWritable(err.to_string()))?;
    remove_stale_stages(&layout.dir, &stage);
    let _cleanup = RemoveDirOnDrop(stage.clone());

    let extracted = extract_archive(archive, &stage)?;

    let mut entries = Vec::new();
    for name in DOC_FILES {
        if let Some(source) = extracted.docs.iter().find(|(doc, _)| doc == name) {
            entries.push(Entry {
                source: source.1.clone(),
                dest: layout.dir.join(name),
            });
        }
    }
    entries.push(Entry {
        source: extracted.helper,
        dest: layout.dir.join(HELPER_EXE),
    });
    entries.push(Entry {
        source: extracted.main,
        dest: layout.exe.clone(),
    });
    apply_entries(&entries, ctx.rename)
}

/// 解出来的文件。
struct Extracted {
    main: PathBuf,
    helper: PathBuf,
    docs: Vec<(&'static str, PathBuf)>,
}

fn is_name(candidate: &str, wanted: &str) -> bool {
    candidate.eq_ignore_ascii_case(wanted)
}

/// 检查 zip 的条目、把白名单上的文件平铺解到 `stage`。其余条目（包括 `portable` 标记）一律不解。
fn extract_archive(archive: &Path, stage: &Path) -> Result<Extracted, PortableError> {
    let file =
        std::fs::File::open(archive).map_err(|err| PortableError::BadArchive(err.to_string()))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|err| PortableError::BadArchive(err.to_string()))?;
    if zip.len() > MAX_ENTRIES {
        return Err(PortableError::UnsafeEntry(format!("{} entries", zip.len())));
    }

    let mut main = None;
    let mut helper = None;
    let mut docs: Vec<(&'static str, PathBuf)> = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|err| PortableError::BadArchive(err.to_string()))?;
        let raw_name = entry
            .name()
            .map(|name| name.into_owned())
            .unwrap_or_default();
        // 绝对路径、`..`、盘符、软链接：正规的包不会有，出现就整包拒绝。zip 库的 `enclosed_name` 会把开头的
        // 斜杠当成相对路径放过，所以原始名字这里再查一遍。
        let Some(relative) = entry.enclosed_name() else {
            return Err(PortableError::UnsafeEntry(raw_name));
        };
        let raw_is_unsafe = raw_name.starts_with(['/', '\\'])
            || raw_name.contains(':')
            || raw_name.split(['/', '\\']).any(|part| part == "..");
        if raw_is_unsafe || entry.is_symlink() {
            return Err(PortableError::UnsafeEntry(raw_name));
        }
        if entry.is_dir() {
            continue;
        }
        // 只认顶层的普通文件；带子目录的条目不是我们要的，忽略。
        let mut parts = relative.components();
        let (Some(std::path::Component::Normal(name)), None) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        let target_name = if is_name(&name, MAIN_EXE) {
            MAIN_EXE
        } else if is_name(&name, HELPER_EXE) {
            HELPER_EXE
        } else if let Some(doc) = DOC_FILES.iter().find(|doc| is_name(&name, doc)) {
            doc
        } else {
            continue;
        };
        if entry.size() > MAX_FILE_BYTES {
            return Err(PortableError::BadFile(target_name.to_owned()));
        }
        let out_path = stage.join(target_name);
        let extract_failed = |err: io::Error| PortableError::Io {
            step: PortableStep::Extract,
            message: err.to_string(),
            rolled_back: false,
        };
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out_path)
            .map_err(extract_failed)?;
        // 声明的大小不可信：按实际读到的字节数再卡一次上限。
        let copied = io::copy(&mut entry.by_ref().take(MAX_FILE_BYTES + 1), &mut out)
            .map_err(|err| PortableError::BadArchive(err.to_string()))?;
        out.flush()
            .and_then(|()| out.sync_all())
            .map_err(extract_failed)?;
        drop(out);
        if copied > MAX_FILE_BYTES {
            return Err(PortableError::BadFile(target_name.to_owned()));
        }
        let is_exe = target_name == MAIN_EXE || target_name == HELPER_EXE;
        if is_exe && (copied == 0 || !looks_like_windows_exe(&out_path)) {
            return Err(PortableError::BadFile(target_name.to_owned()));
        }
        match target_name {
            MAIN_EXE => main = Some(out_path),
            HELPER_EXE => helper = Some(out_path),
            doc => {
                if let Some(doc) = DOC_FILES.iter().find(|candidate| **candidate == doc) {
                    docs.push((doc, out_path));
                }
            }
        }
    }
    Ok(Extracted {
        main: main.ok_or_else(|| PortableError::MissingFile(MAIN_EXE.to_owned()))?,
        helper: helper.ok_or_else(|| PortableError::MissingFile(HELPER_EXE.to_owned()))?,
        docs,
    })
}

/// 开头是 `MZ`：Windows 程序（PE）的魔数。挡住"包里的 exe 其实是别的东西"这种明显的错误。
fn looks_like_windows_exe(path: &Path) -> bool {
    let mut head = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && head == *b"MZ"
}

/// 一个已经处理过的目标，回滚要用。
struct Applied {
    dest: PathBuf,
    /// 旧文件改名后的位置；原来没有这个文件时为 `None`。
    backup: Option<PathBuf>,
    /// 新文件已经移到 `dest` 了吗（旧文件改名成功、新文件还没移上去时为假）。
    placed: bool,
}

/// `dest` 的备份名：`dest` 加 `.old`（`ZeeTerm.exe` → `ZeeTerm.exe.old`）。
pub fn old_name(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(OLD_SUFFIX);
    PathBuf::from(name)
}

/// 依次"旧文件改名成 `*.old`、新文件移到位"；出错就把已处理的全部恢复，成功后尽量删掉 `*.old`。
fn apply_entries(
    entries: &[Entry],
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), PortableError> {
    let mut applied: Vec<Applied> = Vec::new();
    for entry in entries {
        if let Err((step, message)) = apply_one(entry, rename, &mut applied) {
            let rolled_back = rollback(&applied);
            return Err(PortableError::Io {
                step,
                message,
                rolled_back,
            });
        }
    }
    // 旧文件尽量当场删：正在运行的主程序删不掉（下次启动清理），没在用的 `zssh-mcp.exe` 删得掉。
    for done in &applied {
        if let Some(backup) = &done.backup {
            let _ = std::fs::remove_file(backup);
        }
    }
    Ok(())
}

fn apply_one(
    entry: &Entry,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
    applied: &mut Vec<Applied>,
) -> Result<(), (PortableStep, String)> {
    let backup = match std::fs::symlink_metadata(&entry.dest) {
        Ok(meta) if meta.is_file() => {
            let backup = old_name(&entry.dest);
            // 上次的 `*.old` 还在（比如还被占用着没删掉）：先清掉，改名才不会撞上。
            match std::fs::remove_file(&backup) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err((PortableStep::ClearOld, err.to_string())),
            }
            rename(&entry.dest, &backup).map_err(|err| (PortableStep::Backup, err.to_string()))?;
            Some(backup)
        }
        Ok(_) => {
            return Err((
                PortableStep::Backup,
                format!("{} is not a regular file", entry.dest.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err((PortableStep::Backup, err.to_string())),
    };
    applied.push(Applied {
        dest: entry.dest.clone(),
        backup,
        placed: false,
    });
    rename(&entry.source, &entry.dest).map_err(|err| (PortableStep::Replace, err.to_string()))?;
    if let Some(last) = applied.last_mut() {
        last.placed = true;
    }
    Ok(())
}

/// 按相反顺序恢复：换上去的新文件删掉，`*.old` 改回原名；原来没有的文件只删新的。
/// 全部恢复成功返回 `true`。回滚用真正的 `rename`，不经过测试注入的那个（它是用来模拟"换上去时失败"的）。
fn rollback(applied: &[Applied]) -> bool {
    let mut all_ok = true;
    for done in applied.iter().rev() {
        if done.placed && std::fs::remove_file(&done.dest).is_err() {
            all_ok = false;
            continue;
        }
        if let Some(backup) = &done.backup
            && std::fs::rename(backup, &done.dest).is_err()
        {
            all_ok = false;
        }
    }
    all_ok
}

// ---------------------------------------------------------------------------
// 启动时清理
// ---------------------------------------------------------------------------

/// 上次更新留下的东西：`*.old`（程序运行时删不掉的旧主程序）和中途被杀留下的临时目录。
/// 启动时调一次，失败（还被占用着）就留到下次。返回删掉的路径，给日志和测试看。
pub fn cleanup_leftovers(layout: &PortableLayout) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let mut candidates = vec![
        old_name(&layout.exe),
        old_name(&layout.dir.join(HELPER_EXE)),
    ];
    candidates.extend(DOC_FILES.iter().map(|doc| old_name(&layout.dir.join(doc))));
    // 用户改过 exe 文件名时，安装包里的名字对应的 `.old` 也要顾到。
    candidates.push(old_name(&layout.dir.join(MAIN_EXE)));
    candidates.sort();
    candidates.dedup();
    for path in candidates {
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed.extend(remove_stale_stages(&layout.dir, Path::new("")));
    removed
}

/// 上次更新中途被杀（断电、崩溃）留下的临时目录：超过一天的清掉。`current` 和更新鲜的不动。
fn remove_stale_stages(dir: &Path, current: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_stage = entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{STAGE_PREFIX}-"));
        let stale = entry
            .metadata()
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if is_stage && stale && path != current && std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

fn unique_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos())
}

/// 在 `dir` 下起一个不会撞车的名字（进程号 + 纳秒）。
fn unique_name(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        unique_suffix()
    ))
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// 重启
// ---------------------------------------------------------------------------

/// 重启时给新进程的参数：先是 `--zeeterm-wait-pid=<旧进程号>`，再是原来的参数（去掉可能带着的旧等待参数，
/// 重启两次不会越积越多）。新进程在抢单实例互斥体之前先等旧进程退出，否则会被当成"已经有一个在运行"而退出。
pub fn restart_args(wait_pid: u32, args: &[String]) -> Vec<OsString> {
    let mut out = vec![OsString::from(format!("{WAIT_PID_ARG}={wait_pid}"))];
    out.extend(without_wait_arg(args).into_iter().map(OsString::from));
    out
}

/// 去掉等待参数后的参数列表。
pub fn without_wait_arg(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|arg| !is_wait_arg(arg))
        .cloned()
        .collect()
}

fn is_wait_arg(arg: &str) -> bool {
    arg.strip_prefix(WAIT_PID_ARG)
        .is_some_and(|rest| rest.starts_with('='))
}

/// 从命令行里找 `--zeeterm-wait-pid=<数字>`（第一个参数是程序名，跳过）。进程号为 0 或不是数字当没有。
pub fn wait_pid_from_args(args: impl IntoIterator<Item = OsString>) -> Option<u32> {
    args.into_iter().skip(1).find_map(|arg| {
        let arg = arg.to_str()?;
        let rest = arg.strip_prefix(WAIT_PID_ARG)?.strip_prefix('=')?;
        rest.parse::<u32>().ok().filter(|pid| *pid != 0)
    })
}

/// 起一个脱离当前进程的子进程，不等它。标准输入输出全接空，不弹控制台窗口。
pub fn spawn_detached(program: &Path, args: &[OsString]) -> io::Result<()> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        command.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests;
