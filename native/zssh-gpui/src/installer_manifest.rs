//! 打包清单的单测：安装脚本（`native/installer/zssh-native.nsi`）、构建脚本（`build.ps1`）、
//! CI 工作流和程序里的产品名 / 数据目录互相对得上。只读文件、不跑安装器；真正的静默安装 / 升级 /
//! 卸载流程见 `native/installer/smoke-test.ps1` 和报告 `docs/reports/V2-G12.md`。

use std::path::PathBuf;

use crate::product;

fn repo_file(relative: &str) -> String {
    let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", relative]
        .iter()
        .collect();
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("读不了 {relative}：{err}"))
        .replace("\r\n", "\n")
}

fn nsi() -> String {
    repo_file("native/installer/zssh-native.nsi")
}

/// `!define NAME "值"` 的值。
fn define(text: &str, name: &str) -> String {
    let prefix = format!("!define {name} \"");
    text.lines()
        .find_map(|line| {
            let line = line.trim().trim_start_matches('\u{feff}');
            line.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix('"'))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| panic!("安装脚本里没有 !define {name}"))
}

/// `Section "Uninstall"` 到下一个 `SectionEnd` 之间的文字。
fn uninstall_section(text: &str) -> String {
    let start = text.find("Section \"Uninstall\"").expect("有卸载段");
    let rest = &text[start..];
    let end = rest.find("SectionEnd").expect("卸载段有结尾");
    rest[..end].to_owned()
}

fn install_section(text: &str) -> String {
    let start = text.find("Section \"Install").expect("有安装段");
    let rest = &text[start..];
    let end = rest.find("SectionEnd").expect("安装段有结尾");
    rest[..end].to_owned()
}

#[test]
fn the_installer_uses_the_zeeterm_identity() {
    let text = nsi();
    assert_eq!(define(&text, "APP_NAME"), product::NAME);
    assert_eq!(define(&text, "APP_EXE"), format!("{}.exe", product::NAME));
    // 内部组件的名字不变（AI 工具桥按这个名字在主程序旁边找它）。
    assert_eq!(define(&text, "MCP_EXE"), "zssh-mcp.exe");
    assert_eq!(define(&text, "PUBLISHER"), product::NAME);
    let key = define(&text, "UNINST_KEY");
    assert!(
        key.ends_with(&format!("\\Uninstall\\{}", product::NAME)),
        "卸载键：{key}"
    );
    assert!(
        text.contains(&format!(
            "InstallDir \"$LOCALAPPDATA\\Programs\\{}\"",
            product::NAME
        )),
        "默认安装目录应是 %LOCALAPPDATA%\\Programs\\{}",
        product::NAME
    );
    // 并存：任何地方都不能出现 Tauri 版 Z-SSH 的卸载键、目录和程序名。
    for tauri_name in [
        "Uninstall\\Z-SSH\"",
        "Programs\\Z-SSH",
        "Z-SSH.exe",
        "Software\\Z-SSH",
    ] {
        assert!(!text.contains(tauri_name), "不能碰 Tauri 版：{tauri_name}");
    }
}

