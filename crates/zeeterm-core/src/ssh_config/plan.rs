//! 预览与计划：解析出的主机怎么变成 ZeeTerm 的主机。纯函数，界面每次勾选变化都可以重算。
//!
//! 几个结论（对照 ZeeTerm 现有行为查清的）：
//! - **跳板机可以链式挂**：`jump_host_id` 指向的主机自己还能有跳板机，拨号链最多
//!   [`MAX_CHAIN_HOPS`] 跳（含目标）。所以 `ProxyJump a,b` 可以串成 目标→b→a。
//! - **用户名**：新建主机表单里 SSH 的用户名不能留空（空了报「必填」，库里也拒绝），表单默认预填 `root`。
//!   配置里没写 `User` 时，OpenSSH 用的是本机用户名，这里照 OpenSSH 来（`default_user`，由调用方传入），
//!   拿不到才退回表单的默认 `root`，并在预览里标出来。
//! - **没写 `IdentityFile`**：和表单里不选凭据一致，认证方式是「密码」、不关联凭据（连接时再问）。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::model::{Host, Identity, IdentityKind, Protocol};
use crate::store::hosts::MAX_CHAIN_HOPS;

use super::load::path_key;
use super::{HostNote, ParsedConfig, ProxyJump};

/// 配置里没写 `User` 又拿不到本机用户名时的默认值（和新建主机表单一致）。
const FALLBACK_USER: &str = "root";

/// 计划要对照的现有数据。
#[derive(Debug, Clone, Copy)]
pub struct ImportContext<'a> {
    pub hosts: &'a [Host],
    pub identities: &'a [Identity],
    /// 配置里没写 `User` 时用的名字（OpenSSH 用本机用户名）；空就用 `root`。
    pub default_user: &'a str,
}

/// 预览里的一行：一台可以导入的主机。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    pub alias: String,
    pub address: String,
    pub port: u16,
    pub username: String,
    /// 用户名是默认值补的（配置里没写）。
    pub user_defaulted: bool,
    /// 私钥路径；没有就是密码认证。
    pub key_path: Option<String>,
    pub proxy_jump: ProxyJump,
    /// ZeeTerm 里已经有地址、端口、用户名都相同的 SSH 主机。
    pub exists: bool,
    pub unsupported: Vec<String>,
    pub notes: Vec<HostNote>,
}

/// 把解析结果整理成预览行，并标出和现有主机重复的。
pub fn build_rows(parsed: &ParsedConfig, ctx: &ImportContext<'_>) -> Vec<ImportRow> {
    let fallback = {
        let name = ctx.default_user.trim();
        if name.is_empty() {
            FALLBACK_USER
        } else {
            name
        }
    };
    parsed
        .hosts
        .iter()
        .map(|host| {
            let written = host
                .user
                .as_deref()
                .map(str::trim)
                .filter(|user| !user.is_empty());
            let username = written.unwrap_or(fallback).to_owned();
            let exists = ctx.hosts.iter().any(|existing| {
                existing.protocol.protocol == Protocol::Ssh
                    && existing.port == host.port
                    && existing.username == username
                    && existing.host.trim().eq_ignore_ascii_case(&host.address)
            });
            ImportRow {
                alias: host.alias.clone(),
                address: host.address.clone(),
                port: host.port,
                username,
                user_defaulted: written.is_none(),
                key_path: host.identity_file.clone(),
                proxy_jump: host.proxy_jump.clone(),
                exists,
                unsupported: host.unsupported.clone(),
                notes: host.notes.clone(),
            }
        })
        .collect()
}

/// 一台主机的跳板机。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpTarget {
    None,
    /// 这次导入里的另一台（行下标）；落库时先建它。
    Row(usize),
    /// ZeeTerm 里已有的同名主机。
    Existing {
        id: i64,
        name: String,
    },
}

/// 一台主机的认证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAuth {
    /// 密码认证，不关联凭据（连接时问）。
    Password,
    /// 私钥认证，用这次要新建的第 n 条凭据。
    NewKey(usize),
    /// 私钥认证，复用已有的同路径凭据。
    ExistingKey(i64),
}

/// 要新建的私钥文件凭据：只记路径，不读文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIdentity {
    pub name: String,
    pub path: String,
}

