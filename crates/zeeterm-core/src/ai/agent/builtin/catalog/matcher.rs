//! 数据库（[`Catalog`]）与"用户填的模型名 → 数据库条目"的匹配。
//!
//! 匹配按这个顺序，命中即停：
//! 1. 完整 id 或 `canonical_model_id` 相等（忽略大小写，依次去掉前面的 `a/` 段再试，
//!    所以 `openrouter/anthropic/claude-opus-5` 也能命中），以及最后一段拆成 token 后相等
//!    （按非字母数字切开，`gpt-5.5`、`gpt-5-5`、`gpt_5.5` 同样是 `gpt 5 5`），两路候选一起排名；
//! 2. 没有的话去掉末尾的日期 / `latest` / `free` 等再按 token 精确匹配；
//! 3. 最长 token 前缀（`gpt-5.5-codex-max` → `gpt-5.5`）；
//! 4. 反过来：用户名是某些条目的前缀（`gemini-3-pro` → `gemini-3-pro-preview`），取多出 token 最少的。
//!
//! 同一把键下有多个候选时：和模型同厂商的优先（厂商来自 `Qwen/` 这类前缀，没有就按模型名推），
//! 其次是厂商自家接口优先于聚合平台，再其次没被标 deprecated 的；仍然并列就取"保守值"——
//! 上下文最小的那一个（再比最大输出）。

use std::collections::HashMap;

use super::data::{CatalogEntry, CatalogFile};
use super::types::CapsSource;

/// 数据库的来源，决定 [`CapsSource`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogOrigin {
    /// 安装包内置快照。
    Snapshot,
    /// 运行时更新的缓存。
    Cache,
    /// 什么都没加载到（只靠规则兜底）。
    Empty,
}

impl CatalogOrigin {
    pub(crate) fn caps_source(self) -> CapsSource {
        match self {
            CatalogOrigin::Cache => CapsSource::ModelsDev,
            _ => CapsSource::Builtin,
        }
    }
}

/// 匹配是怎么命中的（给界面"识别为……"的可信度提示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// id / canonical id 完全相等。
    ExactId,
    /// 模型名规整后（token 序列）相等。
    ExactName,
    /// 条目名是用户名的前缀（用户名多了版本后缀 / 变体）。
    Prefix,
    /// 用户名是条目名的前缀（用户名缺了后缀）。
    Extends,
}

#[derive(Debug, Clone, Copy)]
pub struct CatalogMatch<'a> {
    pub entry: &'a CatalogEntry,
    pub kind: MatchKind,
}

/// 内存里的模型数据库。构造后只读。
#[derive(Debug)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
    origin: CatalogOrigin,
    timestamp: Option<u64>,
    by_full: HashMap<String, Vec<usize>>,
    by_tokens: HashMap<Vec<String>, Vec<usize>>,
    by_key: HashMap<String, usize>,
}

impl Catalog {
    /// 空数据库：所有名字都落到规则兜底。
    pub fn empty() -> Self {
        Self::from_entries(Vec::new(), CatalogOrigin::Empty, None)
    }

