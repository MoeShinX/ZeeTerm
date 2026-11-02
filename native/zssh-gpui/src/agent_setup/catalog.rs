//! 模型目录的进程级缓存（0.2.14 `modelOptions.ts` 的 `catalogCache` / `loadCatalog`）。
//!
//! 面板一打开就要显示默认模型，但不能每次挂载都去问智能体（opencode 冷启动可能要几十秒）。
//! 按"智能体 + 安装路径 + 版本"缓存：换了安装或者升级了版本，键就变了，自然重新取。
//! 同一个键同时只发一次请求；失败不缓存成功值（下次重新检测时再试）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use zssh_core::ai::agent::AgentModelCatalog;
use zssh_core::ai::agent::discovery::AgentInstall;

use super::AgentKind;

enum Entry {
    Loading,
    Ready(AgentModelCatalog),
    Failed,
}

static CACHE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

fn cache() -> MutexGuard<'static, Option<HashMap<String, Entry>>> {
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 缓存键：智能体 + 路径 + 版本。
pub fn key(kind: AgentKind, install: &AgentInstall) -> String {
    format!("{}|{}|{}", kind.id(), install.path, install.version)
}

/// 已经取到的目录。
pub fn get(key: &str) -> Option<AgentModelCatalog> {
    match cache().as_ref()?.get(key)? {
        Entry::Ready(catalog) => Some(catalog.clone()),
        _ => None,
    }
}

/// 上一次取目录失败了吗（失败不自动重试，等重新检测）。
#[cfg(test)]
fn failed(key: &str) -> bool {
    matches!(
        cache().as_ref().and_then(|map| map.get(key)),
        Some(Entry::Failed)
    )
}

/// 新对话拿到的最新目录顺手更新缓存。
pub fn store(key: String, catalog: AgentModelCatalog) {
    cache()
        .get_or_insert_with(HashMap::new)
        .insert(key, Entry::Ready(catalog));
}

/// 重新检测某个智能体时丢掉它的旧目录（路径或版本变了键本来就不同，这里是连同失败记录一起清掉）。
pub fn forget(kind: AgentKind) {
    let prefix = format!("{}|", kind.id());
    if let Some(map) = cache().as_mut() {
        map.retain(|key, _| !key.starts_with(&prefix));
    }
}

/// 这个键要不要发起一次取目录：没有记录才要，并立刻标成"取数中"，同一个键不会同时发两次。
fn claim(key: &str) -> bool {
    let mut guard = cache();
    let map = guard.get_or_insert_with(HashMap::new);
    if map.contains_key(key) {
        return false;
    }
    map.insert(key.to_owned(), Entry::Loading);
    true
}

fn finish(key: String, result: Result<AgentModelCatalog, String>) {
    let entry = match result {
        Ok(catalog) => Entry::Ready(catalog),
        Err(reason) => {
            // 0.2.14 也只是记日志：没有目录模型下拉就保持"默认模型"，发消息时再读一次。
            tracing::warn!(%reason, "预取模型目录失败");
            Entry::Failed
        }
    };
    cache().get_or_insert_with(HashMap::new).insert(key, entry);
}

/// 面板用：这个安装的目录没取过就在后台取一次（opencode / codex 会拉共用进程，Claude Code 只跑一次
/// `--help` 探测）。已经有、取数中、失败过的都不重复发。
pub fn ensure(kind: AgentKind, install: &AgentInstall) {
    let key = key(kind, install);
    if !claim(&key) {
        return;
    }
    let install = install.clone();
    match crate::ssh_backend::runtime() {
        Ok(runtime) => {
            runtime.spawn(async move {
                let result = crate::agent_host::fetch_catalog(kind, install).await;
                finish(key, result);
            });
        }
        Err(err) => finish(key, Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ai::agent::AgentModel;
    use zssh_core::ai::agent::discovery::DiscoverySource;

    fn install(path: &str, version: &str) -> AgentInstall {
        AgentInstall {
            path: path.to_owned(),
            version: version.to_owned(),
            source: DiscoverySource::Path,
            needs_shell: false,
            auto: true,
            too_old: false,
            skipped_older: None,
        }
    }

    fn catalog(id: &str) -> AgentModelCatalog {
        AgentModelCatalog {
            models: vec![AgentModel {
                id: id.to_owned(),
                name: id.to_owned(),
                unavailable: false,
            }],
            default: None,
            default_label: None,
        }
    }

    // 缓存是进程级的：这些用例只用各自独有的键，不清全局，免得和别的用例互相踩。
    #[test]
    fn the_key_follows_agent_path_and_version() {
        let a = key(AgentKind::Claude, &install("C:\\a\\claude.exe", "2.1.300"));
        assert_eq!(a, "claude|C:\\a\\claude.exe|2.1.300");
        assert_ne!(
            a,
            key(AgentKind::Claude, &install("C:\\a\\claude.exe", "2.2.0"))
        );
        assert_ne!(
            a,
            key(AgentKind::Claude, &install("C:\\b\\claude.exe", "2.1.300"))
        );
        assert_ne!(
            a,
            key(AgentKind::Codex, &install("C:\\a\\claude.exe", "2.1.300"))
        );
    }

    #[test]
    fn a_key_is_claimed_once_and_failures_stay_until_forgotten() {
        let k = "opencode|C:\\only-this-test\\x.exe|opencode 1.0.0".to_owned();
        assert!(claim(&k), "第一次占位");
        assert!(!claim(&k), "取数中不再发第二次");
        assert!(get(&k).is_none());
        finish(k.clone(), Err("opencode 没起来".into()));
        assert!(failed(&k));
        assert!(!claim(&k), "失败过也不自动重试（等重新检测）");
        forget(AgentKind::OpenCode);
        assert!(!failed(&k));
        assert!(claim(&k), "忘掉之后可以重新取");
        finish(k.clone(), Ok(catalog("a/b")));
        assert_eq!(get(&k), Some(catalog("a/b")));
        store(k.clone(), catalog("c/d"));
        assert_eq!(get(&k), Some(catalog("c/d")), "新对话的目录顺手更新缓存");
        forget(AgentKind::OpenCode);
    }

    #[test]
    fn forgetting_one_agent_keeps_the_others() {
        let claude = "claude|C:\\keep-me\\claude.exe|2.1.300".to_owned();
        let codex = "codex|C:\\drop-me\\codex.exe|0.150.0".to_owned();
        store(claude.clone(), catalog("sonnet"));
        store(codex.clone(), catalog("gpt"));
        forget(AgentKind::Codex);
        assert!(get(&claude).is_some());
        assert!(get(&codex).is_none());
        forget(AgentKind::Claude);
    }
}
