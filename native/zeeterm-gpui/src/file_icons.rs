//! 文件列表里按文件类型显示的图标（V2-U52）。
//!
//! 两层：
//! - **内置分类图标**（三个平台都有）：按文件名把文件分成十几类（压缩包再细分），每类一个 Lucide 图标 + 一个固定颜色
//!   （不随皮肤变，浅色和深色皮肤下都看得清；做法同 [`crate::icons::folder_icon`]）；分类不认识的仍是原来的灰色文件图标。
//! - **Windows 系统图标**（仅 Windows）：像 Xftp 一样按扩展名向外壳要关联图标，后台查询 + 进程级缓存，
//!   查好之前、查不到、或者系统只给了「通用白纸」时都用上面的内置图标，见 [`system`]。
//!
//! 分类只看文件名，是纯函数（[`classify`]），不碰文件系统，远端和本地共用。
//!
//! ## 分类规则（按顺序，先命中先算）
//!
//! 1. 整名匹配的特殊文件名（不分大小写）：`Dockerfile`、`Makefile` 等构建文件算「代码」；`.bashrc`、`.zshrc`、`.profile`、
//!    `.gitignore`、`.env`（含 `.env.local` 这类）等点开头的设置文件算「配置」；`id_rsa`、`id_ed25519`、`authorized_keys`
//!    这类无扩展名的密钥文件算「证书和密钥」；`LICENSE`、`COPYING`、`NOTICE`、`AUTHORS`、`CHANGELOG`、`README` 无扩展名时算「文本」。
//! 2. 复合扩展名：`.tar.gz` `.tar.xz` `.tar.bz2` `.tar.zst` 等都算「tar 系压缩包」（和单独的 `.gz` 区分开）。
//! 3. 最后一个扩展名查表（见 [`kind_of_extension`]）。**脚本**只收「写完直接交给 shell / 批处理 / PowerShell 执行」的
//!    （sh bash zsh fish ksh csh ps1 psm1 bat cmd vbs），`py` `rb` `pl` `lua` `php` 这类编程语言归「代码」。
//! 4. 其余（没有扩展名、点开头但不认识、扩展名不认识）都是「普通文件」。
//! 5. 名字里以 `README` / `LICENSE` / `COPYING` / `NOTICE` / `CHANGELOG` 开头、扩展名又不认识的（`README.rst`、`LICENSE-MIT`）算「文本」。

use ::gpui::{AnyElement, IntoElement as _, Styled as _};
use gpui_kit::component::{Icon, IconName};

use crate::app_theme::Tokens;
use crate::icons::Ico;

#[cfg(any(windows, test))]
mod cache;
#[cfg(windows)]
mod system;

/// 压缩包的细分：能一眼区分的几种（颜色不同，tar 系再换一个图标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArchiveFlavor {
    /// zip 和同格式的 jar / war / whl 等。
    Zip,
    /// tar 和各种 tar.xxx / tgz。
    Tar,
    SevenZip,
    Rar,
    /// 单文件压缩：gz / xz / bz2 / zst 等（不含 tar.xxx）。
    Compressed,
    /// 其他压缩 / 打包格式：cab、arj、lzh、cpio 等。
    Other,
}

/// 文件类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Archive(ArchiveFlavor),
    Image,
    Video,
    Audio,
    Code,
    Script,
    /// 纯文本、日志。
    Text,
    /// pdf、Word、markdown 等文档。
    Document,
    Spreadsheet,
    Presentation,
    /// 可执行文件和安装包。
    Executable,
    DiskImage,
    /// 证书和密钥。
    Certificate,
    Config,
    Database,
    Font,
    /// 普通文件：没有扩展名 / 不认识。
    Plain,
}