    pub fn from_entries(
        entries: Vec<CatalogEntry>,
        origin: CatalogOrigin,
        timestamp: Option<u64>,
    ) -> Self {
        let mut by_full: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_tokens: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
        let mut by_key = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_key.entry(e.key()).or_insert(i);
            let mut fulls = vec![e.id.to_lowercase()];
            let mut lasts = vec![last_segment(&e.id)];
            if let Some(c) = &e.canonical {
                fulls.push(c.to_lowercase());
                lasts.push(last_segment(c));
            }
            for f in fulls {
                push_unique(by_full.entry(f).or_default(), i);
            }
            for l in lasts {
                let t = tokens(l);
                if !t.is_empty() {
                    push_unique(by_tokens.entry(t).or_default(), i);
                }
            }
        }
        Catalog {
            entries,
            origin,
            timestamp,
            by_full,
            by_tokens,
            by_key,
        }
    }

    pub(crate) fn from_file(file: CatalogFile, origin: CatalogOrigin) -> Self {
        let ts = file.timestamp();
        Self::from_entries(file.entries, origin, ts)
    }

    pub fn origin(&self) -> CatalogOrigin {
        self.origin
    }

    /// 数据对应的时间（Unix 秒）：快照是生成当天，缓存是拉取时刻。
    pub fn timestamp(&self) -> Option<u64> {
        self.timestamp
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// 按 [`CatalogEntry::key`] 取条目（用户在设置页改选时用）。
    pub fn get(&self, key: &str) -> Option<&CatalogEntry> {
        self.by_key.get(key).map(|&i| &self.entries[i])
    }

    /// 名字匹配。没有命中返回 None（由调用方走规则兜底）。
    pub fn find(&self, model_name: &str) -> Option<CatalogMatch<'_>> {
        let name = model_name.trim();
        if name.is_empty() || self.entries.is_empty() {
            return None;
        }
        let segments: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();
        let last = *segments.last()?;
        let hint = if segments.len() >= 2 {
            vendor_from_prefix(segments[segments.len() - 2])
        } else {
            None
        };
        let t0 = tokens(last);
        if t0.is_empty() {
            return None;
        }
        let vendor = hint.or_else(|| vendor_from_family(&t0));

        // 1+2. 精确：完整 id（依次去掉最前面的 `xxx/` 段）和最后一段的 token 相等合在一起排名，
        //      这样 `openai/gpt-5.5` 这种聚合平台写法也会优先落到厂商自家的条目上。
        //      聚合平台里别家厂商托管的同 id 条目（如别家的 `glm-5-2`）在完整 id 这一路里不收。
        let mut exact: Vec<usize> = Vec::new();
        let mut by_id: Vec<usize> = Vec::new();
        for start in 0..segments.len() {
            let candidate = segments[start..].join("/").to_lowercase();
            if let Some(idx) = self.by_full.get(&candidate) {
                for &i in idx {
                    if rank(&self.entries[i], vendor).0 == 0 {
                        push_unique(&mut by_id, i);
                        push_unique(&mut exact, i);
                    }
                }
            }
        }
        if let Some(idx) = self.by_tokens.get(t0.as_slice()) {
            for &i in idx {
                push_unique(&mut exact, i);
            }
        }
        if !exact.is_empty() {
            let best = self.pick(&exact, vendor, MatchKind::ExactName)?;
            let kind = if by_id
                .iter()
                .any(|&i| std::ptr::eq(&self.entries[i], best.entry))
            {
                MatchKind::ExactId
            } else {
                MatchKind::ExactName
            };
            return Some(CatalogMatch { kind, ..best });
        }
        // 去掉尾部的日期 / latest / free 再精确一次。
        let t1 = strip_noise(&t0);
        if let Some(idx) = self.by_tokens.get(t1.as_slice()) {
            return self.pick(idx, vendor, MatchKind::ExactName);
        }

        // 3. 最长前缀。
        for n in (1..t0.len()).rev() {
            if !prefix_len_allowed(&t0[..n]) {
                continue;
            }
            if let Some(idx) = self.by_tokens.get(&t0[..n]) {
                return self.pick(idx, vendor, MatchKind::Prefix);
            }
        }

        // 4. 用户名是条目名的前缀：取多出 token 最少的那一批。至少两个 token，免得
        //    `gpt` 这种泛名字乱挂。
        if t1.len() >= 2 {
            let mut best: Vec<usize> = Vec::new();
            let mut best_extra = usize::MAX;
            for (key, idx) in &self.by_tokens {
                if key.len() > t1.len() && key[..t1.len()] == t1[..] {
                    let extra = key.len() - t1.len();
                    if extra < best_extra {
                        best_extra = extra;
                        best.clear();
                    }
                    if extra == best_extra {
                        for &i in idx {
                            push_unique(&mut best, i);
                        }
                    }
                }
            }
            if !best.is_empty() {
                return self.pick(&best, vendor, MatchKind::Extends);
            }
        }
        None
    }

    fn pick(
        &self,
        candidates: &[usize],
        vendor: Option<Vendor>,
        kind: MatchKind,
    ) -> Option<CatalogMatch<'_>> {
        let best = candidates.iter().copied().min_by(|&a, &b| {
            let (ea, eb) = (&self.entries[a], &self.entries[b]);
            rank(ea, vendor)
                .cmp(&rank(eb, vendor))
                // 并列取保守值：上下文小的、最大输出小的；最后按键排，结果稳定。
                .then(ea.context.cmp(&eb.context))
                .then(ea.output.cmp(&eb.output))
                .then_with(|| ea.key().cmp(&eb.key()))
        })?;
        Some(CatalogMatch {
            entry: &self.entries[best],
            kind,
        })
    }
}

fn push_unique(v: &mut Vec<usize>, i: usize) {
    if !v.contains(&i) {
        v.push(i);
    }
}

fn rank(e: &CatalogEntry, vendor: Option<Vendor>) -> (u8, u8, u8) {
    let mismatch = match (vendor, entry_vendor(e)) {
        (Some(want), Some(have)) => want != have,
        // 查询厂商未知时不区分厂商；条目厂商未知（聚合平台里的冷门前缀）排后。
        (Some(_), None) => true,
        (None, _) => false,
    };
    (
        u8::from(mismatch),
        u8::from(e.provider == "openrouter"),
        u8::from(e.deprecated),
    )
}

// ---- 名字规整 ----

