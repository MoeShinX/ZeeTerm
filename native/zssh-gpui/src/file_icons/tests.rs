use std::collections::HashSet;

use super::cache::Lookup;
use super::*;

use ArchiveFlavor as A;
use FileKind as K;

#[test]
fn classification_table_covers_every_category() {
    let cases: &[(&str, FileKind)] = &[
        // 压缩包
        ("a.zip", K::Archive(A::Zip)),
        ("tool.jar", K::Archive(A::Zip)),
        ("b.tar.gz", K::Archive(A::Tar)),
        ("b.tgz", K::Archive(A::Tar)),
        ("b.tar", K::Archive(A::Tar)),
        ("b.tar.xz", K::Archive(A::Tar)),
        ("b.txz", K::Archive(A::Tar)),
        ("b.tar.bz2", K::Archive(A::Tar)),
        ("b.tbz2", K::Archive(A::Tar)),
        ("b.tar.zst", K::Archive(A::Tar)),
        ("c.7z", K::Archive(A::SevenZip)),
        ("d.rar", K::Archive(A::Rar)),
        ("e.gz", K::Archive(A::Compressed)),
        ("e.xz", K::Archive(A::Compressed)),
        ("e.bz2", K::Archive(A::Compressed)),
        ("e.zst", K::Archive(A::Compressed)),
        ("setup.cab", K::Archive(A::Other)),
        // 图片 / 视频 / 音频
        ("f.png", K::Image),
        ("f.JPEG", K::Image),
        ("f.svg", K::Image),
        ("g.mp4", K::Video),
        ("g.mkv", K::Video),
        ("h.mp3", K::Audio),
        ("h.flac", K::Audio),
        // 代码 / 脚本
        ("i.rs", K::Code),
        ("i.py", K::Code),
        ("i.ts", K::Code),
        ("index.html", K::Code),
        ("j.sh", K::Script),
        ("j.bash", K::Script),
        ("j.ps1", K::Script),
        ("j.bat", K::Script),
        ("j.cmd", K::Script),
        // 文本 / 文档 / 表格 / 演示
        ("k.log", K::Text),
        ("k.txt", K::Text),
        ("l.pdf", K::Document),
        ("l.docx", K::Document),
        ("l.odt", K::Document),
        ("l.md", K::Document),
        ("m.xlsx", K::Spreadsheet),
        ("m.xls", K::Spreadsheet),
        ("m.csv", K::Spreadsheet),
        ("m.ods", K::Spreadsheet),
        ("n.pptx", K::Presentation),
        ("n.ppt", K::Presentation),
        // 可执行和安装包
        ("o.exe", K::Executable),
        ("o.msi", K::Executable),
        ("o.deb", K::Executable),
        ("o.rpm", K::Executable),
        ("o.apk", K::Executable),
        ("Tool.AppImage", K::Executable),
        ("o.dmg", K::Executable),
        // 磁盘镜像
        ("p.iso", K::DiskImage),
        ("p.img", K::DiskImage),
        ("p.qcow2", K::DiskImage),
        ("p.vmdk", K::DiskImage),
        ("p.vhd", K::DiskImage),
        ("p.vhdx", K::DiskImage),
        // 证书和密钥
        ("q.pem", K::Certificate),
        ("q.crt", K::Certificate),
        ("q.cer", K::Certificate),
        ("q.key", K::Certificate),
        ("q.pub", K::Certificate),
        ("q.p12", K::Certificate),
        ("q.pfx", K::Certificate),
        ("q.ppk", K::Certificate),
        // 配置
        ("r.conf", K::Config),
        ("r.cfg", K::Config),
        ("r.ini", K::Config),
        ("r.yaml", K::Config),
        ("r.yml", K::Config),
        ("r.toml", K::Config),
        ("r.json", K::Config),
        ("r.xml", K::Config),
        ("r.properties", K::Config),
        ("nginx.service", K::Config),
        // 数据库 / 字体
        ("s.db", K::Database),
        ("s.sqlite", K::Database),
        ("s.sql", K::Database),
        ("s.mdb", K::Database),
        ("t.ttf", K::Font),
        ("t.otf", K::Font),
        ("t.woff2", K::Font),
        // 其他
        ("u.xyz123", K::Plain),
    ];
    for (name, want) in cases {
        assert_eq!(classify(name), *want, "{name}");
    }
}