#[test]
fn the_installer_is_per_user_and_ships_no_shared_runtime() {
    let text = nsi();
    assert!(text.contains("RequestExecutionLevel user"), "不要管理员");
    assert!(!text.contains("HKLM"), "只写当前用户的注册表");
    assert!(!text.contains("SetShellVarContext all"));
    // 不引导 / 不卸载 WebView2：脚本里没有它的安装段、引导程序名和下载地址。
    for forbidden in [
        "Section WebView2",
        "MicrosoftEdgeWebview2Setup",
        "MicrosoftEdgeWebView2RuntimeInstaller",
        "go.microsoft.com",
    ] {
        assert!(!text.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn the_installer_ships_exactly_the_expected_files() {
    let section = install_section(&nsi());
    let mut files: Vec<String> = section
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("File "))
        .map(|line| {
            // `File /oname=X "src"` 或 `File "src"`：取安装后的名字。
            match line.split_once("/oname=") {
                Some((_, rest)) => rest.split_whitespace().next().unwrap_or("").to_owned(),
                None => line
                    .trim_start_matches("File ")
                    .trim_matches('"')
                    .rsplit('\\')
                    .next()
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_owned(),
            }
        })
        .collect();
    files.sort();
    let mut expected = vec![
        "${APP_EXE}".to_owned(),
        "${MCP_EXE}".to_owned(),
        "LICENSE".to_owned(),
        "NOTICE".to_owned(),
        "${APP_ICON}".to_owned(),
    ];
    expected.sort();
    assert_eq!(files, expected, "安装段里的 File 行");
}

#[test]
fn uninstall_removes_what_install_wrote_and_never_the_user_data() {
    let text = nsi();
    let un = uninstall_section(&text);
    // 装了什么就删什么。
    for deleted in [
        "$SMPROGRAMS\\${APP_NAME}.lnk",
        "$DESKTOP\\${APP_NAME}.lnk",
        "$INSTDIR\\${APP_EXE}",
        "$INSTDIR\\${MCP_EXE}",
        "$INSTDIR\\LICENSE",
        "$INSTDIR\\NOTICE",
        "$INSTDIR\\${APP_ICON}",
        "$INSTDIR\\uninstall.exe",
    ] {
        assert!(un.contains(&format!("Delete \"{deleted}\"")), "{deleted}");
    }
    assert!(un.contains("DeleteRegKey HKCU \"${UNINST_KEY}\""));
    // 用户数据：不碰数据目录；目录只在空的时候才删（没有 /r）。
    assert!(!un.contains("$APPDATA"), "卸载不能碰 %APPDATA%（数据目录）");
    assert!(!un.contains("$LOCALAPPDATA"));
    assert!(!un.contains("RMDir /r"), "不能递归删目录");
    assert!(un.contains("RMDir \"$INSTDIR\""));
    // 数据目录名（%APPDATA% 下的 ZeeTerm）不能出现在卸载段的任何命令里（注释行除外）。
    for line in un
        .lines()
        .filter(|line| !line.trim_start().starts_with(';'))
    {
        assert!(
            !line.contains(product::DATA_DIR_NAME),
            "卸载命令里不该有数据目录：{line}"
        );
    }
}

#[test]
fn the_build_script_stages_the_same_files_and_rebuilds_the_mcp_bridge() {
    let script = repo_file("native/installer/build.ps1");
    assert!(script.contains(&format!("$productName = '{}'", product::NAME)));
    assert!(script.contains("@($mainExeName, 'zssh-mcp.exe', 'LICENSE', 'NOTICE', 'icon.ico')"));
    // 安装包里的 NOTICE 是根 NOTICE 和 native/NOTICE 合并的（拼法与 product::combine_notices 一致）。
    assert!(
        script.contains(
            "Copy-Notice (Join-Path $repoRoot 'NOTICE') (Join-Path $nativeRoot 'NOTICE')"
        )
    );
    assert!(script.contains(r#"$root + "`n`n" + $native"#));
    assert!(script.contains(r#".Replace("`r`n", "`n").TrimEnd()"#));
    // zssh-mcp 每次现编，并取 cargo 报告的路径；不再按固定位置去找旧文件。
    assert!(script.contains("-p zssh-mcp"));
    assert!(script.contains("--message-format=json-render-diagnostics"));
    assert!(
        !script.contains("native\\target\\release\\zssh-mcp.exe"),
        "不能再从 native/target 里捡旧的 zssh-mcp.exe"
    );
    // 版本号只在 -TestVersion 时临时指定（测试包），仓库里的版本号只读不写。
    assert!(script.contains("[string]$TestVersion"));
    assert!(!script.contains("Set-Content"));
    // 图标放在 native/installer/assets/，脚本从那里取，不读 src-tauri/。
    assert!(script.contains(r"'assets\icon.ico'"));
    assert!(
        !script.contains("src-tauri"),
        "安装脚本不能再依赖 Tauri 版目录"
    );
    let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("installer")
        .join("assets")
        .join("icon.ico");
    let icon_bytes = std::fs::read(&icon).unwrap_or_else(|err| panic!("读不了安装图标：{err}"));
    assert!(icon_bytes.starts_with(&[0, 0, 1, 0]), "应是 .ico 文件");
    // 安装包图标和 exe 里编进去的是同一份 Logo：只维护 assets/brand/zeeterm.ico 一个源头。
    let brand_icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("brand")
        .join("zeeterm.ico");
    let brand_bytes =
        std::fs::read(&brand_icon).unwrap_or_else(|err| panic!("读不了品牌图标：{err}"));
    assert_eq!(icon_bytes, brand_bytes, "安装图标应与 exe 内嵌的 Logo 相同");
    // 多尺寸齐全：小尺寸给托盘 / 标题栏，大尺寸给资源管理器和安装向导。
    let count = u16::from_le_bytes([icon_bytes[4], icon_bytes[5]]) as usize;
    let mut sizes: Vec<u32> = (0..count)
        .map(|i| match icon_bytes[6 + i * 16] {
            0 => 256,
            n => u32::from(n),
        })
        .collect();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![16, 20, 24, 32, 48, 64, 128, 256]);
}

/// 原生版的 CI 工作流：带着 Tauri 版的仓库里是 `native.yml`；导出的独立仓库里只有 `ci.yml`。
fn assert_workflow_only_builds_and_uploads(workflow: &str) {
    assert!(workflow.contains("./native/installer/build.ps1"));
    assert!(workflow.contains("actions/upload-artifact@"));
    // 不发布、不签名、不用 secrets：这些词一个都不能出现（注释里写"不…"的除外，所以只查具体用法）。
    for forbidden in [
        "secrets.",
        "gh release",
        "softprops/action-gh-release",
        "contents: write",
        "TAURI_SIGNING",
        "signtool",
    ] {
        assert!(!workflow.contains(forbidden), "{forbidden}");
    }
    // release.yml 没被引用。
    assert!(!workflow.contains("release.yml"));
}

#[test]
fn the_native_workflow_only_builds_and_uploads_the_installer() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let native_yml = root.join(".github/workflows/native.yml");
    if native_yml.is_file() {
        assert_workflow_only_builds_and_uploads(&repo_file(".github/workflows/native.yml"));
    } else {
        // 独立导出的仓库：只有一份 ci.yml，里面带着打包步骤。
        assert_workflow_only_builds_and_uploads(&repo_file(".github/workflows/ci.yml"));
    }
    // 导出用的 CI 模板（只在 monorepo 里有）也要守同一条规矩。
    let template = root.join("scripts/export-zeeterm/ci.yml");
    if template.is_file() {
        assert_workflow_only_builds_and_uploads(&repo_file("scripts/export-zeeterm/ci.yml"));
    }
}

// ---- 版本号来源：改版本只该动 release.ps1 统一写的那几处，彼此必须一致 ----

/// `[workspace.package]` 段里的 `version = "x.y.z"`。
fn workspace_version(manifest: &str) -> String {
    let section = manifest
        .split("[workspace.package]")
        .nth(1)
        .expect("Cargo.toml 应有 [workspace.package]");
    section
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .find_map(|line| {
            line.trim()
                .strip_prefix("version")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .map(|value| value.trim().trim_matches('"').to_owned())
        })
        .expect("workspace 应有版本号")
}

/// Cargo.lock 里"本仓库自己的、版本跟随工作区"的包：名字以 `zssh-` 开头、没有 `source`（路径依赖）、
/// 版本不是占位的 `0.0.0`。返回 `(包名, 版本)`。
fn lock_workspace_versions(lock: &str) -> Vec<(String, String)> {
    lock.split("[[package]]")
        .skip(1)
        .filter_map(|block| {
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|line| line.strip_prefix(&format!("{key} = \"")))
                    .and_then(|rest| rest.strip_suffix('"'))
                    .map(str::to_owned)
            };
            let name = field("name")?;
            let version = field("version")?;
            let has_source = block.lines().any(|line| line.starts_with("source = "));
            (name.starts_with("zssh-") && !has_source && version != "0.0.0")
                .then_some((name, version))
        })
        .collect()
}

#[test]
fn every_place_that_carries_the_version_agrees() {
    let native = workspace_version(&repo_file("native/Cargo.toml"));
    let root = workspace_version(&repo_file("Cargo.toml"));
    assert_eq!(
        native, root,
        "native/Cargo.toml 与根 Cargo.toml 的版本要一致"
    );
    assert_eq!(native, env!("CARGO_PKG_VERSION"), "编进程序里的版本");

    for lock in ["Cargo.lock", "native/Cargo.lock"] {
        let versions = lock_workspace_versions(&repo_file(lock));
        assert!(!versions.is_empty(), "{lock} 里应有本仓库的包");
        for (name, version) in versions {
            assert_eq!(
                version, native,
                "{lock} 里 {name} 的版本落后了（release.ps1 会统一改）"
            );
        }
    }

    // 安装包的版本来自 native/Cargo.toml（build.ps1 读它），安装脚本里不写死具体版本。
    let build = repo_file("native/installer/build.ps1");
    assert!(build.contains(r"^\[workspace\.package\]"));
    let nsi = nsi();
    assert_eq!(
        define(&nsi, "APP_VERSION"),
        "0.0.0",
        "只有构建参数覆盖的占位默认值"
    );
}

#[test]
fn the_lock_file_parser_only_counts_path_packages_that_follow_the_workspace() {
    let lock = r#"
[[package]]
name = "zssh-core"
version = "1.2.3"

[[package]]
name = "zssh-regex"
version = "0.0.0"

[[package]]
name = "zssh-lookalike"
version = "9.9.9"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "serde"
version = "1.0.0"
"#;
    assert_eq!(
        lock_workspace_versions(lock),
        vec![("zssh-core".to_owned(), "1.2.3".to_owned())]
    );
}

// ---- Linux 打包：桌面文件、图标名、包文件名、发布流水线彼此对得上 ----

fn linux_file(relative: &str) -> String {
    repo_file(&format!("native/installer/linux/{relative}"))
}

/// `.desktop` 里 `Key=值` 的值（只看主段，忽略带语言后缀的 `Key[xx]=`）。
fn desktop_value(text: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
        .unwrap_or_else(|| panic!(".desktop 里没有 {key}="))
}

#[test]
fn the_desktop_entry_matches_the_window_identity() {
    let app_id = crate::window_frame::APP_ID;
    let desktop = linux_file(&format!("{app_id}.desktop"));
    assert!(desktop.starts_with("[Desktop Entry]\n"));
    assert_eq!(desktop_value(&desktop, "Name"), product::NAME);
    // 窗口的 app_id / WM_CLASS 就是图标名和 StartupWMClass：任务栏靠它把窗口和这个 .desktop 对上。
    assert_eq!(desktop_value(&desktop, "Icon"), app_id);
    assert_eq!(desktop_value(&desktop, "StartupWMClass"), app_id);
    // deb 把命令装在 /usr/bin/zeeterm（指向 /usr/lib/zeeterm/zeeterm 的软链接）；Exec 和 TryExec 写绝对路径，不依赖 PATH。
    assert_eq!(desktop_value(&desktop, "Exec"), "/usr/bin/zeeterm");
    assert_eq!(desktop_value(&desktop, "TryExec"), "/usr/bin/zeeterm");
    assert_eq!(desktop_value(&desktop, "Type"), "Application");
    assert_eq!(desktop_value(&desktop, "Terminal"), "false");
    assert_eq!(
        desktop_value(&desktop, "Categories"),
        "System;TerminalEmulator;Network;"
    );
    // 多语言：中文说明和关键词在，列表值都以分号结尾。
    assert!(desktop.contains("\nComment[zh_CN]="));
    assert!(desktop.contains("\nKeywords[zh_CN]="));
    for key in ["Categories", "Keywords"] {
        assert!(desktop_value(&desktop, key).ends_with(';'), "{key}");
    }
}

#[test]
fn the_icon_tool_writes_the_icon_name_the_desktop_entry_uses() {
    let app_id = crate::window_frame::APP_ID;
    let tool = repo_file("native/tools/icons/src/main.rs");
    assert!(
        tool.contains(&format!("const LINUX_ICON_NAME: &str = \"{app_id}\";")),
        "出图工具的 Linux 图标名要等于应用标识"
    );
    let script = repo_file("scripts/package-linux.sh");
    assert!(script.contains(&format!("APP_ID=\"{app_id}\"")));
    for name in ["install.sh", "uninstall.sh"] {
        let text = linux_file(&format!("tar/{name}"));
        assert!(text.contains(&format!("APP_ID=\"{app_id}\"")), "{name}");
    }
    let verify = linux_file("verify-in-container.sh");
    assert!(verify.contains(&format!("app_id=\"{app_id}\"")));
}

#[test]
fn the_tar_installer_and_the_in_app_updater_agree_on_the_layout() {
    use zssh_update::linux::{APP_ID, DEB_COMMAND, HELPER_BIN, MAIN_BIN};
    assert_eq!(APP_ID, crate::window_frame::APP_ID);
    let install = linux_file("tar/install.sh");
    let uninstall = linux_file("tar/uninstall.sh");
    // 程序在 <prefix>/lib/zeeterm，bin/zeeterm 是相对软链接；清单在 <prefix>/share/zeeterm/install-manifest——
    // 应用内更新（zssh_update::linux::TarLayout）靠这几处认出 install.sh 的布局。
    assert!(install.contains("lib_dir=\"$prefix/lib/zeeterm\""));
    assert!(install.contains("state_dir=\"$prefix/share/zeeterm\""));
    assert!(install.contains("manifest=\"$state_dir/install-manifest\""));
    for name in [MAIN_BIN, HELPER_BIN] {
        assert!(
            install.contains(&format!("put 755 \"$here/{name}\" \"$lib_dir/{name}\"")),
            "{name} 要装进 lib/zeeterm"
        );
    }
    assert!(install.contains(&format!("ln -s ../lib/zeeterm/{MAIN_BIN} \"$link_tmp\"")));
    assert!(
        !install.contains("\"$bin_dir/zssh-mcp\""),
        "zssh-mcp 不进用户 PATH 里的 bin"
    );
    assert!(uninstall.contains("rmdir \"$prefix/lib/zeeterm\""));
    // 图标沿用打包脚本的目录结构：icons/hicolor/<尺寸>/apps/<应用标识>.png。
    assert!(
        repo_file("scripts/package-linux.sh")
            .contains("install -D -m 644 \"$work/$png\" \"$tar_root/icons/$png\"")
    );
    // deb 的命令位置和桌面文件的 Exec 是同一个。
    let desktop = linux_file(&format!("{APP_ID}.desktop"));
    assert_eq!(desktop_value(&desktop, "Exec"), DEB_COMMAND);
    assert_eq!(desktop_value(&desktop, "TryExec"), DEB_COMMAND);
    assert!(
        repo_file("scripts/package-linux.sh")
            .contains("ln -s ../lib/zeeterm/zeeterm \"$bin_dir/zeeterm\"")
    );
    // install.sh 把装好的路径写进 Exec 和 TryExec 两行。
    assert!(install.contains("/^Exec=/") && install.contains("/^TryExec=/"));
}

#[test]
fn the_appstream_template_matches_the_desktop_entry_and_license() {
    let app_id = crate::window_frame::APP_ID;
    let xml = linux_file(&format!("{app_id}.metainfo.xml.in"));
    assert!(xml.contains(&format!("<id>{app_id}</id>")));
    assert!(xml.contains(&format!(
        "<launchable type=\"desktop-id\">{app_id}.desktop</launchable>"
    )));
    assert!(xml.contains("<project_license>GPL-3.0-or-later</project_license>"));
    assert!(xml.contains("<binary>zeeterm</binary>"));
    // 版本、日期、类型由打包脚本填；模板里必须留着这些占位符，脚本才替换得到。
    let script = repo_file("scripts/package-linux.sh");
    for placeholder in ["@VERSION@", "@TAG_VERSION@", "@DATE@", "@RELEASE_TYPE@"] {
        assert!(xml.contains(placeholder), "{placeholder}");
        assert!(script.contains(placeholder), "脚本没有替换 {placeholder}");
    }
}

#[test]
fn linux_package_names_agree_between_the_script_and_the_release_pipeline() {
    let script = repo_file("scripts/package-linux.sh");
    assert!(script.contains("ARCH_NAME=\"linux-x86_64\""));
    assert!(script.contains("base_name=\"ZeeTerm-${version}-${ARCH_NAME}\""));
    assert!(script.contains("deb_file=\"$out_dir/$base_name.deb\""));
    assert!(script.contains("tar_file=\"$out_dir/$base_name.tar.gz\""));
    // glibc 2.35（Ubuntu 22.04）基线，deb 的 Depends 补上 dlopen 的运行时库。
    assert!(script.contains("GLIBC_BASELINE=\"2.35\""));
    for dlopened in ["libvulkan1", "libwayland-client0", "libegl1"] {
        assert!(script.contains(dlopened), "{dlopened}");
    }
    // zssh-mcp 必须和主程序放在同一个目录（AI 工具桥按主程序旁边找它），而且是内部辅助程序，不占 PATH 里的命令名：
    // deb 把两个程序都装进 /usr/lib/zeeterm，/usr/bin/zeeterm 是指向 ../lib/zeeterm/zeeterm 的相对软链接。
    assert!(script.contains("install -m 755 \"$gpui_bin\" \"$lib_dir/zeeterm\""));
    assert!(script.contains("install -m 755 \"$mcp_bin\" \"$lib_dir/zssh-mcp\""));
    assert!(script.contains("lib_dir=\"$stage/usr/lib/zeeterm\""));
    assert!(script.contains("ln -s ../lib/zeeterm/zeeterm \"$bin_dir/zeeterm\""));
    assert!(
        !script.contains("\"$bin_dir/zssh-mcp\""),
        "zssh-mcp 不进 /usr/bin"
    );
    // tar.gz 包里是平铺的（解压即可运行），install.sh 才按 deb 的布局装到 <prefix>/lib/zeeterm。
    assert!(script.contains("install -m 755 \"$mcp_bin\" \"$tar_root/zssh-mcp\""));

    let sign = repo_file("scripts/release-sign.ps1");
    assert!(sign.contains("ZeeTerm-$Version-linux-x86_64.deb"));
    assert!(sign.contains("ZeeTerm-$Version-linux-x86_64.tar.gz"));
    let lib = repo_file("scripts/release-lib.ps1");
    assert!(lib.contains(&format!(
        "'{}' = 'linux-deb'",
        zssh_update::PLATFORM_LINUX_DEB
    )));
    assert!(lib.contains(&format!(
        "'{}' = 'linux-tar'",
        zssh_update::PLATFORM_LINUX_TAR
    )));
    assert_eq!(zssh_update::PLATFORM_LINUX_DEB, "linux-x86_64-deb");
    assert_eq!(zssh_update::PLATFORM_LINUX_TAR, "linux-x86_64-tar");

    let release = repo_file(".github/workflows/release.yml");
    assert!(release.contains("ZeeTerm-$env:VERSION-linux-x86_64.deb"));
    assert!(release.contains("ZeeTerm-$env:VERSION-linux-x86_64.tar.gz"));
    assert!(release.contains("ZeeTerm-$VERSION-linux-x86_64.deb"));
    assert!(release.contains("ZeeTerm-$VERSION-linux-x86_64.tar.gz"));
}

#[test]
fn only_the_final_release_job_touches_the_signing_key() {
    let release = repo_file(".github/workflows/release.yml");
    let job = |name: &str| {
        release
            .find(&format!("\n  {name}:\n"))
            .unwrap_or_else(|| panic!("有 {name} 任务"))
    };
    let (verify, linux, windows, sign) =
        (job("verify"), job("linux"), job("windows"), job("release"));
    assert!(
        verify < linux && linux < windows && windows < sign,
        "任务顺序：verify、linux、windows、release"
    );
    let forbidden = [
        "secrets.",
        "ZEETERM_UPDATER_KEY",
        "environment:",
        "contents: write",
        "gh release create",
        "release-sign",
    ];
    // 校验和两个编译任务都拿不到私钥、不能建 Release。
    for (name, block) in [
        ("verify", &release[verify..linux]),
        ("linux", &release[linux..windows]),
        ("windows", &release[windows..sign]),
    ] {
        for word in forbidden {
            assert!(!block.contains(word), "{name} 任务里不能有 {word}");
        }
        assert!(block.contains("contents: read"), "{name}");
    }
    let verify_job = &release[verify..linux];
    // 门禁不在发版里重跑，所以发版前必须核对 main 上这个提交的 CI 已经通过。
    assert!(verify_job.contains("actions/workflows/ci.yml/runs"));
    assert!(verify_job.contains("completed/success"));
    let linux_job = &release[linux..windows];
    assert!(linux_job.contains("runs-on: ubuntu-22.04"));
    assert!(linux_job.contains("needs: verify"));
    assert!(linux_job.contains("./scripts/package-linux.sh"));
    assert!(linux_job.contains("./scripts/linux-container-check.sh"));
    let windows_job = &release[windows..sign];
    assert!(windows_job.contains("needs: verify"));
    assert!(windows_job.contains("native/installer/build.ps1"));
    // 签名、发布只在 release 任务里，而且要等两个编译任务都成功。
    let release_job = &release[sign..];
    assert!(release_job.contains("needs: [verify, linux, windows]"));
    assert!(release_job.contains("-LinuxDebPath"));
    assert!(release_job.contains("-LinuxTarPath"));
    assert!(release_job.contains("environment: release"));
    // 私钥文件在取回三个包之后才写出来，而且无论成败都删掉。
    let write_key = release_job.find("写出更新私钥").expect("写私钥步骤");
    for fetched in ["取回 Windows 安装包", "取回 Linux 包"] {
        let at = release_job.find(fetched).expect(fetched);
        assert!(at < write_key, "{fetched} 要在写私钥之前");
    }
    let cleanup = release_job.find("删掉临时私钥文件").expect("清理步骤");
    assert!(release_job[cleanup..].contains("if: always()"));
}

#[test]
fn linux_scripts_are_lf_shell_scripts_with_a_shebang() {
    for relative in [
        "scripts/gates.sh",
        "scripts/package-linux.sh",
        "scripts/linux-build-deps.sh",
        "scripts/linux-keyring-session.sh",
        "scripts/linux-container-check.sh",
        "native/installer/linux/verify-in-container.sh",
        "native/installer/linux/tar/install.sh",
        "native/installer/linux/tar/uninstall.sh",
        "native/installer/linux/deb/postinst",
        "native/installer/linux/deb/postrm",
        "native/installer/linux/app.zeeterm.ZeeTerm.desktop",
    ] {
        let path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", relative]
            .iter()
            .collect();
        let bytes = std::fs::read(&path).unwrap_or_else(|err| panic!("读不了 {relative}：{err}"));
        if !relative.ends_with(".desktop") {
            assert!(bytes.starts_with(b"#!"), "{relative} 要有 shebang");
        }
        assert!(!bytes.contains(&b'\r'), "{relative} 必须是 LF 换行");
    }
}

#[test]
fn the_maintainer_scripts_only_refresh_desktop_caches() {
    for name in ["postinst", "postrm"] {
        let text = linux_file(&format!("deb/{name}"));
        assert!(text.starts_with("#!/bin/sh\n"), "{name}");
        assert!(text.contains("\nset -e\n"), "{name}");
        // 只能调这两个刷新命令，而且都是"有才调、失败不挡安装"。
        for command in ["update-desktop-database", "gtk-update-icon-cache"] {
            assert!(
                text.contains(&format!("command -v {command}")),
                "{name}: {command}"
            );
        }
        for forbidden in [
            "\nrm ",
            "rm -",
            "useradd",
            "systemctl",
            "chmod",
            "chown",
            "curl",
            "wget",
        ] {
            assert!(!text.contains(forbidden), "{name} 不该有 {forbidden}");
        }
        assert!(text.contains("|| true"));
    }
}

#[test]
fn the_ci_builds_and_packages_on_ubuntu_with_the_shared_dependency_list() {
    let ci = repo_file(".github/workflows/ci.yml");
    assert!(ci.contains("runs-on: ubuntu-22.04"));
    assert!(ci.contains("./scripts/linux-build-deps.sh"));
    assert!(ci.contains("./scripts/linux-keyring-session.sh cargo test"));
    assert!(ci.contains("./scripts/package-linux.sh"));
    assert!(ci.contains("./scripts/linux-container-check.sh"));
    // 系统依赖只有 scripts/linux-build-deps.sh 一份清单，工作流里不再自己 apt-get install。
    assert!(!ci.contains("apt-get install"));
    let release = repo_file(".github/workflows/release.yml");
    assert!(!release.contains("apt-get install"));
    let deps = repo_file("scripts/linux-build-deps.sh");
    for needed in [
        "xvfb",
        "xauth",
        "gnome-keyring",
        "dpkg-dev",
        "libxkbcommon-x11-dev",
        "libssl-dev",
    ] {
        assert!(deps.contains(needed), "{needed}");
    }
}
