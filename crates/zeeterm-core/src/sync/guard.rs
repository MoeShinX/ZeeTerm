//! 防误删：一次同步会删掉太多记录时先停下来，把选择交给上层。
//!
//! 阈值（任务卡第 5 节）：删掉的记录超过本地记录总数的一半，或者删掉的主机
//! 超过 10 台。两种都按“超过”算，正好一半 / 正好 10 台不触发。

use std::collections::BTreeSet;

use super::package::{
    SyncSnapshot, ENTITY_AI_ENDPOINT, ENTITY_GROUP, ENTITY_HOST, ENTITY_IDENTITY, ENTITY_PROXY,
};

pub const MAX_DELETED_HOSTS: usize = 10;

/// 这次同步会删掉多少。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DeletionPlan {
    pub local_records: usize,
    pub deleted_records: usize,
    pub local_hosts: usize,
    pub deleted_hosts: usize,
    /// 会被删掉的记录里的前几条（对话框里列出来，让用户看得出删的是什么）。
    pub samples: Vec<DeletedSample>,
    /// 这份计划的身份：存储身份 + 会被删掉的全部记录（见 [`plan_token`]）。用户在对话框里
    /// 确认时把它带回来（[`DeletionDecision::ContinuePlan`] / [`DeletionDecision::UseCloudPlan`]），
    /// 引擎重新算出来对不上就不执行。
    pub token: u64,
    /// 用户确认的那份计划已经作废（换了存储、或云端 / 本机在确认期间变了），这是重新算出来的
    /// 新计划，需要重新确认；什么都没有执行。
    pub reconfirm: bool,
}

/// 对话框最多列几条名字。
pub const MAX_SAMPLES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeletedSample {
    /// `host` / `group` / `identity` / `proxy`
    pub entity: String,
    pub name: String,
}

impl DeletionPlan {
    pub fn needs_confirmation(&self) -> bool {
        self.deleted_records * 2 > self.local_records || self.deleted_hosts > MAX_DELETED_HOSTS
    }

    pub fn summary(&self) -> String {
        format!(
            "这次同步会删除 {} 条记录（本地共 {} 条），其中主机 {} 台（本地共 {} 台）",
            self.deleted_records, self.local_records, self.deleted_hosts, self.local_hosts
        )
    }
}

fn deleted_keys(entity: &str, local: &SyncSnapshot, target: &SyncSnapshot) -> (usize, usize) {
    let local_keys: BTreeSet<String> = local.keys_of(entity);
    let target_keys: BTreeSet<String> = target.keys_of(entity);
    let deleted = local_keys.difference(&target_keys).count();
    (local_keys.len(), deleted)
}

/// 本地有、目标里没有的记录的名字（主机优先排在前面由调用顺序决定）。
fn sample_names(entity: &str, local: &SyncSnapshot, target_keys: &BTreeSet<String>) -> Vec<String> {
    match entity {
        ENTITY_GROUP => local
            .groups
            .iter()
            .filter(|r| !target_keys.contains(&r.uid))
            .map(|r| r.name.clone())
            .collect(),
        ENTITY_HOST => local
            .hosts
            .iter()
            .filter(|r| !target_keys.contains(&r.uid))
            .map(|r| r.name.clone())
            .collect(),
        ENTITY_IDENTITY => local
            .identities
            .iter()
            .filter(|r| !target_keys.contains(&r.uid))
            .map(|r| r.name.clone())
            .collect(),
        ENTITY_PROXY => local
            .proxies
            .iter()
            .filter(|r| !target_keys.contains(&r.uid))
            .map(|r| r.name.clone())
            .collect(),
        ENTITY_AI_ENDPOINT => local
            .ai_endpoints
            .iter()
            .filter(|r| !target_keys.contains(&r.uid))
            .map(|r| r.name.clone())
            .collect(),
        _ => Vec::new(),
    }
}