fn last_segment(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// 小写，按非字母数字切开。`5.5` 与 `5-5` 因此相同；`55` 不同（有意：不猜）。
pub(crate) fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// 去掉尾部的日期、`latest`、OpenRouter 的 `free` / `nitro` / `floor` / `online`。
fn strip_noise(t: &[String]) -> Vec<String> {
    let mut out = t.to_vec();
    loop {
        let before = out.len();
        if out.len() > 1 {
            let last = out[out.len() - 1].as_str();
            if matches!(last, "latest" | "free" | "nitro" | "floor" | "online")
                || is_date_token(last)
            {
                out.pop();
            }
        }
        // `2026-10-01` 三段式日期。
        if out.len() > 3 {
            let n = out.len();
            if out[n - 3].len() == 4
                && out[n - 2].len() == 2
                && out[n - 1].len() == 2
                && out[n - 3..]
                    .iter()
                    .all(|x| x.chars().all(|c| c.is_ascii_digit()))
                && out[n - 3].starts_with("20")
            {
                out.truncate(n - 3);
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// 8 位（YYYYMMDD）、6 位（YYMMDD）或 4 位（YYMM，厂商常用，如 `-2507`）的纯数字。
fn is_date_token(t: &str) -> bool {
    if !t.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let num = |a: usize, b: usize| t[a..b].parse::<u32>().unwrap_or(99);
    match t.len() {
        8 => (2000..2100).contains(&num(0, 4)) && (1..=12).contains(&num(4, 6)),
        6 => (20..40).contains(&num(0, 2)) && (1..=12).contains(&num(2, 4)),
        4 => (20..40).contains(&num(0, 2)) && (1..=12).contains(&num(2, 4)),
        _ => false,
    }
}

/// 前缀匹配的下限：单个 token 的前缀必须像 `o3`、`qwen3` 这样带数字，免得 `claude` 一个词
/// 就把所有名字拉到某个具体条目上。
fn prefix_len_allowed(prefix: &[String]) -> bool {
    match prefix {
        [] => false,
        [one] => one.len() >= 2 && one.chars().any(|c| c.is_ascii_digit()),
        _ => true,
    }
}

// ---- 厂商 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vendor {
    OpenAi,
    Anthropic,
    Google,
    DeepSeek,
    Alibaba,
    Moonshot,
    Zhipu,
    MiniMax,
    Xai,
    Meta,
    Mistral,
}

/// `Qwen/…`、`deepseek-ai/…`、`z-ai/…` 之类的前缀 → 厂商。
fn vendor_from_prefix(p: &str) -> Option<Vendor> {
    use Vendor::*;
    Some(match p.trim_start_matches('~').to_lowercase().as_str() {
        "openai" => OpenAi,
        "anthropic" => Anthropic,
        "google" | "gemini" => Google,
        "deepseek" | "deepseek-ai" => DeepSeek,
        "qwen" | "alibaba" | "alibaba-cn" | "aliyun" => Alibaba,
        "moonshotai" | "moonshot" | "moonshotai-cn" => Moonshot,
        "z-ai" | "zai" | "zai-org" | "zhipuai" | "thudm" | "glm" => Zhipu,
        "minimax" | "minimaxai" | "minimax-cn" => MiniMax,
        "x-ai" | "xai" => Xai,
        "meta" | "meta-llama" => Meta,
        "mistralai" | "mistral" => Mistral,
        _ => return None,
    })
}

/// 没有前缀时按模型名第一个 token 推厂商。
fn vendor_from_family(t: &[String]) -> Option<Vendor> {
    use Vendor::*;
    let first = t.first()?.as_str();
    let is_o_series = first.len() >= 2
        && first.starts_with('o')
        && first[1..].chars().all(|c| c.is_ascii_digit());
    Some(match first {
        _ if is_o_series => OpenAi,
        "gpt" | "chatgpt" | "codex" => OpenAi,
        "claude" => Anthropic,
        "gemini" | "gemma" => Google,
        "deepseek" => DeepSeek,
        "qwen" | "qwq" | "qvq" => Alibaba,
        _ if first.starts_with("qwen") => Alibaba,
        "kimi" | "moonshot" => Moonshot,
        "glm" | "chatglm" => Zhipu,
        "minimax" => MiniMax,
        "grok" => Xai,
        "llama" | "muse" => Meta,
        "mistral" | "ministral" | "magistral" | "devstral" | "codestral" | "pixtral"
        | "voxtral" => Mistral,
        _ => return None,
    })
}

/// 条目属于哪个厂商：厂商自家接口看 provider id；聚合平台看 id 里的前缀。
fn entry_vendor(e: &CatalogEntry) -> Option<Vendor> {
    if e.provider == "openrouter" {
        return e.id.split('/').next().and_then(vendor_from_prefix);
    }
    vendor_from_prefix(&e.provider)
}