/// 整名匹配的特殊文件名（小写）。
const SPECIAL_NAMES: &[(&str, FileKind)] = &[
    ("dockerfile", FileKind::Code),
    ("containerfile", FileKind::Code),
    ("makefile", FileKind::Code),
    ("gnumakefile", FileKind::Code),
    ("cmakelists.txt", FileKind::Code),
    ("jenkinsfile", FileKind::Code),
    ("vagrantfile", FileKind::Code),
    ("rakefile", FileKind::Code),
    ("gemfile", FileKind::Code),
    ("procfile", FileKind::Config),
    ("license", FileKind::Text),
    ("copying", FileKind::Text),
    ("notice", FileKind::Text),
    ("authors", FileKind::Text),
    ("changelog", FileKind::Text),
    ("readme", FileKind::Text),
    (".bashrc", FileKind::Config),
    (".bash_profile", FileKind::Config),
    (".bash_logout", FileKind::Config),
    (".bash_aliases", FileKind::Config),
    (".bash_history", FileKind::Text),
    (".zshrc", FileKind::Config),
    (".zprofile", FileKind::Config),
    (".zshenv", FileKind::Config),
    (".zlogin", FileKind::Config),
    (".zsh_history", FileKind::Text),
    (".profile", FileKind::Config),
    (".inputrc", FileKind::Config),
    (".vimrc", FileKind::Config),
    (".tmux.conf", FileKind::Config),
    (".gitignore", FileKind::Config),
    (".gitattributes", FileKind::Config),
    (".gitmodules", FileKind::Config),
    (".gitconfig", FileKind::Config),
    (".dockerignore", FileKind::Config),
    (".editorconfig", FileKind::Config),
    (".npmrc", FileKind::Config),
    (".env", FileKind::Config),
    ("id_rsa", FileKind::Certificate),
    ("id_dsa", FileKind::Certificate),
    ("id_ecdsa", FileKind::Certificate),
    ("id_ed25519", FileKind::Certificate),
    ("id_ecdsa_sk", FileKind::Certificate),
    ("id_ed25519_sk", FileKind::Certificate),
    ("authorized_keys", FileKind::Certificate),
    ("known_hosts", FileKind::Config),
];

/// 文件名以它们开头、扩展名又查不到类型时算「文本」。
const TEXT_PREFIXES: &[&str] = &[
    "readme",
    "license",
    "licence",
    "copying",
    "notice",
    "changelog",
];

/// 复合扩展名（含前导点）：都算 tar 系压缩包。
const TAR_SUFFIXES: &[&str] = &[
    ".tar.gz",
    ".tar.xz",
    ".tar.bz2",
    ".tar.zst",
    ".tar.zstd",
    ".tar.lz4",
    ".tar.lz",
    ".tar.lzma",
    ".tar.z",
];