/// 计划的身份：同步存储的身份 + 会被删掉的每一条记录（实体 + 键，排序后）。
///
/// 防误删对话框挂着的时候用户可能换了存储、另一台电脑可能又同步了一次；确认只对用户
/// 当时看到的那份计划有效，重新读云端、重新合并出来的计划对不上就必须重新确认。
/// 只取摘要的前 8 个字节：它不是密码学承诺，只是防"确认的不是同一份"的校验位。
pub fn plan_token(storage: Option<&str>, local: &SyncSnapshot, target: &SyncSnapshot) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(storage.unwrap_or_default().as_bytes());
    hasher.update([0u8]);
    for entity in PLAN_ENTITIES {
        let target_keys = target.keys_of(entity);
        // BTreeSet 的迭代本身有序，同样的删除集合在哪台电脑上算出来都一样。
        for key in local.keys_of(entity).difference(&target_keys) {
            hasher.update(entity.as_bytes());
            hasher.update([0u8]);
            hasher.update(key.as_bytes());
            hasher.update([0u8]);
        }
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

const PLAN_ENTITIES: [&str; 5] = [
    ENTITY_HOST,
    ENTITY_GROUP,
    ENTITY_IDENTITY,
    ENTITY_PROXY,
    ENTITY_AI_ENDPOINT,
];

/// 本地当前 → 合并结果（或云端版本）会删掉多少。
pub fn deletion_plan(local: &SyncSnapshot, target: &SyncSnapshot) -> DeletionPlan {
    let mut plan = DeletionPlan::default();
    for entity in PLAN_ENTITIES {
        let (local_count, deleted) = deleted_keys(entity, local, target);
        let target_keys = target.keys_of(entity);
        for name in sample_names(entity, local, &target_keys) {
            if plan.samples.len() < MAX_SAMPLES {
                plan.samples.push(DeletedSample {
                    entity: entity.to_owned(),
                    name,
                });
            }
        }
        plan.local_records += local_count;
        plan.deleted_records += deleted;
        if entity == ENTITY_HOST {
            plan.local_hosts = local_count;
            plan.deleted_hosts = deleted;
        }
    }
    plan
}

/// 上层的选择（S-2 的界面按钮）。
///
/// 界面用带计划身份的 [`Self::ContinuePlan`] / [`Self::UseCloudPlan`]：确认只对用户看到的那份
/// 计划有效。不带身份的 [`Self::Continue`] / [`Self::UseCloud`] 是"不管计划是什么都执行"，
/// 只给不经过确认对话框的调用方（测试、脚本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionDecision {
    /// 继续按合并结果删。
    Continue,
    /// 放弃合并结果，改用云端版本覆盖本地。
    UseCloud,
    /// 取消这次同步（本地和云端都不动）。
    Cancel,
    /// 同 [`Self::Continue`]，但只执行 [`DeletionPlan::token`] 对得上的那份计划；对不上就
    /// 什么都不做，返回重新算出来的计划（`reconfirm` 为真）让用户重新确认。
    ContinuePlan(u64),
    /// 同 [`Self::UseCloud`]，同样只对确认过的那份计划有效。
    UseCloudPlan(u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::package::{GroupRecord, HostRecord, SyncSnapshot};

    fn host(uid: &str) -> HostRecord {
        HostRecord {
            uid: uid.to_owned(),
            group_uid: None,
            name: uid.to_owned(),
            host: "example.internal".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: "password".to_owned(),
            identity_uid: None,
            proxy_uid: None,
            jump_host_uid: None,
            proxy_mode: "default".to_owned(),
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            protocol: "ssh".to_owned(),
            x11_forwarding: false,
            protocol_options: None,
            created_at: 1,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        }
    }

    fn group(uid: &str) -> GroupRecord {
        GroupRecord {
            uid: uid.to_owned(),
            name: uid.to_owned(),
            parent_uid: None,
            sort: 0,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        }
    }

    #[test]
    fn half_or_ten_hosts_is_not_enough() {
        let mut local = SyncSnapshot::empty("dev-a", 1);
        for index in 0..20 {
            local.hosts.push(host(&format!("h{index}")));
        }
        let mut target = SyncSnapshot::empty("dev-b", 1);
        for index in 10..20 {
            target.hosts.push(host(&format!("h{index}")));
        }
        let plan = deletion_plan(&local, &target);
        assert_eq!((plan.local_records, plan.deleted_records), (20, 10));
        assert_eq!((plan.local_hosts, plan.deleted_hosts), (20, 10));
        assert!(!plan.needs_confirmation(), "正好一半、正好 10 台不触发");
    }

    #[test]
    fn the_plan_lists_the_first_few_names_hosts_first() {
        let mut local = SyncSnapshot::empty("dev-a", 1);
        local.groups.push(group("old-group"));
        for index in 0..8 {
            local.hosts.push(host(&format!("h{index}")));
        }
        let target = SyncSnapshot::empty("dev-b", 1);
        let plan = deletion_plan(&local, &target);
        assert_eq!(plan.samples.len(), MAX_SAMPLES);
        assert!(plan.samples.iter().all(|s| s.entity == "host"));
        assert_eq!(plan.samples[0].name, "h0");
        assert_eq!(plan.deleted_records, 9, "名字只列前几条，数量是全部");
    }

    #[test]
    fn more_than_half_of_records_pauses() {
        let mut local = SyncSnapshot::empty("dev-a", 1);
        for index in 0..10 {
            local.hosts.push(host(&format!("h{index}")));
        }
        let mut target = SyncSnapshot::empty("dev-b", 1);
        target.hosts.push(host("h9"));
        let plan = deletion_plan(&local, &target);
        assert!(plan.needs_confirmation(), "删 9/10 超过一半");
        assert!(plan.summary().contains("9"));
    }

    #[test]
    fn more_than_ten_hosts_pauses_even_when_not_half() {
        let mut local = SyncSnapshot::empty("dev-a", 1);
        for index in 0..100 {
            local.groups.push(group(&format!("g{index}")));
        }
        for index in 0..30 {
            local.hosts.push(host(&format!("h{index}")));
        }
        let mut target = SyncSnapshot::empty("dev-b", 1);
        for index in 0..100 {
            target.groups.push(group(&format!("g{index}")));
        }
        for index in 11..30 {
            target.hosts.push(host(&format!("h{index}")));
        }
        let plan = deletion_plan(&local, &target);
        assert_eq!(plan.deleted_records, 11);
        assert!(plan.deleted_records * 2 <= plan.local_records, "没超过一半");
        assert!(plan.needs_confirmation(), "删了 19 台主机");
    }

    #[test]
    fn empty_local_database_never_asks() {
        let local = SyncSnapshot::empty("dev-a", 1);
        let plan = deletion_plan(&local, &local);
        assert!(!plan.needs_confirmation());
    }

    #[test]
    fn deleted_identity_counts_as_a_record() {
        let mut local = SyncSnapshot::empty("dev-a", 1);
        local.identities.push(crate::sync::package::IdentityRecord {
            uid: "i1".to_owned(),
            name: "密码".to_owned(),
            kind: "password".to_owned(),
            key_path: None,
            secret: None,
            updated_at: 1,
            device_id: "dev-a".to_owned(),
        });
        let target = SyncSnapshot::empty("dev-b", 1);
        let plan = deletion_plan(&local, &target);
        assert_eq!(plan.deleted_records, 1);
        assert!(plan.needs_confirmation());
    }
}