#[test]
fn extensions_are_case_insensitive_and_compound_ones_win_over_the_last_dot() {
    assert_eq!(classify("BACKUP.TAR.GZ"), K::Archive(A::Tar));
    assert_eq!(classify("Backup.Tar.Xz"), K::Archive(A::Tar));
    assert_eq!(classify("PHOTO.JpG"), K::Image);
    // 复合扩展名要比「最后一个点」优先：.tar.gz 是 tar 系，不是单文件压缩的 .gz。
    assert_ne!(classify("a.tar.gz"), classify("a.gz"));
    // 多个点的普通名字照常看最后一个。
    assert_eq!(classify("my.report.final.pdf"), K::Document);
    assert_eq!(classify("data.2026.11.log"), K::Text);
    // 名字里带 tar 但不是复合扩展名。
    assert_eq!(classify("guitar.gz"), K::Archive(A::Compressed));
    assert_eq!(classify("tar.gz.txt"), K::Text);
}

#[test]
fn special_file_names_and_dotfiles() {
    for (name, want) in [
        ("Dockerfile", K::Code),
        ("dockerfile", K::Code),
        ("Makefile", K::Code),
        ("LICENSE", K::Text),
        ("README", K::Text),
        ("README.rst", K::Text),
        ("LICENSE-MIT", K::Text),
        ("CHANGELOG", K::Text),
        (".bashrc", K::Config),
        (".zshrc", K::Config),
        (".profile", K::Config),
        (".gitignore", K::Config),
        (".env", K::Config),
        (".env.local", K::Config),
        (".ENV.production", K::Config),
        ("id_rsa", K::Certificate),
        ("id_ed25519", K::Certificate),
        ("id_ecdsa", K::Certificate),
        ("id_rsa.pub", K::Certificate),
        ("authorized_keys", K::Certificate),
    ] {
        assert_eq!(classify(name), want, "{name}");
    }
    // README.md 有认识的扩展名，按文档算。
    assert_eq!(classify("README.md"), K::Document);
}

#[test]
fn no_extension_unknown_dotfiles_and_odd_names_are_plain() {
    for name in [
        "noext",
        ".hidden",
        ".config",
        ".a.unknownext",
        "trailingdot.",
        ".",
        "..",
        "",
        "file.",
        ".tar",
        "x.🙂",
        "文件.未知",
        // 超长、带怪字符的扩展名
        "x.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "x.a b",
        "x.a\u{7}b",
    ] {
        assert_eq!(classify(name), K::Plain, "{name:?}");
    }
    // 中文文件名不影响扩展名判断。
    assert_eq!(classify("报告 最终版.pdf"), K::Document);
}

#[test]
fn archive_flavours_are_all_distinct_in_kind_colour_and_icon() {
    let samples = [
        classify("a.zip"),
        classify("a.tar.gz"),
        classify("a.7z"),
        classify("a.rar"),
        classify("a.gz"),
    ];
    // tgz 与 tar.gz 同组，gz 单独一组，其余互不相同。
    assert_eq!(classify("a.tgz"), classify("a.tar.gz"));
    let kinds: HashSet<_> = samples.iter().collect();
    assert_eq!(kinds.len(), samples.len(), "{samples:?}");
    let looks: HashSet<_> = samples
        .iter()
        .map(|kind| {
            let (icon, colour) = builtin(*kind);
            (format!("{icon:?}"), colour)
        })
        .collect();
    assert_eq!(
        looks.len(),
        samples.len(),
        "压缩包细分要能一眼区分: {looks:?}"
    );
}

#[test]
fn every_kind_has_a_distinct_builtin_look_and_plain_keeps_the_old_grey_file() {
    let kinds = [
        K::Archive(A::Zip),
        K::Archive(A::Tar),
        K::Archive(A::SevenZip),
        K::Archive(A::Rar),
        K::Archive(A::Compressed),
        K::Archive(A::Other),
        K::Image,
        K::Video,
        K::Audio,
        K::Code,
        K::Script,
        K::Text,
        K::Document,
        K::Spreadsheet,
        K::Presentation,
        K::Executable,
        K::DiskImage,
        K::Certificate,
        K::Config,
        K::Database,
        K::Font,
        K::Plain,
    ];
    let mut looks = HashSet::new();
    for kind in kinds {
        let (icon, colour) = builtin(kind);
        assert!(
            looks.insert((format!("{icon:?}"), colour)),
            "{kind:?} 和别的类外观重复"
        );
    }
    // 普通文件：没有固定色（用皮肤的灰色），图标仍是组件库的 File。
    assert_eq!(builtin(K::Plain), (Ico::File, None));
    // 其余类都有固定色。
    for kind in kinds.iter().filter(|k| **k != K::Plain) {
        assert!(builtin(*kind).1.is_some(), "{kind:?}");
    }
}

