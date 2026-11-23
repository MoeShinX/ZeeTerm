//! 把展开后的段按 OpenSSH 的规则合并成每个别名的设置：对每个可导入的别名，按文件顺序走一遍所有段，
//! 匹配上的段里「还没取到值」的字段才填；所以最先出现的值生效。

use std::collections::HashSet;
use std::path::Path;

use super::load::{join_native, Block, Loaded, Scope, Setting};
use super::syntax::{host_line_matches, is_importable_alias};
use super::{
    ConfigHost, HostNote, ParseOptions, ParsedConfig, ProxyJump, SkipReason, SkippedEntry,
};

/// 导入时认得的关键字（其余的都进「没有导入」列表）。`Include` / `Host` / `Match` 在展开阶段就处理掉了。
const HANDLED: &[&str] = &["hostname", "user", "port", "identityfile", "proxyjump"];

pub(super) fn resolve(loaded: Loaded, options: &ParseOptions) -> ParsedConfig {
    let mut parsed = ParsedConfig {
        notices: loaded.notices,
        ..ParsedConfig::default()
    };
    for alias in aliases(&loaded.blocks) {
        match resolve_alias(&alias, &loaded.blocks, options) {
            Ok(host) => parsed.hosts.push(host),
            Err(reason) => parsed.skipped.push(SkippedEntry { alias, reason }),
        }
    }
    parsed
}

/// 所有 `Host` 行里可导入的名字，按第一次出现的顺序，重复的只算一次。
fn aliases(blocks: &[Block]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for block in blocks {
        if let Scope::Hosts(patterns) = &block.scope {
            for pattern in patterns {
                if is_importable_alias(pattern) && seen.insert(pattern.clone()) {
                    out.push(pattern.clone());
                }
            }
        }
    }
    out
}

/// 一个别名取到的原始值（还没有校验）。
#[derive(Default)]
struct Values {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<String>,
    proxy_jump: Option<String>,
    identity_files: Vec<String>,
    unsupported: Vec<String>,
}

fn resolve_alias(
    alias: &str,
    blocks: &[Block],
    options: &ParseOptions,
) -> Result<ConfigHost, SkipReason> {
    let mut values = Values::default();
    for block in blocks {
        let applies = match &block.scope {
            Scope::Global => true,
            Scope::Hosts(patterns) => host_line_matches(patterns, alias),
            Scope::Match => false,
        };
        if applies {
            for setting in &block.settings {
                apply(&mut values, setting);
            }
        }
    }

    let mut notes = Vec::new();

    let hostname = values.hostname.unwrap_or_else(|| alias.to_owned());
    let address = hostname.replace("%h", alias);
    if address.contains('%') {
        notes.push(HostNote::PercentInHostName);
    }
    // 地址要和「新建主机」同一个标准：之后落库时同样的检查会拦下不合格的，这里提前说清原因。
    if crate::ssh::known_hosts::check_host_address(&address).is_err() {
        return Err(SkipReason::BadAddress(address));
    }

    let port = match &values.port {
        None => 22,
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|port| (1..=65535).contains(port))
            .map(|port| port as u16)
            .ok_or_else(|| SkipReason::BadPort(raw.clone()))?,
    };

    let identity_file = values.identity_files.first().map(|raw| {
        if raw.contains('%') {
            notes.push(HostNote::PercentInIdentityFile);
        }
        expand_identity_path(raw, options)
    });
    if values.identity_files.len() > 1 {
        notes.push(HostNote::ExtraIdentityFiles(
            values.identity_files.len() - 1,
        ));
    }

    let proxy_jump = match values.proxy_jump {
        None => ProxyJump::Unset,
        Some(raw) if raw.trim().eq_ignore_ascii_case("none") => ProxyJump::Disabled,
        Some(raw) => {
            let hops: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|hop| !hop.is_empty())
                .map(str::to_owned)
                .collect();
            if hops.is_empty() {
                ProxyJump::Unset
            } else {
                ProxyJump::Hops(hops)
            }
        }
    };

    Ok(ConfigHost {
        alias: alias.to_owned(),
        address,
        port,
        user: values.user,
        identity_file,
        proxy_jump,
        unsupported: values.unsupported,
        notes,
    })
}

/// 把一个设置并进已取到的值：单值字段只有还没取到才填；`IdentityFile` 是累加的（取第一个，数个数）。
fn apply(values: &mut Values, setting: &Setting) {
    let first = || setting.args.first().cloned();
    match setting.key.as_str() {
        "hostname" => {
            if values.hostname.is_none() {
                values.hostname = first();
            }
        }
        "user" => {
            if values.user.is_none() {
                values.user = first();
            }
        }
        "port" => {
            if values.port.is_none() {
                values.port = first();
            }
        }
        "proxyjump" => {
            if values.proxy_jump.is_none() {
                // ProxyJump 的值可以写成 `a, b`（逗号后带空格）：参数被空白切开了，拼回去再按逗号分。
                if !setting.args.is_empty() {
                    values.proxy_jump = Some(setting.args.join(" "));
                }
            }
        }
        "identityfile" => {
            if let Some(path) = first() {
                values.identity_files.push(path);
            }
        }
        key if HANDLED.contains(&key) => {}
        _ => {
            if !values
                .unsupported
                .iter()
                .any(|seen| seen.eq_ignore_ascii_case(&setting.key_raw))
            {
                values.unsupported.push(setting.key_raw.clone());
            }
        }
    }
}

/// `IdentityFile`：开头的 `~` 展开成用户目录，相对路径按 OpenSSH 的规则相对 `ssh_dir`，绝对路径原样。
/// 只是拼字符串，不打开、不检查那个文件。
fn expand_identity_path(raw: &str, options: &ParseOptions) -> String {
    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| (raw == "~").then_some(""))
    {
        return join_native(&options.home, rest)
            .to_string_lossy()
            .into_owned();
    }
    let path = Path::new(raw);
    if path.is_absolute() || path.has_root() || raw.starts_with('%') {
        return raw.to_owned();
    }
    join_native(&options.ssh_dir, raw)
        .to_string_lossy()
        .into_owned()
}