/// 单个扩展名（小写，不带点）→ 类型。同一个扩展名只出现一次（有测试核对）。
const EXTENSIONS: &[(&str, FileKind)] = &[
    // 压缩包
    ("zip", FileKind::Archive(ArchiveFlavor::Zip)),
    ("zipx", FileKind::Archive(ArchiveFlavor::Zip)),
    ("jar", FileKind::Archive(ArchiveFlavor::Zip)),
    ("war", FileKind::Archive(ArchiveFlavor::Zip)),
    ("ear", FileKind::Archive(ArchiveFlavor::Zip)),
    ("whl", FileKind::Archive(ArchiveFlavor::Zip)),
    ("nupkg", FileKind::Archive(ArchiveFlavor::Zip)),
    ("xpi", FileKind::Archive(ArchiveFlavor::Zip)),
    ("tar", FileKind::Archive(ArchiveFlavor::Tar)),
    ("tgz", FileKind::Archive(ArchiveFlavor::Tar)),
    ("tbz", FileKind::Archive(ArchiveFlavor::Tar)),
    ("tbz2", FileKind::Archive(ArchiveFlavor::Tar)),
    ("txz", FileKind::Archive(ArchiveFlavor::Tar)),
    ("tzst", FileKind::Archive(ArchiveFlavor::Tar)),
    ("tlz", FileKind::Archive(ArchiveFlavor::Tar)),
    ("taz", FileKind::Archive(ArchiveFlavor::Tar)),
    ("7z", FileKind::Archive(ArchiveFlavor::SevenZip)),
    ("rar", FileKind::Archive(ArchiveFlavor::Rar)),
    ("gz", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("xz", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("bz2", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("zst", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("zstd", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("lz", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("lz4", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("lzma", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("lzo", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("z", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("br", FileKind::Archive(ArchiveFlavor::Compressed)),
    ("cab", FileKind::Archive(ArchiveFlavor::Other)),
    ("arj", FileKind::Archive(ArchiveFlavor::Other)),
    ("lzh", FileKind::Archive(ArchiveFlavor::Other)),
    ("lha", FileKind::Archive(ArchiveFlavor::Other)),
    ("cpio", FileKind::Archive(ArchiveFlavor::Other)),
    ("ace", FileKind::Archive(ArchiveFlavor::Other)),
    ("sit", FileKind::Archive(ArchiveFlavor::Other)),
    ("zpaq", FileKind::Archive(ArchiveFlavor::Other)),
    ("wim", FileKind::Archive(ArchiveFlavor::Other)),
    // 图片
    ("png", FileKind::Image),
    ("jpg", FileKind::Image),
    ("jpeg", FileKind::Image),
    ("jfif", FileKind::Image),
    ("gif", FileKind::Image),
    ("bmp", FileKind::Image),
    ("webp", FileKind::Image),
    ("svg", FileKind::Image),
    ("ico", FileKind::Image),
    ("tif", FileKind::Image),
    ("tiff", FileKind::Image),
    ("heic", FileKind::Image),
    ("heif", FileKind::Image),
    ("avif", FileKind::Image),
    ("jxl", FileKind::Image),
    ("apng", FileKind::Image),
    ("raw", FileKind::Image),
    ("cr2", FileKind::Image),
    ("nef", FileKind::Image),
    ("arw", FileKind::Image),
    ("dng", FileKind::Image),
    ("psd", FileKind::Image),
    ("xcf", FileKind::Image),
    // 视频（没有 ts：它更常见的是 TypeScript）
    ("mp4", FileKind::Video),
    ("m4v", FileKind::Video),
    ("mkv", FileKind::Video),
    ("avi", FileKind::Video),
    ("mov", FileKind::Video),
    ("wmv", FileKind::Video),
    ("flv", FileKind::Video),
    ("webm", FileKind::Video),
    ("mpg", FileKind::Video),
    ("mpeg", FileKind::Video),
    ("3gp", FileKind::Video),
    ("rmvb", FileKind::Video),
    ("vob", FileKind::Video),
    ("ogv", FileKind::Video),
    ("m2ts", FileKind::Video),
    ("mts", FileKind::Video),
    // 音频
    ("mp3", FileKind::Audio),
    ("wav", FileKind::Audio),
    ("flac", FileKind::Audio),
    ("aac", FileKind::Audio),
    ("ogg", FileKind::Audio),
    ("oga", FileKind::Audio),
    ("m4a", FileKind::Audio),
    ("wma", FileKind::Audio),
    ("opus", FileKind::Audio),
    ("aiff", FileKind::Audio),
    ("aif", FileKind::Audio),
    ("mid", FileKind::Audio),
    ("midi", FileKind::Audio),
    ("ape", FileKind::Audio),
    ("amr", FileKind::Audio),
    ("mka", FileKind::Audio),
    // 脚本：交给 shell / 批处理 / PowerShell 直接执行的
    ("sh", FileKind::Script),
    ("bash", FileKind::Script),
    ("zsh", FileKind::Script),
    ("fish", FileKind::Script),
    ("ksh", FileKind::Script),
    ("csh", FileKind::Script),
    ("tcsh", FileKind::Script),
    ("ps1", FileKind::Script),
    ("psm1", FileKind::Script),
    ("bat", FileKind::Script),
    ("cmd", FileKind::Script),
    ("vbs", FileKind::Script),
    // 代码（含 py 等编程语言）
    ("rs", FileKind::Code),
    ("c", FileKind::Code),
    ("h", FileKind::Code),
    ("cpp", FileKind::Code),
    ("cc", FileKind::Code),
    ("cxx", FileKind::Code),
    ("hpp", FileKind::Code),
    ("hh", FileKind::Code),
    ("hxx", FileKind::Code),
    ("cs", FileKind::Code),
    ("java", FileKind::Code),
    ("kt", FileKind::Code),
    ("kts", FileKind::Code),
    ("scala", FileKind::Code),
    ("go", FileKind::Code),
    ("js", FileKind::Code),
    ("mjs", FileKind::Code),
    ("cjs", FileKind::Code),
    ("jsx", FileKind::Code),
    ("ts", FileKind::Code),
    ("tsx", FileKind::Code),
    ("py", FileKind::Code),
    ("pyw", FileKind::Code),
    ("rb", FileKind::Code),
    ("pl", FileKind::Code),
    ("pm", FileKind::Code),
    ("php", FileKind::Code),
    ("lua", FileKind::Code),
    ("swift", FileKind::Code),
    ("m", FileKind::Code),
    ("mm", FileKind::Code),
    ("dart", FileKind::Code),
    ("zig", FileKind::Code),
    ("nim", FileKind::Code),
    ("ex", FileKind::Code),
    ("exs", FileKind::Code),
    ("erl", FileKind::Code),
    ("hs", FileKind::Code),
    ("ml", FileKind::Code),
    ("clj", FileKind::Code),
    ("jl", FileKind::Code),
    ("r", FileKind::Code),
    ("asm", FileKind::Code),
    ("html", FileKind::Code),
    ("htm", FileKind::Code),
    ("css", FileKind::Code),
    ("scss", FileKind::Code),
    ("sass", FileKind::Code),
    ("less", FileKind::Code),
    ("vue", FileKind::Code),
    ("svelte", FileKind::Code),
    ("tf", FileKind::Code),
    ("proto", FileKind::Code),
    ("graphql", FileKind::Code),
    ("gql", FileKind::Code),
    ("cmake", FileKind::Code),
    ("gradle", FileKind::Code),
    ("mk", FileKind::Code),
    ("diff", FileKind::Code),
    ("patch", FileKind::Code),
    ("ipynb", FileKind::Code),
    // 纯文本和日志
    ("txt", FileKind::Text),
    ("text", FileKind::Text),
    ("log", FileKind::Text),
    ("out", FileKind::Text),
    ("err", FileKind::Text),
    ("nfo", FileKind::Text),
    ("rst", FileKind::Text),
    ("adoc", FileKind::Text),
    // 文档
    ("pdf", FileKind::Document),
    ("doc", FileKind::Document),
    ("docx", FileKind::Document),
    ("docm", FileKind::Document),
    ("odt", FileKind::Document),
    ("rtf", FileKind::Document),
    ("md", FileKind::Document),
    ("markdown", FileKind::Document),
    ("epub", FileKind::Document),
    ("mobi", FileKind::Document),
    ("djvu", FileKind::Document),
    ("pages", FileKind::Document),
    ("wps", FileKind::Document),
    ("xps", FileKind::Document),
    // 表格
    ("xls", FileKind::Spreadsheet),
    ("xlsx", FileKind::Spreadsheet),
    ("xlsm", FileKind::Spreadsheet),
    ("xlsb", FileKind::Spreadsheet),
    ("csv", FileKind::Spreadsheet),
    ("tsv", FileKind::Spreadsheet),
    ("ods", FileKind::Spreadsheet),
    ("numbers", FileKind::Spreadsheet),
    // 演示（没有 key：按卡里的定义它是密钥）
    ("ppt", FileKind::Presentation),
    ("pptx", FileKind::Presentation),
    ("pptm", FileKind::Presentation),
    ("pps", FileKind::Presentation),
    ("ppsx", FileKind::Presentation),
    ("odp", FileKind::Presentation),
    // 可执行和安装包
    ("exe", FileKind::Executable),
    ("msi", FileKind::Executable),
    ("msix", FileKind::Executable),
    ("msixbundle", FileKind::Executable),
    ("appx", FileKind::Executable),
    ("appxbundle", FileKind::Executable),
    ("deb", FileKind::Executable),
    ("rpm", FileKind::Executable),
    ("apk", FileKind::Executable),
    ("xapk", FileKind::Executable),
    ("aab", FileKind::Executable),
    ("ipa", FileKind::Executable),
    ("appimage", FileKind::Executable),
    ("dmg", FileKind::Executable),
    ("pkg", FileKind::Executable),
    ("snap", FileKind::Executable),
    ("run", FileKind::Executable),
    ("dll", FileKind::Executable),
    ("so", FileKind::Executable),
    ("dylib", FileKind::Executable),
    // 磁盘镜像
    ("iso", FileKind::DiskImage),
    ("img", FileKind::DiskImage),
    ("qcow2", FileKind::DiskImage),
    ("qcow", FileKind::DiskImage),
    ("vmdk", FileKind::DiskImage),
    ("vhd", FileKind::DiskImage),
    ("vhdx", FileKind::DiskImage),
    ("vdi", FileKind::DiskImage),
    ("ova", FileKind::DiskImage),
    // 证书和密钥
    ("pem", FileKind::Certificate),
    ("crt", FileKind::Certificate),
    ("cer", FileKind::Certificate),
    ("der", FileKind::Certificate),
    ("csr", FileKind::Certificate),
    ("key", FileKind::Certificate),
    ("pub", FileKind::Certificate),
    ("p12", FileKind::Certificate),
    ("pfx", FileKind::Certificate),
    ("p7b", FileKind::Certificate),
    ("p7c", FileKind::Certificate),
    ("ppk", FileKind::Certificate),
    ("jks", FileKind::Certificate),
    ("keystore", FileKind::Certificate),
    ("gpg", FileKind::Certificate),
    ("pgp", FileKind::Certificate),
    ("asc", FileKind::Certificate),
    ("kdbx", FileKind::Certificate),
    // 配置
    ("conf", FileKind::Config),
    ("cfg", FileKind::Config),
    ("cnf", FileKind::Config),
    ("ini", FileKind::Config),
    ("yaml", FileKind::Config),
    ("yml", FileKind::Config),
    ("toml", FileKind::Config),
    ("json", FileKind::Config),
    ("json5", FileKind::Config),
    ("jsonc", FileKind::Config),
    ("xml", FileKind::Config),
    ("properties", FileKind::Config),
    ("service", FileKind::Config),
    ("timer", FileKind::Config),
    ("socket", FileKind::Config),
    ("mount", FileKind::Config),
    ("target", FileKind::Config),
    ("plist", FileKind::Config),
    ("reg", FileKind::Config),
    ("env", FileKind::Config),
    ("desktop", FileKind::Config),
    ("tfvars", FileKind::Config),
    ("hcl", FileKind::Config),
    ("editorconfig", FileKind::Config),
    // 数据库
    ("db", FileKind::Database),
    ("sqlite", FileKind::Database),
    ("sqlite3", FileKind::Database),
    ("db3", FileKind::Database),
    ("sql", FileKind::Database),
    ("mdb", FileKind::Database),
    ("accdb", FileKind::Database),
    ("dbf", FileKind::Database),
    ("ldb", FileKind::Database),
    ("frm", FileKind::Database),
    ("ibd", FileKind::Database),
    ("myd", FileKind::Database),
    ("myi", FileKind::Database),
    // 字体
    ("ttf", FileKind::Font),
    ("otf", FileKind::Font),
    ("ttc", FileKind::Font),
    ("woff", FileKind::Font),
    ("woff2", FileKind::Font),
    ("eot", FileKind::Font),
    ("pfb", FileKind::Font),
];

/// 一个扩展名（任意大小写、不带点）对应的类型；不认识返回 `None`。
pub fn kind_of_extension(ext: &str) -> Option<FileKind> {
    let lower = ext.to_lowercase();
    EXTENSIONS
        .iter()
        .find(|(known, _)| *known == lower)
        .map(|(_, kind)| *kind)
}

/// 按文件名（不含路径）分类，规则见模块文档。
pub fn classify(name: &str) -> FileKind {
    let lower = name.to_lowercase();
    if let Some((_, kind)) = SPECIAL_NAMES.iter().find(|(special, _)| *special == lower) {
        return *kind;
    }
    // `.env.local`、`.env.production` 这类：点开头的 `.env.` 前缀。
    if lower.starts_with(".env.") {
        return FileKind::Config;
    }
    // `id_rsa.pub` 走扩展名（pub），`id_ed25519_sk` 等已在整名表里。
    if TAR_SUFFIXES
        .iter()
        .any(|suffix| lower.len() > suffix.len() && lower.ends_with(suffix))
    {
        return FileKind::Archive(ArchiveFlavor::Tar);
    }
    // 最后一个点之后是扩展名；点在开头（`.bashrc` 这类）或结尾的不算有扩展名。
    if let Some(dot) = lower.rfind('.')
        && dot > 0
        && dot + 1 < lower.len()
        && let Some(kind) = kind_of_extension(&lower[dot + 1..])
    {
        return kind;
    }
    if !lower.starts_with('.') && TEXT_PREFIXES.iter().any(|prefix| lower.starts_with(prefix)) {
        return FileKind::Text;
    }
    FileKind::Plain
}

/// 一类文件的内置图标和固定颜色（`None` = 用皮肤的灰色，只有普通文件）。
pub fn builtin(kind: FileKind) -> (Ico, Option<u32>) {
    use ArchiveFlavor as A;
    match kind {
        FileKind::Archive(A::Zip) => (Ico::FileArchive, Some(0xF59E0B)),
        FileKind::Archive(A::Tar) => (Ico::Archive, Some(0xB45309)),
        FileKind::Archive(A::SevenZip) => (Ico::FileArchive, Some(0x14B8A6)),
        FileKind::Archive(A::Rar) => (Ico::FileArchive, Some(0xA855F7)),
        FileKind::Archive(A::Compressed) => (Ico::FileArchive, Some(0x0EA5E9)),
        FileKind::Archive(A::Other) => (Ico::FileArchive, Some(0x94A3B8)),
        FileKind::Image => (Ico::FileImage, Some(0xEC4899)),
        FileKind::Video => (Ico::FileVideoCamera, Some(0xE11D48)),
        FileKind::Audio => (Ico::FileMusic, Some(0x06B6D4)),
        FileKind::Code => (Ico::FileCode, Some(0x6366F1)),
        FileKind::Script => (Ico::FileTerminal, Some(0x65A30D)),
        FileKind::Text => (Ico::FileText, Some(0x7C8DA6)),
        FileKind::Document => (Ico::FileType, Some(0x3B82F6)),
        FileKind::Spreadsheet => (Ico::FileSpreadsheet, Some(0x16A34A)),
        FileKind::Presentation => (Ico::Presentation, Some(0xF97316)),
        FileKind::Executable => (Ico::Package, Some(0xC026D3)),
        FileKind::DiskImage => (Ico::Disc3, Some(0x78716C)),
        FileKind::Certificate => (Ico::FileKey, Some(0xEAB308)),
        FileKind::Config => (Ico::FileCog, Some(0x6B7280)),
        FileKind::Database => (Ico::Database, Some(0x0891B2)),
        FileKind::Font => (Ico::Type, Some(0x8D6E63)),
        // 普通文件保持原来的样子：组件库的 File 图标 + 皮肤的次要文字色。
        FileKind::Plain => (Ico::File, None),
    }
}

/// 内置图标元素（16px 方块，和原来文件行的灰色 File 图标一样大）。
fn builtin_icon(kind: FileKind, tokens: &Tokens) -> AnyElement {
    let (icon, colour) = builtin(kind);
    match colour {
        Some(colour) => Icon::new(icon)
            .size_4()
            .text_color(::gpui::rgb(colour))
            .into_any_element(),
        None => Icon::new(IconName::File)
            .size_4()
            .text_color(tokens.text_muted)
            .into_any_element(),
    }
}

/// 文件行（和文件类链接底图）用的图标：Windows 上系统图标优先，其余情况是按类型分的内置图标。
/// `scale` 是窗口缩放（1.0 = 100%），系统图标据此取够清晰的尺寸。
pub fn file_icon(name: &str, scale: f32, tokens: &Tokens) -> AnyElement {
    let kind = classify(name);
    #[cfg(windows)]
    let kind = match choose_icon(kind, system::lookup(name, scale)) {
        IconSource::System(image) => return system::image_element(image),
        IconSource::Builtin(kind) => kind,
    };
    #[cfg(not(windows))]
    let _ = scale;
    builtin_icon(kind, tokens)
}

/// 后台又查好了新的系统图标：列表要重画。`seen` 是调用方记着的上次的值。非 Windows 恒为 `false`。
pub fn take_refresh(seen: &mut u64) -> bool {
    #[cfg(windows)]
    {
        let now = system::generation();
        if now != *seen {
            *seen = now;
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = seen;
    false
}

/// 系统图标要查的扩展名：最后一个点之后的部分，只收字母数字、最长 16 个字符，转小写。
/// 没有扩展名、点开头的名字（`.bashrc`）、带路径分隔符 / 空格 / 控制字符 / 非 ASCII / 过长的都返回 `None`，
/// 也就是**不去问系统**——传给系统的只有这个清洗过的扩展名拼出来的假文件名，绝不是真实路径。
#[cfg(any(windows, test))]
pub fn system_extension(name: &str) -> Option<String> {
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    sanitize_extension(&name[dot + 1..])
}

/// 扩展名清洗：1–16 个 ASCII 字母数字，转小写；别的一律 `None`。
#[cfg(any(windows, test))]
pub fn sanitize_extension(ext: &str) -> Option<String> {
    let ok = !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then(|| ext.to_ascii_lowercase())
}

/// 「系统图标查到了没有」决定画哪个：只有查到可用的系统图标才用它，
/// 还在查、没查到、只有通用白纸时都画内置图标。
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum IconSource<T> {
    System(T),
    Builtin(FileKind),
}

#[cfg(any(windows, test))]
fn choose_icon<T>(kind: FileKind, lookup: cache::Lookup<T>) -> IconSource<T> {
    match lookup {
        cache::Lookup::Ready(icon) => IconSource::System(icon),
        cache::Lookup::Pending | cache::Lookup::Missing => IconSource::Builtin(kind),
    }
}

#[cfg(test)]
mod tests;