#[test]
fn the_extension_table_has_no_duplicates_and_is_lowercase_ascii() {
    let mut seen = HashSet::new();
    for (ext, _) in EXTENSIONS {
        assert!(seen.insert(*ext), "扩展名重复：{ext}");
        assert!(
            ext.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{ext} 要小写 ASCII"
        );
    }
    let mut names = HashSet::new();
    for (name, _) in SPECIAL_NAMES {
        assert!(names.insert(*name), "特殊文件名重复：{name}");
        assert_eq!(*name, name.to_lowercase());
    }
}

#[test]
fn sanitized_extension_only_lets_short_alphanumerics_through() {
    assert_eq!(system_extension("a.TXT").as_deref(), Some("txt"));
    assert_eq!(system_extension("a.tar.gz").as_deref(), Some("gz"));
    assert_eq!(system_extension("archive.7z").as_deref(), Some("7z"));
    assert_eq!(
        system_extension("x.0123456789abcdef").as_deref(),
        Some("0123456789abcdef")
    );
    for name in [
        // 没有扩展名 / 点开头
        "noext",
        ".bashrc",
        "trailingdot.",
        "",
        // 超过 16 个字符
        "x.0123456789abcdefg",
        // 路径分隔符
        "x.a/b",
        "x.a\\b",
        "x./etc/passwd",
        "x..\\..\\windows",
        "x.txt/",
        // 空格、控制字符、NUL、换行
        "x.a b",
        "x.a\tb",
        "x.a\nb",
        "x.a\0b",
        "x.\u{1b}[31m",
        // 非 ASCII、通配符、流、冒号
        "x.文档",
        "x.tx*",
        "x.t?t",
        "x.txt:stream",
        "x.<>",
    ] {
        assert_eq!(system_extension(name), None, "{name:?} 不能拿去问系统");
    }
    assert_eq!(sanitize_extension(""), None);
    assert_eq!(sanitize_extension("a.b"), None);
    assert_eq!(sanitize_extension("Zip").as_deref(), Some("zip"));
}

#[test]
fn only_a_ready_system_icon_replaces_the_builtin_one() {
    let kind = classify("server.log");
    assert_eq!(choose_icon(kind, Lookup::Ready(7u8)), IconSource::System(7));
    // 还在查：先画内置图标。
    assert_eq!(
        choose_icon(kind, Lookup::<u8>::Pending),
        IconSource::Builtin(K::Text)
    );
    // 查不到 / 只有通用图标（resolver 把它收成 Missing）：认识的类别用内置分类图标。
    assert_eq!(
        choose_icon(kind, Lookup::<u8>::Missing),
        IconSource::Builtin(K::Text)
    );
    // 分类也不认识的：回到内置的灰色普通文件图标。
    assert_eq!(
        choose_icon(classify("weird.xyz123"), Lookup::<u8>::Missing),
        IconSource::Builtin(K::Plain)
    );
}

#[test]
fn every_builtin_icon_is_compiled_in_and_loads() {
    use gpui_kit::AssetSource as _;
    let kinds = [
        K::Archive(A::Zip),
        K::Archive(A::Tar),
        K::Archive(A::SevenZip),
        K::Archive(A::Rar),
        K::Archive(A::Compressed),
        K::Archive(A::Other),
        K::Image,
        K::Video,
        K::Audio,
        K::Code,
        K::Script,
        K::Text,
        K::Document,
        K::Spreadsheet,
        K::Presentation,
        K::Executable,
        K::DiskImage,
        K::Certificate,
        K::Config,
        K::Database,
        K::Font,
        K::Plain,
    ];
    for kind in kinds {
        let (icon, _) = builtin(kind);
        let bytes = crate::icons::AppAssets
            .load(&icon.path())
            .expect("读图标")
            .unwrap_or_else(|| panic!("{kind:?} 的图标 {icon:?} 没有编进来"));
        assert!(bytes.starts_with(b"<svg"), "{icon:?} 不是 SVG");
    }
}