/// 计划里的补充说明（和勾选状态有关，所以不在解析阶段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanNote {
    /// `ProxyJump none`：没有跳板机。
    JumpDisabled,
    /// 跳板机在配置里找不到对应的别名，也没有同名的现有主机；没设跳板机。
    JumpNotFound(String),
    /// 跳板机是配置里的别名，但这次没勾选导入它，也没有同名的现有主机；没设跳板机。
    JumpNotSelected(String),
    /// 多级跳板没能串起来，只接了最后一跳。
    ChainTruncated(ChainIssue),
    /// 为了串起别的主机的多级跳板，这台被设成了经由 `via` 连接。
    WiredAsJump { via: String },
    /// 跳板机绕成了圈，没设跳板机。
    JumpLoop,
    /// 跳板链超过了 ZeeTerm 允许的级数，没设跳板机。
    JumpTooDeep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainIssue {
    /// 前面某一级（名字）找不到或没勾选。
    HopMissing(String),
    /// 某一级（名字）自己已经设了不同的跳板机，改它会冲突。
    Conflict(String),
}

/// 一行的计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPlan {
    pub jump: JumpTarget,
    /// 只对勾选了的行有意义（没勾选的行不落库，这里恒为 `Password`）。
    pub auth: PlanAuth,
    pub notes: Vec<PlanNote>,
}

/// 按当前勾选算出的整份计划。`rows` 和预览行一一对应（没勾的行也有，用来显示跳板机和提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    pub rows: Vec<RowPlan>,
    /// 要创建的行，按创建顺序（跳板机排在依赖它的主机前面）。
    pub order: Vec<usize>,
    /// 要新建的私钥凭据。
    pub identities: Vec<NewIdentity>,
}

/// 内部用：对跳板机的引用（已经确定会存在）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ref {
    Row(usize),
    Existing(i64),
}

/// 一级 `ProxyJump` 解析的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hop {
    Found(Ref),
    NotSelected(String),
    Unknown(String),
}

