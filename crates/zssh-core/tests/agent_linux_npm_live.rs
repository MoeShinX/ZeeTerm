//! Linux：真实 npm 全局安装的智能体，发现逻辑能不能找到并读出版本号（LINUX-AI 第 7 条）。
//!
//! 只验证"找到并读出 `--version`"，不登录、不跑对话、不碰任何真实账号。
//!
//! 需要事先把智能体装进一个临时 npm 前缀（`npm install -g --prefix <前缀> <包>`），
//! 再用环境变量 `ZT_LIVE_NPM_PREFIX` 指给测试；没设置（日常 `cargo test` / CI）就跳过并打印原因。
//! 可选 `ZT_LIVE_NODE_BIN`：node 所在目录，设了就把它也放进假 `PATH`，
//! 用来对比"PATH 里有 node / 没有 node"两种情况（桌面启动的应用常常没有 node）。
//!
//! 环境用 `EnvView::fake` 构造：`PATH` 只有那个前缀的 `bin`，`HOME` 指到一个空目录，
//! 所以不会碰到这台机器上别的安装，也不读用户的真实家目录。

#![cfg(unix)]
// 测试辅助函数里失败就该 panic（clippy 只在 `#[test]` 函数里自动放行）。
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use zssh_core::ai::agent::discovery::{
    discover_with_env, resolve_launch_path, AgentSpec, EnvView, CLAUDE, CODEX, OPENCODE,
};

fn live_prefix() -> Option<PathBuf> {
    match std::env::var_os("ZT_LIVE_NPM_PREFIX") {
        Some(prefix) if !prefix.is_empty() => Some(PathBuf::from(prefix)),
        _ => {
            println!("跳过：没有设置 ZT_LIVE_NPM_PREFIX（真实 npm 安装的发现实测）");
            None
        }
    }
}

async fn check(spec: &AgentSpec, prefix: &std::path::Path, with_node: bool) {
    let bin = prefix.join("bin");
    if !bin.join(spec.command).exists() {
        println!("跳过 {}：前缀里没有装这个智能体", spec.command);
        return;
    }
    let mut path = bin.display().to_string();
    if with_node {
        if let Some(node) = std::env::var_os("ZT_LIVE_NODE_BIN") {
            path = format!("{}:{}", path, PathBuf::from(node).display());
        }
    }
    // 每次一个随机唯一的空目录：固定名字会让上一次的残留混进这一次的发现结果。
    let home_guard = tempfile::Builder::new()
        .prefix("zssh-live-empty-home-")
        .tempdir()
        .expect("空家目录");
    let home = home_guard.path().to_path_buf();
    let home_text = home.display().to_string();
    let env = EnvView::fake(
        &[("PATH", path.as_str()), ("HOME", home_text.as_str())],
        &[],
    );
    let data_guard = tempfile::Builder::new()
        .prefix("zssh-live-empty-data-")
        .tempdir()
        .expect("空数据目录");
    let data_dir = data_guard.path().to_path_buf();

    let linked = bin.join(spec.command);
    let resolved = resolve_launch_path(&linked);
    let install = discover_with_env(spec, None, &data_dir, &env)
        .await
        .expect("发现不该报错")
        .unwrap_or_else(|| panic!("{} 没有被发现", spec.command));
    println!(
        "{}（PATH {} node）：链接 {} -> 解析到 {} ；发现 path={} version={} source={:?} too_old={}",
        spec.command,
        if with_node { "含" } else { "不含" },
        linked.display(),
        resolved.display(),
        install.path,
        install.version,
        install.source,
        install.too_old,
    );
    assert!(!install.version.trim().is_empty());
    assert_eq!(PathBuf::from(&install.path), resolved);
}

#[tokio::test]
async fn npm_installed_agents_are_found_with_a_version_even_without_node_on_path() {
    let Some(prefix) = live_prefix() else {
        return;
    };
    for spec in [&OPENCODE, &CODEX, &CLAUDE] {
        check(spec, &prefix, false).await;
    }
}

#[tokio::test]
async fn npm_installed_agents_are_found_with_node_on_path_too() {
    let Some(prefix) = live_prefix() else {
        return;
    };
    for spec in [&OPENCODE, &CODEX, &CLAUDE] {
        check(spec, &prefix, true).await;
    }
}