/// 固定色在浅色和深色皮肤的背景上都要看得清：和白底、深灰底的亮度对比不能太低。
#[test]
fn fixed_colours_stay_readable_on_light_and_dark_backgrounds() {
    fn luminance(rgb: u32) -> f64 {
        let channel = |shift: u32| {
            let c = f64::from((rgb >> shift) & 0xFF) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }
    fn contrast(a: u32, b: u32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }
    const LIGHT: u32 = 0xFFFFFF;
    const DARK: u32 = 0x1E1E1E;
    for (name, kind) in [
        ("zip", K::Archive(A::Zip)),
        ("tar", K::Archive(A::Tar)),
        ("7z", K::Archive(A::SevenZip)),
        ("rar", K::Archive(A::Rar)),
        ("gz", K::Archive(A::Compressed)),
        ("other archive", K::Archive(A::Other)),
        ("image", K::Image),
        ("video", K::Video),
        ("audio", K::Audio),
        ("code", K::Code),
        ("script", K::Script),
        ("text", K::Text),
        ("document", K::Document),
        ("sheet", K::Spreadsheet),
        ("slides", K::Presentation),
        ("exe", K::Executable),
        ("disk", K::DiskImage),
        ("cert", K::Certificate),
        ("config", K::Config),
        ("db", K::Database),
        ("font", K::Font),
    ] {
        let colour = builtin(kind).1.expect("固定色");
        // 图标是图形不是正文，1.8:1 是「不会融进背景」的底线（接近的灰在 1.5 以下就糊了）。
        assert!(
            contrast(colour, LIGHT) >= 1.8,
            "{name} 在浅色背景上太淡: {colour:06X}"
        );
        assert!(
            contrast(colour, DARK) >= 1.8,
            "{name} 在深色背景上太暗: {colour:06X}"
        );
    }
}

/// 回归：文件行里目录、盘符、链接的画法和以前一样，只有「其余文件」和「文件类链接的底图」换成按类型的图标。
#[test]
fn the_file_row_keeps_folder_drive_and_link_icons_and_only_files_use_file_icon() {
    let source = include_str!("../sftp/view.rs");
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    // 目录：黄色文件夹；盘符：硬盘图标 + 文件夹蓝；都没动。
    assert!(code.contains("(None, EntryKind::Dir) => {\n                        crate::icons::folder_icon(false, 16.0).into_any_element()"));
    assert!(code.contains("(None, EntryKind::Drive) => Icon::new(IconName::HardDrive)"));
    assert!(code.contains(".text_color(rgb(FOLDER_COLOR))"));
    // 链接：目录链接的底还是黄色文件夹，角标（强调色 / 危险色圆点 + 箭头）原样。
    assert!(
        code.contains(
            "LinkBase::Folder => crate::icons::folder_icon(false, 16.0).into_any_element()"
        )
    );
    assert!(code.contains("link_badge_colors(info.broken, tokens)"));
    assert!(code.contains("Icon::new(Ico::ArrowUpRight).size(px(8.0))"));
    // 文件行和文件类链接的底图走 file_icon；原来的一律灰色 File 图标不再直接画。
    assert_eq!(code.matches("crate::file_icons::file_icon(").count(), 2);
    assert!(!code.contains("Icon::new(IconName::File)"));
    // 行高常量没动。
    assert!(code.contains("const ROW_HEIGHT: f32 = 34.0;"));
}

/// 回归：停靠面板 / 左侧边栏 / 文件窗口共用同一个视图，所以改这一处两边都生效；后台查好图标要能触发重画。
#[test]
fn the_shared_sftp_view_repaints_when_a_system_icon_arrives() {
    let source = include_str!("../sftp.rs");
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    assert!(code.contains("crate::file_icons::take_refresh(&mut self.icon_refresh)"));
    let sidebar = include_str!("../main_window/sidebar_files.rs");
    assert!(
        !sidebar.contains("file_icon("),
        "侧边栏复用文件视图的行渲染，不该另画一套文件图标"
    );
}

/// Linux / macOS：不调任何系统接口，文件行只用内置图标，也不会因为后台图标要求重画。
#[cfg(not(windows))]
#[test]
fn non_windows_platforms_only_have_builtin_icons() {
    let mut seen = 0;
    assert!(!take_refresh(&mut seen));
    assert_eq!(seen, 0);
}