pub fn plan_import(rows: &[ImportRow], selected: &[bool], ctx: &ImportContext<'_>) -> ImportPlan {
    let count = rows.len();
    let is_selected = |index: usize| selected.get(index).copied().unwrap_or(false);

    let by_alias: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| (row.alias.as_str(), index))
        .collect();
    // 同名的现有 SSH 主机（跳板机必须是 SSH）：重名时取 id 最小的，结果稳定。
    let mut existing_by_id: HashMap<i64, &Host> = HashMap::new();
    let mut existing_by_name: HashMap<&str, &Host> = HashMap::new();
    for host in ctx.hosts {
        existing_by_id.insert(host.id, host);
        if host.protocol.protocol != Protocol::Ssh {
            continue;
        }
        let slot = existing_by_name.entry(host.name.as_str()).or_insert(host);
        if host.id < slot.id {
            *slot = host;
        }
    }
    let name_of = |target: Ref| -> String {
        match target {
            Ref::Row(index) => rows[index].alias.clone(),
            Ref::Existing(id) => existing_by_id
                .get(&id)
                .map(|host| host.name.clone())
                .unwrap_or_default(),
        }
    };
    let resolve = |hop: &str| -> Hop {
        if let Some(&index) = by_alias.get(hop) {
            if is_selected(index) {
                return Hop::Found(Ref::Row(index));
            }
            return match existing_by_name.get(hop) {
                Some(host) => Hop::Found(Ref::Existing(host.id)),
                None => Hop::NotSelected(hop.to_owned()),
            };
        }
        match existing_by_name.get(hop) {
            Some(host) => Hop::Found(Ref::Existing(host.id)),
            None => Hop::Unknown(hop.to_owned()),
        }
    };

    let mut notes: Vec<Vec<PlanNote>> = vec![Vec::new(); count];
    let mut jump_of: Vec<Option<Ref>> = vec![None; count];
    // 写了多级、并且每一级都找得到的行：完整的链（从最外层到最靠近目标）。
    let mut chains: Vec<Option<Vec<Hop>>> = vec![None; count];

    // 第一步：每一行自己的 ProxyJump 取最后一跳。
    for (index, row) in rows.iter().enumerate() {
        match &row.proxy_jump {
            ProxyJump::Unset => {}
            ProxyJump::Disabled => notes[index].push(PlanNote::JumpDisabled),
            ProxyJump::Hops(hops) => {
                let resolved: Vec<Hop> = hops.iter().map(|hop| resolve(hop)).collect();
                match resolved.last() {
                    Some(Hop::Found(target)) => jump_of[index] = Some(*target),
                    Some(Hop::NotSelected(name)) => {
                        notes[index].push(PlanNote::JumpNotSelected(name.clone()))
                    }
                    Some(Hop::Unknown(name)) => {
                        notes[index].push(PlanNote::JumpNotFound(name.clone()))
                    }
                    None => {}
                }
                if resolved.len() > 1 && jump_of[index].is_some() {
                    chains[index] = Some(resolved);
                }
            }
        }
    }

    // 第二步：多级跳板串成链。目标→最后一跳 已经设好；这里让每一级跳板机经由它前一级，
    // 前提是不和它们已经设好的跳板机冲突。串不起来就只留最后一跳。只处理勾选了的行。
    for index in 0..count {
        if !is_selected(index) {
            continue;
        }
        let Some(hops) = chains[index].take() else {
            continue;
        };
        let mut chain: Vec<Ref> = Vec::new();
        let mut missing: Option<String> = None;
        for hop in &hops {
            match hop {
                Hop::Found(target) => chain.push(*target),
                Hop::NotSelected(name) | Hop::Unknown(name) => {
                    missing.get_or_insert_with(|| name.clone());
                }
            }
        }
        if let Some(name) = missing {
            notes[index].push(PlanNote::ChainTruncated(ChainIssue::HopMissing(name)));
            continue;
        }
        let mut pending: Vec<(usize, Ref)> = Vec::new();
        let mut conflict: Option<String> = None;
        for position in (1..chain.len()).rev() {
            let (hop, previous) = (chain[position], chain[position - 1]);
            let fits = match hop {
                Ref::Row(row) => match jump_of[row] {
                    None => {
                        pending.push((row, previous));
                        true
                    }
                    Some(current) => current == previous,
                },
                Ref::Existing(id) => match (existing_by_id.get(&id), previous) {
                    (Some(host), Ref::Existing(previous_id)) => {
                        host.jump_host_id == Some(previous_id)
                    }
                    _ => false,
                },
            };
            if !fits {
                conflict = Some(name_of(hop));
                break;
            }
        }
        match conflict {
            Some(name) => notes[index].push(PlanNote::ChainTruncated(ChainIssue::Conflict(name))),
            None => {
                for (row, previous) in pending {
                    jump_of[row] = Some(previous);
                    notes[row].push(PlanNote::WiredAsJump {
                        via: name_of(previous),
                    });
                }
            }
        }
    }

    // 第三步：圈和太深的链。先把所有圈拆掉（每个圈在第一个遇到的成员处断开），再查深度。
    for index in 0..count {
        if is_selected(index) && on_cycle(index, &jump_of) {
            jump_of[index] = None;
            notes[index].push(PlanNote::JumpLoop);
        }
    }
    for index in 0..count {
        if is_selected(index) && chain_hops(index, &jump_of, &existing_by_id) > MAX_CHAIN_HOPS {
            jump_of[index] = None;
            notes[index].push(PlanNote::JumpTooDeep);
        }
    }

    // 创建顺序：深度优先，跳板机先于依赖它的主机。
    let mut order = Vec::new();
    let mut visited = vec![false; count];
    for index in 0..count {
        if is_selected(index) {
            visit(index, &jump_of, &mut visited, &mut order);
        }
    }

    // 凭据：同一路径只建一条，已有同路径的私钥文件凭据直接复用，重名加后缀。
    let mut used_names: HashSet<String> = ctx
        .identities
        .iter()
        .map(|identity| identity.name.to_lowercase())
        .collect();
    let existing_keys: HashMap<String, i64> = ctx
        .identities
        .iter()
        .filter(|identity| identity.kind == IdentityKind::KeyFile)
        .filter_map(|identity| {
            let path = identity.key_path.as_deref()?;
            Some((path_key(Path::new(path)), identity.id))
        })
        .collect();
    let mut identities: Vec<NewIdentity> = Vec::new();
    let mut planned_keys: HashMap<String, usize> = HashMap::new();
    let mut auth: Vec<PlanAuth> = vec![PlanAuth::Password; count];
    // 只为勾选了的行算：没勾选的行不会落库，预览里显示认证方式看 `ImportRow::key_path` 就够了。
    for index in 0..count {
        let Some(path) = rows[index].key_path.as_ref().filter(|_| is_selected(index)) else {
            continue;
        };
        let key = path_key(Path::new(path));
        auth[index] = if let Some(&id) = existing_keys.get(&key) {
            PlanAuth::ExistingKey(id)
        } else if let Some(&position) = planned_keys.get(&key) {
            PlanAuth::NewKey(position)
        } else {
            let name = unique_name(&key_display_name(path), &mut used_names);
            identities.push(NewIdentity {
                name,
                path: path.clone(),
            });
            planned_keys.insert(key, identities.len() - 1);
            PlanAuth::NewKey(identities.len() - 1)
        };
    }

    let plans = (0..count)
        .map(|index| RowPlan {
            jump: match jump_of[index] {
                None => JumpTarget::None,
                Some(Ref::Row(row)) => JumpTarget::Row(row),
                Some(Ref::Existing(id)) => JumpTarget::Existing {
                    id,
                    name: name_of(Ref::Existing(id)),
                },
            },
            auth: auth[index].clone(),
            notes: notes[index].clone(),
        })
        .collect();
    ImportPlan {
        rows: plans,
        order,
        identities,
    }
}

