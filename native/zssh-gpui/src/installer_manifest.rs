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
    let start = text.find("Section \"安装").expect("有安装段");
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
