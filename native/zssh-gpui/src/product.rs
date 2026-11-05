//! 产品身份：面向用户的名字，以及在线更新的总开关。
//!
//! ZeeTerm（最初是 Z-SSH 的 GPUI 原生版）以独立身份存在，可以和 Z-SSH（Tauri 版）并存：产品名、安装目录、
//! 卸载键、主程序名、默认数据目录都与 Tauri 版不同（安装包的对应部分见 `native/installer/zssh-native.nsi`，
//! 两边的名字有测试互相核对）。内部的 crate 名、模块名、`ZSSH_*` 环境变量都不跟着改。

/// 面向用户的产品名：窗口标题、托盘、关于页、对话框标题、安装包都用它。
pub const NAME: &str = "ZeeTerm";

/// 默认数据目录是 `%APPDATA%\<这个名字>`（`ZSSH_DATA_DIR` 优先）。
pub const DATA_DIR_NAME: &str = "ZeeTerm";

/// 在线更新总开关（正式版：开）。
///
/// 开着时：启动后和程序开着的时候按间隔自动检查更新（设置里「启动时自动检查」可关），手动「检查更新」
/// 打开更新窗口；下载的安装包必须通过 minisign 验签、且签名里的版本与清单一致才会安装。
/// 更新签名公钥（`native/zssh-update/updater.json`）还是占位值时，更新窗口只显示"更新签名未配置"，
/// 不请求、不下载、不安装（见 `update`），所以没配公钥的构建不会装上任何东西。
pub const AUTO_UPDATE_ENABLED: bool = true;

/// 仓库根的第三方声明原文。
const NOTICE_SOURCE: &str = include_str!("../../../NOTICE");

/// ZeeTerm 额外使用的第三方组件（GPUI、Lucide 图标、语法高亮等），单独放在 `native/NOTICE`。
const NATIVE_NOTICE_SOURCE: &str = include_str!("../../NOTICE");

/// 完整声明 = 根 NOTICE + 空一行 + `native/NOTICE`，换行统一成 `\n`（Windows 检出可能是 CRLF）。
/// `native/installer/build.ps1` 打安装包时用同一个拼法（有测试核对），两边内容一致。
fn combine_notices(root: &str, native: &str) -> String {
    let root = root.replace("\r\n", "\n");
    let native = native.replace("\r\n", "\n");
    format!("{}\n\n{}", root.trim_end(), native)
}

/// 关于页"第三方组件与许可"里显示的声明：根 NOTICE 与 `native/NOTICE` 合并。
pub fn notice_text() -> String {
    combine_notices(NOTICE_SOURCE, NATIVE_NOTICE_SOURCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_product_has_its_own_names() {
        assert_eq!(NAME, "ZeeTerm");
        assert_eq!(DATA_DIR_NAME, "ZeeTerm");
        // 不能和 Tauri 版重名：并存的前提。
        assert_ne!(NAME, "Z-SSH");
    }

    #[test]
    fn the_notice_names_this_product_and_keeps_third_party_credits() {
        let notice = notice_text();
        assert!(notice.starts_with("ZeeTerm\n"), "声明以产品名开头");
        assert!(notice.contains("Copyright (C) 2026 ZeeTerm contributors"));
        assert!(notice.contains("Netcatty"), "第三方署名不动");
        assert!(notice.contains("Apache-2.0"));
        // Z-SSH 只出现在说明来历的地方（产品的来源、移植素材的经手路径），不再拿它当产品名。
        for line in notice.lines().filter(|line| line.contains("Z-SSH")) {
            assert!(
                line.contains("derived from Z-SSH") || line.contains("through Z-SSH's"),
                "Z-SSH 只应出现在说明来历的句子里：{line}"
            );
        }
        assert!(notice.contains("derived from Z-SSH"));
    }

    /// 去掉空行、行尾空白后的行集合：用来核对"合并结果没丢任何一条"。
    fn content_lines(text: &str) -> std::collections::BTreeSet<String> {
        text.lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_merged_notice_has_every_line_of_both_files() {
        let merged = notice_text();
        let merged_lines = content_lines(&merged);
        for (name, source) in [
            ("根 NOTICE", NOTICE_SOURCE),
            ("native/NOTICE", NATIVE_NOTICE_SOURCE),
        ] {
            for line in content_lines(source) {
                assert!(
                    merged_lines.contains(&line),
                    "{name} 的这一行没进合并结果：{line}"
                );
            }
        }
        // 两份各有自己的条目，根 NOTICE 在前。
        let netcatty = merged.find("Netcatty").expect("根 NOTICE 的条目");
        let gpui = merged
            .find("GPUI, gpui-component")
            .expect("native/NOTICE 的条目");
        assert!(netcatty < gpui);
        assert!(merged.contains("Lucide"));
        assert!(merged.contains("syntect"));
    }

    #[test]
    fn native_only_components_live_in_native_notice_not_the_shared_root() {
        // 根 NOTICE 放核心库和移植代码的声明，只有原生界面才用的组件单独放 native/NOTICE。
        for native_only in [
            "GPUI",
            "gpui-component",
            "Lucide",
            "syntect",
            "alacritty_terminal",
        ] {
            assert!(
                !NOTICE_SOURCE.contains(native_only),
                "根 NOTICE 不该有 {native_only}"
            );
            assert!(
                NATIVE_NOTICE_SOURCE.contains(native_only),
                "native/NOTICE 缺 {native_only}"
            );
        }
    }

    #[test]
    fn combining_uses_one_blank_line_and_normalizes_line_endings() {
        assert_eq!(combine_notices("a\n", "b\n"), "a\n\nb\n");
        assert_eq!(combine_notices("a\r\n\r\n", "b\r\n"), "a\n\nb\n");
    }

    #[test]
    fn online_updates_are_switched_on_in_the_official_build() {
        // 编译期常量：把"正式版开着在线更新"钉住；改它之前要先想清楚占位公钥的防护还在。
        const {
            assert!(AUTO_UPDATE_ENABLED);
        }
    }
}
