//! 导入前的预览：数一数这份数据里有什么、导入后本机会多出 / 改掉 / 删掉多少。
//!
//! 预览和真正导入共用 [`SyncEngine::plan_import`] 的计划，数字不会和导入结果对不上。
//! 预览只读本机的库，不写任何东西（含本地备份）。

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::apply::ChangeCounts;
use super::engine::SyncEngine;
use super::error::SyncError;
use super::offline::OfflineImportMode;
use super::package::SyncSnapshot;

/// 快捷命令在设置里的键（值是一段 JSON 文本，里面是命令数组）。
const QUICK_COMMANDS_KEY: &str = "quick_commands";

/// 这份数据里各类记录的总数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceCounts {
    pub groups: usize,
    pub hosts: usize,
    pub identities: usize,
    pub proxies: usize,
    pub known_hosts: usize,
    pub quick_commands: usize,
    /// 可同步的设置项数（快捷命令也是其中一项，这里不扣掉）。
    pub settings: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportPreview {
    pub mode: OfflineImportMode,
    pub source: SourceCounts,
    /// 导入后相对本机现在的变化：新增 / 更新（同一条记录内容不同）/ 删除（只有替换才会有）。
    pub groups: ChangeCounts,
    pub hosts: ChangeCounts,
    pub identities: ChangeCounts,
    pub proxies: ChangeCounts,
    pub known_hosts_added: usize,
    /// 会被跳过的可疑主机密钥记录数（万能通配、字段不合格）；导入时不会写进 known_hosts。
    pub known_hosts_skipped: usize,
    pub settings_changed: usize,
    /// 合并时两边都改过的条数（取较新的，另一份进冲突页）。
    pub conflicts: usize,
    /// 要新增的主机里，名称、地址、端口、用户名都和本机已有主机一样的台数。两边的记录
    /// 编号不同所以不会合成一条，导入后各留一份。
    pub duplicate_hosts: usize,
    /// 引用的记录不在了、被清空的引用数。
    pub nulled_references: usize,
}

impl SourceCounts {
    pub fn of(snapshot: &SyncSnapshot) -> Self {
        Self {
            groups: snapshot.groups.len(),
            hosts: snapshot.hosts.len(),
            identities: snapshot.identities.len(),
            proxies: snapshot.proxies.len(),
            known_hosts: snapshot.known_hosts.len(),
            quick_commands: quick_command_count(&snapshot.settings),
            settings: snapshot.settings.len(),
        }
    }
}

/// 快捷命令条数；读不出来按 0（预览只是个数字，不能因为它坏了就拒绝导入）。
fn quick_command_count(settings: &BTreeMap<String, Value>) -> usize {
    let Some(value) = settings.get(QUICK_COMMANDS_KEY) else {
        return 0;
    };
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    match parsed {
        Some(Value::Array(items)) => items.len(),
        _ => 0,
    }
}

/// 按 uid 比较两边：新增 / 更新 / 删除各多少条。
fn count_changes<T>(
    current: &[T],
    target: &[T],
    uid: impl Fn(&T) -> &str,
    same: impl Fn(&T, &T) -> bool,
) -> ChangeCounts {
    let before: BTreeMap<&str, &T> = current.iter().map(|r| (uid(r), r)).collect();
    let after: BTreeMap<&str, &T> = target.iter().map(|r| (uid(r), r)).collect();
    let mut counts = ChangeCounts::default();
    for (key, record) in &after {
        match before.get(key) {
            None => counts.inserted += 1,
            Some(old) if !same(old, record) => counts.updated += 1,
            Some(_) => {}
        }
    }
    counts.deleted = before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .count();
    counts
}

impl SyncEngine<'_> {
    /// 预览导入 `incoming` 会发生什么。`incoming` 只被读，不会改动。
    pub fn preview_import(
        &self,
        incoming: &SyncSnapshot,
        mode: OfflineImportMode,
    ) -> Result<ImportPreview, SyncError> {
        let source = SourceCounts::of(incoming);
        let plan = self.plan_import(incoming.clone(), mode, Self::now())?;
        let (current, target) = (&plan.current, &plan.target);

        let hosts = count_changes(
            &current.hosts,
            &target.hosts,
            |r| &r.uid,
            |a, b| a.same_content(b),
        );
        let known_before: BTreeSet<String> = current.known_hosts.iter().map(|r| r.key()).collect();
        let known_new = || {
            target
                .known_hosts
                .iter()
                .filter(|r| !known_before.contains(&r.key()))
        };
        let known_hosts_added = known_new()
            .filter(|r| super::known_hosts::is_importable(r))
            .count();
        let known_hosts_skipped = known_new()
            .filter(|r| !super::known_hosts::is_importable(r))
            .count();
        let settings_changed = target
            .settings
            .iter()
            .filter(|(key, value)| current.settings.get(*key) != Some(*value))
            .count();

        let local_uids: BTreeSet<&str> = current.hosts.iter().map(|h| h.uid.as_str()).collect();
        let local_endpoints: BTreeSet<(&str, &str, u16, &str)> = current
            .hosts
            .iter()
            .map(|h| {
                (
                    h.name.as_str(),
                    h.host.as_str(),
                    h.port,
                    h.username.as_str(),
                )
            })
            .collect();
        // 替换时本机多出来的主机会被删掉，不存在"各留一份"。
        let duplicate_hosts = if mode == OfflineImportMode::Replace {
            0
        } else {
            target
                .hosts
                .iter()
                .filter(|h| !local_uids.contains(h.uid.as_str()))
                .filter(|h| {
                    local_endpoints.contains(&(
                        h.name.as_str(),
                        h.host.as_str(),
                        h.port,
                        h.username.as_str(),
                    ))
                })
                .count()
        };

        Ok(ImportPreview {
            mode,
            source,
            groups: count_changes(
                &current.groups,
                &target.groups,
                |r| &r.uid,
                |a, b| a.same_content(b),
            ),
            hosts,
            identities: count_changes(
                &current.identities,
                &target.identities,
                |r| &r.uid,
                |a, b| a.same_content(b),
            ),
            proxies: count_changes(
                &current.proxies,
                &target.proxies,
                |r| &r.uid,
                |a, b| a.same_content(b),
            ),
            known_hosts_added,
            known_hosts_skipped,
            settings_changed,
            conflicts: plan.conflicts.len(),
            duplicate_hosts,
            nulled_references: plan.nulled_references.len(),
        })
    }
}