/// 从 `start` 顺着跳板机走，能不能绕回 `start`。
fn on_cycle(start: usize, jump_of: &[Option<Ref>]) -> bool {
    let mut current = jump_of[start];
    // 绕回别处的圈（不经过 start）最多走 len 步就会重复，走够了就不再追：那个圈由它自己的成员处理。
    for _ in 0..=jump_of.len() {
        match current {
            Some(Ref::Row(row)) => {
                if row == start {
                    return true;
                }
                current = jump_of[row];
            }
            _ => return false,
        }
    }
    false
}

/// 拨号链有几跳（含目标自己）。已有主机那一段按库里的 `jump_host_id` 数。
fn chain_hops(start: usize, jump_of: &[Option<Ref>], existing: &HashMap<i64, &Host>) -> usize {
    let mut hops = 1;
    let mut current = jump_of[start];
    for _ in 0..=jump_of.len() {
        match current {
            Some(Ref::Row(row)) => {
                hops += 1;
                current = jump_of[row];
            }
            Some(Ref::Existing(id)) => {
                let mut next = Some(id);
                let mut guard = 0;
                while let Some(host_id) = next {
                    hops += 1;
                    guard += 1;
                    if guard > MAX_CHAIN_HOPS + 1 {
                        break;
                    }
                    next = existing.get(&host_id).and_then(|host| host.jump_host_id);
                }
                return hops;
            }
            None => return hops,
        }
    }
    hops
}

fn visit(index: usize, jump_of: &[Option<Ref>], visited: &mut [bool], order: &mut Vec<usize>) {
    if visited[index] {
        return;
    }
    visited[index] = true;
    if let Some(Ref::Row(jump)) = jump_of[index] {
        visit(jump, jump_of, visited, order);
    }
    order.push(index);
}

/// 凭据名：私钥文件名（`id_ed25519`）；路径里取不出来就用通用名。
fn key_display_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("SSH key")
        .to_owned()
}

/// 和已用过的名字（不分大小写）重名就加 ` (2)`、` (3)`……
fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    let mut candidate = base.to_owned();
    let mut suffix = 2;
    while used.contains(&candidate.to_lowercase()) {
        candidate = format!("{base} ({suffix})");
        suffix += 1;
    }
    used.insert(candidate.to_lowercase());
    candidate
}

/// 预览打开时的默认勾选：已经存在的主机不勾（用户可以手动勾上，勾上就照样新建一台）。
pub fn default_selection(rows: &[ImportRow]) -> Vec<bool> {
    rows.iter().map(|row| !row.exists).collect()
}
