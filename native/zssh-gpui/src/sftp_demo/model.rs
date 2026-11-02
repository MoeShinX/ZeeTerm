//! 文件栏的纯数据规则：条目、排序、过滤、多选和键盘移动、面包屑、浏览历史、列。
//!
//! 对照 0.2.14 的 `state/sftpPane.ts`、`state/sftpNav.ts`、`state/dockLayout.ts`
//! 和 `SftpFilePane.tsx` 里的列定义。这里没有界面和网络，本地栏和远端栏共用，
//! 每条规则都有单测（见文件末尾）。
//!
//! 选择、锚点和键盘焦点都按**路径**记，不按行号：排序、过滤、后台刷新之后行号对应的
//! 可能是别的文件，按行号记就会选错、删错。

use std::cmp::Ordering;
use std::collections::HashSet;

use zssh_core::sftp::{DirEntry, FileKind};

use crate::main_window::collation;

/// 条目类型（本地栏多一种“盘符”）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
    Drive,
}

/// 符号链接指向的类型；断链和拿不到是 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetKind {
    Dir,
    File,
}

/// 两栏通用的一行。远端来自 [`DirEntry`]，本地来自 `std::fs`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) name: String,
    /// 远端是 POSIX 路径，本地是 Windows 路径。
    pub(super) path: String,
    pub(super) kind: EntryKind,
    pub(super) target_kind: Option<TargetKind>,
    pub(super) link_target: Option<String>,
    pub(super) size: Option<u64>,
    /// Unix 秒。
    pub(super) mtime: Option<u64>,
    pub(super) permissions: Option<u32>,
    pub(super) owner: Option<String>,
    pub(super) hidden: bool,
    /// 名字不是合法 UTF-8 时，界面标灰、不让操作。
    pub(super) name_is_utf8: bool,
}

impl Entry {
    pub(super) fn from_remote(entry: &DirEntry) -> Self {
        let (kind, target_kind) = match &entry.kind {
            FileKind::Dir => (EntryKind::Dir, None),
            FileKind::File => (EntryKind::File, None),
            FileKind::Symlink { target } => (
                EntryKind::Symlink,
                match target.as_deref() {
                    Some(FileKind::Dir) => Some(TargetKind::Dir),
                    Some(FileKind::File) => Some(TargetKind::File),
                    _ => None,
                },
            ),
            FileKind::Other(_) => (EntryKind::Other, None),
        };
        Self {
            name: entry.name.clone(),
            path: entry.path.clone(),
            kind,
            target_kind,
            link_target: entry.link_target.clone(),
            size: entry.size,
            mtime: entry.mtime.map(u64::from),
            permissions: entry.permissions,
            owner: entry.owner.clone(),
            // 远端按“名字以点开头”算隐藏文件。
            hidden: entry.name.starts_with('.'),
            name_is_utf8: entry.name_is_utf8,
        }
    }

    /// 目录、盘符，或者指向目录的链接：双击能进去。
    pub(super) fn is_enterable(&self) -> bool {
        match self.kind {
            EntryKind::Dir | EntryKind::Drive => true,
            EntryKind::Symlink => self.target_kind == Some(TargetKind::Dir),
            _ => false,
        }
    }

    /// 文件，或者指向文件的链接：双击能打开。
    pub(super) fn is_file_like(&self) -> bool {
        match self.kind {
            EntryKind::File => true,
            EntryKind::Symlink => self.target_kind == Some(TargetKind::File),
            _ => false,
        }
    }

    pub(super) fn is_drive(&self) -> bool {
        self.kind == EntryKind::Drive
    }

    /// 排序时目录（和盘符）排在文件前面。
    fn dir_first(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Drive)
    }

    /// 这一行要不要按「链接」画（V2-U1 第 9 条：Xftp 那种）：名称列只写链接自己的名字，图标按
    /// **目标类型**画，图标左下角叠一个小的快捷方式箭头；目标路径放进悬停提示和属性对话框。
    ///
    /// 远端的链接是 `kind == Symlink`；本地的链接（符号链接、目录联接）在列表里是跟随之后的类型
    /// （`Dir` / `File`，操作都按目标走），靠 `link_target`（`read_link` 读到的）认出来。
    /// 目标不存在 / 读不了的是断链：角标用警示色，提示里写明。没有 `link_target` 的 `Symlink`
    /// 照样按链接画，只是提示里没有路径可写。
    pub(super) fn link_info(&self) -> Option<LinkInfo> {
        let is_link = self.kind == EntryKind::Symlink || self.link_target.is_some();
        if !is_link {
            return None;
        }
        let (base, broken) = match (self.kind, self.target_kind) {
            (EntryKind::Symlink, Some(TargetKind::Dir)) => (LinkBase::Folder, false),
            (EntryKind::Symlink, Some(TargetKind::File)) => (LinkBase::File, false),
            // 远端：目标类型拿不到 = 断链（或没权限解析）。
            (EntryKind::Symlink, None) => (LinkBase::File, true),
            // 本地：已经跟随过了，类型就是目标的类型；`Other` 是目标读不了。
            (EntryKind::Dir, _) => (LinkBase::Folder, false),
            (EntryKind::File, _) => (LinkBase::File, false),
            _ => (LinkBase::File, true),
        };
        Some(LinkInfo {
            base,
            broken,
            target: self.link_target.clone(),
        })
    }
}

/// 链接图标的底：目标是文件夹画文件夹，是文件（或断链）画文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkBase {
    Folder,
    File,
}

/// 一个链接怎么画（见 [`Entry::link_info`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LinkInfo {
    pub(super) base: LinkBase,
    pub(super) broken: bool,
    pub(super) target: Option<String>,
}

impl LinkInfo {
    /// 悬停提示：「链接到：<目标>」；断链再写一句原因。
    pub(super) fn tooltip(&self) -> String {
        let mut text = match &self.target {
            Some(target) => format!("链接到：{target}"),
            None => "链接".to_owned(),
        };
        if self.broken {
            text.push_str("\n目标不存在或读不了（断链）");
        }
        text
    }
}

/// 换目录的方式：`Visit` 用户主动去（记进历史）；`History` 后退 / 前进（不改历史）；
/// `Refresh` 刷新当前目录（保留选择）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NavMode {
    Visit,
    History,
    Refresh,
}

// ---------------------------------------------------------------------------
// 排序和过滤
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SortKey {
    Name,
    Size,
    Mtime,
    Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SortOrder {
    pub(super) key: SortKey,
    /// `false` = 升序；目录恒在文件前面，不管升降。
    pub(super) descending: bool,
}

impl SortOrder {
    pub(super) const DEFAULT: Self = Self {
        key: SortKey::Name,
        descending: false,
    };

    /// 点表头：同一列翻转方向，换一列从升序开始。
    pub(super) fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                descending: !self.descending,
            }
        } else {
            Self {
                key,
                descending: false,
            }
        }
    }
}

fn kind_rank(entry: &Entry) -> u8 {
    match entry.kind {
        EntryKind::Drive => 0,
        EntryKind::Dir => 1,
        EntryKind::Symlink => 2,
        EntryKind::File => 3,
        EntryKind::Other => 4,
    }
}

/// 目录排在文件前面（不管按什么排）；同类按 `order`。名字按拼音、数字按数值、不分大小写。
pub(super) fn sort_entries(entries: &mut [Entry], order: SortOrder) {
    entries.sort_by(|a, b| {
        let dirs = b.dir_first().cmp(&a.dir_first());
        if dirs != Ordering::Equal {
            return dirs;
        }
        let diff = match order.key {
            SortKey::Name => collation::compare(&a.name, &b.name),
            // 大小 / 时间相同的再按名字排，同一个目录每次显示的顺序都一样。
            SortKey::Size => a
                .size
                .unwrap_or(0)
                .cmp(&b.size.unwrap_or(0))
                .then_with(|| collation::compare(&a.name, &b.name)),
            SortKey::Mtime => a
                .mtime
                .unwrap_or(0)
                .cmp(&b.mtime.unwrap_or(0))
                .then_with(|| collation::compare(&a.name, &b.name)),
            SortKey::Kind => kind_rank(a)
                .cmp(&kind_rank(b))
                .then_with(|| collation::compare(&a.name, &b.name)),
        };
        if order.descending {
            diff.reverse()
        } else {
            diff
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Filter {
    /// 按名字过滤（不分大小写的子串）。
    pub(super) text: String,
    pub(super) show_hidden: bool,
}

/// 过滤；不改变顺序，所以“先排序再过滤”和“先过滤再排序”结果一样。
pub(super) fn filter_entries(sorted: &[Entry], filter: &Filter) -> Vec<Entry> {
    let needle = filter.text.trim().to_lowercase();
    sorted
        .iter()
        .filter(|entry| filter.show_hidden || !entry.hidden || entry.is_drive())
        .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// 排序 + 过滤一条龙。
pub(super) fn visible_entries(raw: &[Entry], order: SortOrder, filter: &Filter) -> Vec<Entry> {
    let mut sorted = raw.to_vec();
    sort_entries(&mut sorted, order);
    filter_entries(&sorted, filter)
}

// ---------------------------------------------------------------------------
// 选择（Ctrl / Shift / 键盘）
// ---------------------------------------------------------------------------

/// 一栏的选择状态：全按路径记。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) paths: HashSet<String>,
    /// Shift 范围选择的起点。
    pub(super) anchor: Option<String>,
    /// 键盘焦点所在的那一行（有选中时画一圈边）。
    pub(super) focused: Option<String>,
}

fn index_of(rows: &[Entry], path: Option<&String>) -> Option<usize> {
    let path = path?;
    rows.iter().position(|entry| &entry.path == path)
}

impl Selection {
    pub(super) fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
        self.focused = None;
    }

    pub(super) fn contains(&self, entry: &Entry) -> bool {
        self.paths.contains(&entry.path)
    }

    /// 当前列表里选中的行号（不在列表里的——被过滤掉、刷新后没了——不算）。
    #[cfg(test)]
    pub(super) fn indices(&self, rows: &[Entry]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, entry)| self.paths.contains(&entry.path))
            .map(|(index, _)| index)
            .collect()
    }

    /// 选中的条目，按显示顺序。
    pub(super) fn entries<'a>(&self, rows: &'a [Entry]) -> Vec<&'a Entry> {
        rows.iter()
            .filter(|entry| self.paths.contains(&entry.path))
            .collect()
    }

    fn set_range(&mut self, rows: &[Entry], from: usize, to: usize) {
        let (from, to) = (from.min(to), from.max(to));
        self.paths = rows[from..=to].iter().map(|e| e.path.clone()).collect();
    }

    /// 点一行。裸点只选它；Ctrl+点切换；Shift+点从锚点到它整段选上（没有锚点当裸点）。
    pub(super) fn click(&mut self, rows: &[Entry], index: usize, ctrl: bool, shift: bool) {
        let Some(entry) = rows.get(index) else {
            return;
        };
        let anchor = index_of(rows, self.anchor.as_ref());
        if shift && let Some(anchor) = anchor {
            self.set_range(rows, anchor, index);
        } else if ctrl {
            if !self.paths.remove(&entry.path) {
                self.paths.insert(entry.path.clone());
            }
            self.anchor = Some(entry.path.clone());
        } else {
            self.paths = HashSet::from([entry.path.clone()]);
            self.anchor = Some(entry.path.clone());
        }
        self.focused = Some(entry.path.clone());
    }

    /// 右键点在一行上：已经选中的保持（对整个选择操作），没选中的把选择换成它。
    pub(super) fn right_click(&mut self, rows: &[Entry], index: usize) {
        let Some(entry) = rows.get(index) else {
            return;
        };
        if !self.paths.contains(&entry.path) {
            self.paths = HashSet::from([entry.path.clone()]);
            self.anchor = Some(entry.path.clone());
        }
        self.focused = Some(entry.path.clone());
    }

    /// 键盘上下移动；带 Shift 是扩展选择。返回新的焦点行号。
    pub(super) fn arrow(&mut self, rows: &[Entry], delta: isize, shift: bool) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        let current = index_of(rows, self.focused.as_ref()).unwrap_or(0);
        let next = (current as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
        if shift {
            let start = index_of(rows, self.anchor.as_ref()).unwrap_or(current);
            self.set_range(rows, start, next);
            self.anchor = Some(rows[start].path.clone());
        } else {
            self.paths = HashSet::from([rows[next].path.clone()]);
            self.anchor = Some(rows[next].path.clone());
        }
        self.focused = Some(rows[next].path.clone());
        Some(next)
    }

    /// Home / End 之类跳到某一行（带 Shift 扩展选择）。
    pub(super) fn jump(&mut self, rows: &[Entry], target: usize, shift: bool) -> Option<usize> {
        if rows.is_empty() {
            return None;
        }
        let current = index_of(rows, self.focused.as_ref()).unwrap_or(0) as isize;
        self.arrow(rows, target as isize - current, shift)
    }

    pub(super) fn select_all(&mut self, rows: &[Entry]) {
        self.paths = rows.iter().map(|e| e.path.clone()).collect();
        if let Some(first) = rows.first() {
            self.anchor = Some(first.path.clone());
        }
    }

    /// 要求选中这些路径（压缩 / 解压完成后选中新文件）；返回有没有选上。
    pub(super) fn select_paths(&mut self, rows: &[Entry], wanted: &[String]) -> bool {
        let present: Vec<&Entry> = rows
            .iter()
            .filter(|entry| wanted.contains(&entry.path))
            .collect();
        let Some(first) = present.first() else {
            return false;
        };
        self.anchor = Some(first.path.clone());
        self.focused = Some(first.path.clone());
        self.paths = present.iter().map(|e| e.path.clone()).collect();
        true
    }

    /// 列表换了（刷新、过滤）：只留下还在列表里的。
    pub(super) fn retain_present(&mut self, rows: &[Entry]) {
        let present: HashSet<&String> = rows.iter().map(|e| &e.path).collect();
        self.paths.retain(|path| present.contains(path));
        if self.anchor.as_ref().is_some_and(|a| !present.contains(a)) {
            self.anchor = None;
        }
        if self.focused.as_ref().is_some_and(|f| !present.contains(f)) {
            self.focused = None;
        }
    }
}

// ---------------------------------------------------------------------------
// 面包屑和上一级
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Crumb {
    pub(super) name: String,
    pub(super) path: String,
}

/// 远端路径的面包屑：`/tmp/a/b` → tmp、a、b；根是空列表。
pub(super) fn remote_crumbs(path: &str) -> Vec<Crumb> {
    let mut built = String::new();
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            built = format!("{built}/{part}");
            Crumb {
                name: part.to_owned(),
                path: built.clone(),
            }
        })
        .collect()
}

/// 本地路径的根和根下面的部分。盘符：`C:\a` → (`C:`, `a`)；网络共享：`\\server\share\a` →
/// (`\\server\share`, `a`)；带 `\\?\` 长路径前缀的也认。
fn split_local_root(path: &str) -> (String, String) {
    let mut normalized = path.replace('/', "\\");
    let lower = normalized.to_lowercase();
    if lower.starts_with("\\\\?\\unc\\") {
        normalized = format!("\\\\{}", &normalized[8..]);
    } else if normalized.starts_with("\\\\?\\") {
        normalized = normalized[4..].to_owned();
    }
    if let Some(rest) = normalized.strip_prefix("\\\\") {
        let mut parts = rest.splitn(3, '\\');
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        let tail = parts.next().unwrap_or("");
        let root = if share.is_empty() {
            format!("\\\\{server}")
        } else {
            format!("\\\\{server}\\{share}")
        };
        return (root, tail.to_owned());
    }
    let root: String = normalized.chars().take(2).collect();
    let rest: String = normalized.chars().skip(3).collect();
    (root, rest)
}

/// 本地路径的面包屑：`C:\Users\x` → `C:`、Users、x；“此电脑”（空）是空列表。
pub(super) fn local_crumbs(path: &str) -> Vec<Crumb> {
    if path.is_empty() {
        return Vec::new();
    }
    let (root, rest) = split_local_root(path);
    let mut out = vec![Crumb {
        name: root.clone(),
        path: format!("{root}\\"),
    }];
    let mut built = format!("{root}\\");
    for part in rest.split('\\').filter(|part| !part.is_empty()) {
        built = if built.ends_with('\\') {
            format!("{built}{part}")
        } else {
            format!("{built}\\{part}")
        };
        out.push(Crumb {
            name: part.to_owned(),
            path: built.clone(),
        });
    }
    out
}

/// 本地上一级：`C:\a\b` → `C:\a`；`C:\` → 空（此电脑）。
pub(super) fn local_parent(path: &str) -> String {
    let crumbs = local_crumbs(path);
    if crumbs.len() <= 1 {
        return String::new();
    }
    crumbs[crumbs.len() - 2].path.clone()
}

/// 远端上一级：`/tmp/a` → `/tmp`；根还是根。
pub(super) fn remote_parent(path: &str) -> String {
    match path.rfind('/') {
        Some(cut) if cut > 0 => path[..cut].to_owned(),
        _ => "/".to_owned(),
    }
}

/// 一段面包屑大约占多宽（px）：13px 字号，汉字约 13、其余约 7，加按钮内边距和分隔箭头。
pub(super) fn crumb_width_estimate(name: &str) -> f32 {
    let text: f32 = name
        .chars()
        .map(|c| if (c as u32) > 0x2e7f { 13.0 } else { 7.0 })
        .sum();
    text + 12.0 + 11.0
}

const ELLIPSIS_WIDTH: f32 = 28.0;

/// 面包屑放不下时：最后几级优先保留，前面的折叠成“…”。至少显示最后一级。
/// 返回 (折叠掉的, 显示的)。
pub(super) fn collapse_crumbs(crumbs: &[Crumb], available: f32) -> (Vec<Crumb>, Vec<Crumb>) {
    if crumbs.is_empty() || available <= 0.0 {
        return (Vec::new(), crumbs.to_vec());
    }
    let total: f32 = crumbs.iter().map(|c| crumb_width_estimate(&c.name)).sum();
    if total <= available {
        return (Vec::new(), crumbs.to_vec());
    }
    let mut used = ELLIPSIS_WIDTH;
    let mut keep = 0usize;
    for crumb in crumbs.iter().rev() {
        let width = crumb_width_estimate(&crumb.name);
        if keep > 0 && used + width > available {
            break;
        }
        used += width;
        keep += 1;
    }
    let cut = crumbs.len() - keep;
    (crumbs[..cut].to_vec(), crumbs[cut..].to_vec())
}

// ---------------------------------------------------------------------------
// 浏览历史（后退 / 前进）
// ---------------------------------------------------------------------------

const HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct NavHistory {
    stack: Vec<String>,
    index: Option<usize>,
}

impl NavHistory {
    pub(super) fn current(&self) -> Option<&str> {
        self.index
            .and_then(|i| self.stack.get(i))
            .map(String::as_str)
    }

    /// 进了一个新目录：丢掉“前进”那一截，接在后面；和当前一样（刷新）不算一步。
    pub(super) fn visit(&mut self, path: &str) {
        if self.current() == Some(path) {
            return;
        }
        let keep = self.index.map_or(0, |i| i + 1);
        self.stack.truncate(keep);
        self.stack.push(path.to_owned());
        if self.stack.len() > HISTORY_LIMIT {
            let extra = self.stack.len() - HISTORY_LIMIT;
            self.stack.drain(..extra);
        }
        self.index = Some(self.stack.len() - 1);
    }

    pub(super) fn can_back(&self) -> bool {
        self.index.is_some_and(|i| i > 0)
    }

    pub(super) fn can_forward(&self) -> bool {
        self.index.is_some_and(|i| i + 1 < self.stack.len())
    }

    /// 后退（-1）/ 前进（+1）一步：返回要去的目录并移动指针；走不了返回 `None`。
    pub(super) fn step(&mut self, delta: isize) -> Option<String> {
        let target = self.index? as isize + delta;
        if target < 0 {
            return None;
        }
        let path = self.stack.get(target as usize)?.clone();
        self.index = Some(target as usize);
        Some(path)
    }

    /// 去不了的目录（被删了、没权限）从历史里拿掉，免得后退一直撞墙。
    pub(super) fn forget(&mut self, path: &str) {
        let before = self.index.map_or(0, |i| i + 1);
        let kept_before = self.stack[..before.min(self.stack.len())]
            .iter()
            .filter(|p| p.as_str() != path)
            .count();
        self.stack.retain(|p| p != path);
        self.index = if self.stack.is_empty() {
            None
        } else {
            Some(kept_before.saturating_sub(1).min(self.stack.len() - 1))
        };
    }
}

// ---------------------------------------------------------------------------
// 列
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Column {
    Name,
    Size,
    Mtime,
    Kind,
    Perms,
    Owner,
}

impl Column {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Size => "大小",
            Self::Mtime => "修改时间",
            Self::Kind => "类型",
            Self::Perms => "权限",
            Self::Owner => "所有者",
        }
    }

    /// 点表头能按它排序吗（权限、所有者不能）。
    pub(super) fn sort_key(self) -> Option<SortKey> {
        match self {
            Self::Name => Some(SortKey::Name),
            Self::Size => Some(SortKey::Size),
            Self::Mtime => Some(SortKey::Mtime),
            Self::Kind => Some(SortKey::Kind),
            Self::Perms | Self::Owner => None,
        }
    }

    /// 列宽（px）；名称列吃剩下的空间。
    pub(super) fn width(self) -> Option<f32> {
        match self {
            Self::Name => None,
            Self::Size => Some(64.0),
            Self::Mtime => Some(128.0),
            Self::Kind => Some(48.0),
            Self::Perms => Some(48.0),
            Self::Owner => Some(80.0),
        }
    }

    pub(super) fn right_aligned(self) -> bool {
        self == Self::Size
    }
}

/// 完整列：远端多出权限和所有者。
pub(super) fn full_columns(is_remote: bool) -> Vec<Column> {
    if is_remote {
        vec![
            Column::Name,
            Column::Size,
            Column::Mtime,
            Column::Kind,
            Column::Perms,
            Column::Owner,
        ]
    } else {
        vec![Column::Name, Column::Size, Column::Mtime, Column::Kind]
    }
}

/// 标签页里的两栏在窗口窄时也要看得清名字：按这一栏的像素宽度从右往左依次藏掉
/// 所有者、权限、类型、修改时间，直到名称至少有 150px（0.2.14 固定显示全部列，窄窗口里名称会被挤没）。
/// 宽度还没量到（0）时给完整列。
pub(super) fn adaptive_columns(is_remote: bool, pane_width: f32) -> Vec<Column> {
    let mut columns = full_columns(is_remote);
    if pane_width <= 0.0 {
        return columns;
    }
    let name_room = |columns: &[Column]| {
        // 左右内边距 16 + 图标 16 + 间隙；每个数据列 = 列宽 + 8px 间隙。
        let fixed: f32 = columns
            .iter()
            .filter_map(|c| c.width())
            .map(|w| w + 8.0)
            .sum();
        pane_width - 16.0 - 16.0 - 8.0 - fixed
    };
    for hide in [Column::Owner, Column::Perms, Column::Kind, Column::Mtime] {
        if name_room(&columns) >= 150.0 {
            break;
        }
        columns.retain(|c| *c != hide);
    }
    columns
}

/// 停靠面板里的窄版：按这一栏的像素宽度给出要显示哪些列（名称至少留 120px）。
pub(super) fn compact_columns(pane_width: f32) -> Vec<Column> {
    if pane_width >= 120.0 + 64.0 + 128.0 + 40.0 {
        vec![Column::Name, Column::Size, Column::Mtime]
    } else {
        vec![Column::Name, Column::Size]
    }
}

// ---------------------------------------------------------------------------
// 显示用的文字
// ---------------------------------------------------------------------------

/// 列表里的大小：不到 1 KB 写字节，其余取整的 K / M / G / T（和 0.2.14 的 `formatSizeShort` 一致）。
pub(super) fn size_short(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = "";
    for next in ["K", "M", "G", "T"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value >= 100.0 {
        format!("{}{unit}", value.round() as u64)
    } else {
        format!("{value:.0}{unit}")
    }
}

/// 列表里的修改时间：今年的写 `月-日 时:分`，往年的只写日期；0 和没有留空。
/// `now_year` 和 `utc_offset_secs` 由调用方给，这样可以单测。
pub(super) fn time_short(unix_seconds: Option<u64>, now_year: i64, utc_offset_secs: i64) -> String {
    let Some(seconds) = unix_seconds.filter(|s| *s != 0) else {
        return String::new();
    };
    let local = seconds as i64 + utc_offset_secs;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if year == now_year {
        format!(
            "{month:02}-{day:02} {:02}:{:02}",
            secs_of_day / 3600,
            secs_of_day % 3600 / 60
        )
    } else {
        format!("{year}-{month:02}-{day:02}")
    }
}

/// 属性对话框里的完整时间。
pub(super) fn time_full(unix_seconds: Option<u64>, utc_offset_secs: i64) -> String {
    let Some(seconds) = unix_seconds.filter(|s| *s != 0) else {
        return "—".to_owned();
    };
    let local = seconds as i64 + utc_offset_secs;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60
    )
}

/// 天数（自 1970-01-01）→ 公历年月日。
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 一格的文字。
pub(super) fn cell_text(column: Column, entry: &Entry, now_year: i64, offset: i64) -> String {
    match column {
        Column::Name => entry.name.clone(),
        Column::Size => {
            if matches!(entry.kind, EntryKind::Dir | EntryKind::Drive) {
                String::new()
            } else {
                entry.size.map(size_short).unwrap_or_default()
            }
        }
        Column::Mtime => time_short(entry.mtime, now_year, offset),
        Column::Kind => match entry.kind {
            EntryKind::Dir => "目录",
            EntryKind::Symlink => "链接",
            EntryKind::Drive => "盘符",
            _ => "文件",
        }
        .to_owned(),
        Column::Perms => entry
            .permissions
            .map(|mode| format!("{:04o}", mode & 0o7777))
            .unwrap_or_default(),
        Column::Owner => entry.owner.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        Column, Crumb, Entry, EntryKind, Filter, LinkBase, NavHistory, Selection, SortKey,
        SortOrder, TargetKind, adaptive_columns, cell_text, civil_from_days, collapse_crumbs,
        compact_columns, filter_entries, full_columns, local_crumbs, local_parent, remote_crumbs,
        remote_parent, size_short, sort_entries, time_full, time_short, visible_entries,
    };

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.to_owned(),
            path: format!("/d/{name}"),
            kind,
            target_kind: None,
            link_target: None,
            size: None,
            mtime: None,
            permissions: None,
            owner: None,
            hidden: name.starts_with('.'),
            name_is_utf8: true,
        }
    }

    fn file(name: &str, size: u64, mtime: u64) -> Entry {
        Entry {
            size: Some(size),
            mtime: Some(mtime),
            ..entry(name, EntryKind::File)
        }
    }

    fn names(rows: &[Entry]) -> Vec<&str> {
        rows.iter().map(|e| e.name.as_str()).collect()
    }

    fn sample() -> Vec<Entry> {
        vec![
            file("b.txt", 20, 200),
            entry("zdir", EntryKind::Dir),
            file("a.txt", 30, 100),
            file("c.txt", 10, 300),
            entry("adir", EntryKind::Dir),
        ]
    }

    #[test]
    fn directories_stay_in_front_whatever_the_order() {
        for order in [
            SortOrder::DEFAULT,
            SortOrder {
                key: SortKey::Name,
                descending: true,
            },
            SortOrder {
                key: SortKey::Size,
                descending: false,
            },
            SortOrder {
                key: SortKey::Mtime,
                descending: true,
            },
        ] {
            let mut rows = sample();
            sort_entries(&mut rows, order);
            assert!(
                rows[0].kind == EntryKind::Dir && rows[1].kind == EntryKind::Dir,
                "{order:?}: {:?}",
                names(&rows)
            );
        }
    }

    #[test]
    fn names_sort_numerically_and_ignore_case() {
        let mut rows = vec![
            file("file10", 0, 0),
            file("File2", 0, 0),
            file("file1", 0, 0),
        ];
        sort_entries(&mut rows, SortOrder::DEFAULT);
        assert_eq!(names(&rows), ["file1", "File2", "file10"]);
    }

    #[test]
    fn size_and_time_sorts_follow_the_direction() {
        let mut rows = sample();
        sort_entries(
            &mut rows,
            SortOrder {
                key: SortKey::Size,
                descending: false,
            },
        );
        assert_eq!(&names(&rows)[2..], ["c.txt", "b.txt", "a.txt"]);
        sort_entries(
            &mut rows,
            SortOrder {
                key: SortKey::Mtime,
                descending: true,
            },
        );
        assert_eq!(&names(&rows)[2..], ["c.txt", "b.txt", "a.txt"]);
    }

    #[test]
    fn equal_sizes_and_times_fall_back_to_the_name() {
        let mut rows = vec![
            file("c.txt", 5, 100),
            file("a.txt", 5, 100),
            file("b.txt", 5, 100),
        ];
        for key in [SortKey::Size, SortKey::Mtime] {
            sort_entries(
                &mut rows,
                SortOrder {
                    key,
                    descending: false,
                },
            );
            assert_eq!(names(&rows), ["a.txt", "b.txt", "c.txt"]);
            rows.reverse();
        }
    }

    #[test]
    fn kind_sort_puts_links_between_directories_and_files() {
        let mut link = entry("link", EntryKind::Symlink);
        link.target_kind = Some(TargetKind::File);
        let mut rows = vec![file("f", 1, 1), link, entry("d", EntryKind::Dir)];
        sort_entries(
            &mut rows,
            SortOrder {
                key: SortKey::Kind,
                descending: false,
            },
        );
        assert_eq!(names(&rows), ["d", "link", "f"]);
    }

    #[test]
    fn clicking_a_header_flips_or_switches_the_order() {
        let order = SortOrder::DEFAULT.toggled(SortKey::Name);
        assert!(order.descending);
        let order = order.toggled(SortKey::Size);
        assert_eq!(order.key, SortKey::Size);
        assert!(!order.descending);
    }

    #[test]
    fn filter_hides_dotfiles_and_matches_substrings_case_insensitively() {
        let rows = vec![
            file("Readme.md", 1, 1),
            file(".env", 1, 1),
            file("notes.txt", 1, 1),
        ];
        let shown = filter_entries(&rows, &Filter::default());
        assert_eq!(names(&shown), ["Readme.md", "notes.txt"]);
        let shown = filter_entries(
            &rows,
            &Filter {
                text: " README ".to_owned(),
                show_hidden: true,
            },
        );
        assert_eq!(names(&shown), ["Readme.md"]);
        let shown = filter_entries(
            &rows,
            &Filter {
                text: String::new(),
                show_hidden: true,
            },
        );
        assert_eq!(shown.len(), 3);
    }

    #[test]
    fn drives_are_never_hidden() {
        let mut drive = entry("C:", EntryKind::Drive);
        drive.hidden = true;
        let shown = filter_entries(&[drive], &Filter::default());
        assert_eq!(shown.len(), 1);
    }

    fn rows5() -> Vec<Entry> {
        visible_entries(
            &["a", "b", "c", "d", "e"].map(|n| file(n, 1, 1)),
            SortOrder::DEFAULT,
            &Filter::default(),
        )
    }

    fn picked(sel: &Selection, rows: &[Entry]) -> Vec<String> {
        sel.entries(rows).iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn click_ctrl_and_shift_build_the_selection() {
        let rows = rows5();
        let mut sel = Selection::default();
        sel.click(&rows, 1, false, false);
        assert_eq!(picked(&sel, &rows), ["b"]);
        sel.click(&rows, 3, true, false);
        assert_eq!(picked(&sel, &rows), ["b", "d"]);
        sel.click(&rows, 1, true, false);
        assert_eq!(picked(&sel, &rows), ["d"]);
        // Shift：从锚点（刚 Ctrl 点的 b）到 e。
        sel.click(&rows, 4, false, true);
        assert_eq!(picked(&sel, &rows), ["b", "c", "d", "e"]);
        // 裸点重新只选一个。
        sel.click(&rows, 2, false, false);
        assert_eq!(picked(&sel, &rows), ["c"]);
    }

    #[test]
    fn shift_click_without_an_anchor_acts_like_a_plain_click() {
        let rows = rows5();
        let mut sel = Selection::default();
        sel.click(&rows, 2, false, true);
        assert_eq!(picked(&sel, &rows), ["c"]);
    }

    #[test]
    fn arrows_move_the_focus_and_shift_extends() {
        let rows = rows5();
        let mut sel = Selection::default();
        assert_eq!(sel.arrow(&rows, 1, false), Some(1));
        assert_eq!(picked(&sel, &rows), ["b"]);
        assert_eq!(sel.arrow(&rows, 1, true), Some(2));
        assert_eq!(picked(&sel, &rows), ["b", "c"]);
        assert_eq!(sel.arrow(&rows, -1, true), Some(1));
        assert_eq!(picked(&sel, &rows), ["b"]);
        // 到头不越界。
        assert_eq!(sel.arrow(&rows, -10, false), Some(0));
        assert_eq!(sel.arrow(&rows, 10, false), Some(4));
        assert_eq!(sel.arrow(&[], 1, false), None);
    }

    #[test]
    fn home_and_end_jump() {
        let rows = rows5();
        let mut sel = Selection::default();
        sel.click(&rows, 2, false, false);
        assert_eq!(sel.jump(&rows, 4, false), Some(4));
        assert_eq!(picked(&sel, &rows), ["e"]);
        assert_eq!(sel.jump(&rows, 0, true), Some(0));
        assert_eq!(picked(&sel, &rows).len(), 5);
    }

    #[test]
    fn select_all_and_clear() {
        let rows = rows5();
        let mut sel = Selection::default();
        sel.select_all(&rows);
        assert_eq!(sel.indices(&rows), [0, 1, 2, 3, 4]);
        sel.clear();
        assert!(sel.paths.is_empty() && sel.anchor.is_none() && sel.focused.is_none());
        sel.select_all(&[]);
        assert!(sel.paths.is_empty());
    }

    #[test]
    fn right_click_keeps_a_selected_row_and_replaces_for_an_unselected_one() {
        let rows = rows5();
        let mut sel = Selection::default();
        sel.click(&rows, 0, false, false);
        sel.click(&rows, 2, false, true);
        assert_eq!(picked(&sel, &rows), ["a", "b", "c"]);
        sel.right_click(&rows, 1);
        assert_eq!(picked(&sel, &rows), ["a", "b", "c"]);
        sel.right_click(&rows, 4);
        assert_eq!(picked(&sel, &rows), ["e"]);
    }

    #[test]
    fn selection_follows_the_file_not_the_row_number() {
        let mut rows = rows5();
        let mut sel = Selection::default();
        sel.click(&rows, 1, false, false);
        // 倒序之后 b 不再是第 1 行。
        sort_entries(
            &mut rows,
            SortOrder {
                key: SortKey::Name,
                descending: true,
            },
        );
        assert_eq!(picked(&sel, &rows), ["b"]);
        assert_eq!(sel.indices(&rows), [3]);
        // 过滤掉它：不再算选中，也就不会被操作。
        let shown = filter_entries(
            &rows,
            &Filter {
                text: "c".to_owned(),
                show_hidden: false,
            },
        );
        assert!(picked(&sel, &shown).is_empty());
        sel.retain_present(&shown);
        assert!(sel.paths.is_empty() && sel.anchor.is_none());
    }

    #[test]
    fn select_paths_picks_what_is_present() {
        let rows = rows5();
        let mut sel = Selection::default();
        assert!(!sel.select_paths(&rows, &["/d/nope".to_owned()]));
        assert!(sel.select_paths(&rows, &["/d/c".to_owned(), "/d/nope".to_owned()]));
        assert_eq!(picked(&sel, &rows), ["c"]);
    }

    #[test]
    fn remote_breadcrumbs_and_parent() {
        let crumbs = remote_crumbs("/tmp/a/b");
        assert_eq!(
            crumbs,
            vec![
                Crumb {
                    name: "tmp".into(),
                    path: "/tmp".into()
                },
                Crumb {
                    name: "a".into(),
                    path: "/tmp/a".into()
                },
                Crumb {
                    name: "b".into(),
                    path: "/tmp/a/b".into()
                },
            ]
        );
        assert!(remote_crumbs("/").is_empty());
        assert_eq!(remote_parent("/tmp/a"), "/tmp");
        assert_eq!(remote_parent("/tmp"), "/");
        assert_eq!(remote_parent("/"), "/");
    }

    #[test]
    fn local_breadcrumbs_handle_drives_shares_and_the_verbatim_prefix() {
        let crumbs = local_crumbs(r"C:\Users\demo\Docs");
        let paths: Vec<&str> = crumbs.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            [r"C:\", r"C:\Users", r"C:\Users\demo", r"C:\Users\demo\Docs"]
        );
        assert_eq!(crumbs[0].name, "C:");
        assert_eq!(local_parent(r"C:\Users\demo"), r"C:\Users");
        assert_eq!(local_parent(r"C:\Users"), r"C:\");
        assert_eq!(local_parent(r"C:\"), "");
        assert_eq!(local_parent(""), "");

        let share = local_crumbs(r"\\srv\share\x\y");
        assert_eq!(share[0].name, r"\\srv\share");
        assert_eq!(share[0].path, r"\\srv\share\");
        assert_eq!(share[1].path, r"\\srv\share\x");
        assert_eq!(local_parent(r"\\srv\share\x"), r"\\srv\share\");
        assert_eq!(local_parent(r"\\srv\share"), "");

        let long = local_crumbs(r"\\?\C:\a");
        assert_eq!(long[0].name, "C:");
        let unc = local_crumbs(r"\\?\UNC\srv\share\a");
        assert_eq!(unc[0].name, r"\\srv\share");
    }

    #[test]
    fn crumbs_collapse_from_the_left_and_keep_the_last_one() {
        let crumbs = remote_crumbs("/very/long/path/with/many/parts/inside");
        let (hidden, shown) = collapse_crumbs(&crumbs, 100_000.0);
        assert!(hidden.is_empty());
        assert_eq!(shown.len(), crumbs.len());
        let (hidden, shown) = collapse_crumbs(&crumbs, 150.0);
        assert!(!hidden.is_empty());
        assert_eq!(shown.last(), crumbs.last());
        assert_eq!(hidden.len() + shown.len(), crumbs.len());
        // 再窄也至少留最后一级。
        let (_, shown) = collapse_crumbs(&crumbs, 1.0);
        assert_eq!(shown.len(), 1);
        assert_eq!(collapse_crumbs(&[], 100.0), (Vec::new(), Vec::new()));
    }

    #[test]
    fn history_goes_back_and_forward_and_drops_the_forward_tail() {
        let mut history = NavHistory::default();
        assert!(!history.can_back() && !history.can_forward());
        history.visit("/a");
        history.visit("/b");
        history.visit("/b"); // 刷新不算一步
        history.visit("/c");
        assert!(history.can_back() && !history.can_forward());
        assert_eq!(history.step(-1).as_deref(), Some("/b"));
        assert_eq!(history.step(-1).as_deref(), Some("/a"));
        assert_eq!(history.step(-1), None);
        assert_eq!(history.step(1).as_deref(), Some("/b"));
        history.visit("/d"); // 在中间走新路：丢掉 /c
        assert!(!history.can_forward());
        assert_eq!(history.step(-1).as_deref(), Some("/b"));
        assert_eq!(history.current(), Some("/b"));
    }

    #[test]
    fn history_forgets_unreachable_directories() {
        let mut history = NavHistory::default();
        for path in ["/a", "/b", "/c"] {
            history.visit(path);
        }
        history.step(-1);
        history.forget("/b");
        assert_eq!(history.current(), Some("/a"));
        assert!(history.can_forward());
        history.forget("/a");
        history.forget("/c");
        assert_eq!(history.current(), None);
        assert!(!history.can_back());
    }

    #[test]
    fn history_is_capped() {
        let mut history = NavHistory::default();
        for n in 0..150 {
            history.visit(&format!("/d{n}"));
        }
        assert_eq!(history.current(), Some("/d149"));
        let mut steps = 0;
        while history.step(-1).is_some() {
            steps += 1;
        }
        assert_eq!(steps, 99);
    }

    #[test]
    fn columns_match_the_front_end() {
        let remote = full_columns(true);
        assert_eq!(remote.len(), 6);
        assert!(remote.contains(&Column::Perms) && remote.contains(&Column::Owner));
        let local = full_columns(false);
        assert_eq!(
            local,
            [Column::Name, Column::Size, Column::Mtime, Column::Kind]
        );
        assert_eq!(
            compact_columns(600.0),
            [Column::Name, Column::Size, Column::Mtime]
        );
        assert_eq!(compact_columns(300.0), [Column::Name, Column::Size]);
        assert!(Column::Perms.sort_key().is_none() && Column::Owner.sort_key().is_none());
        assert_eq!(Column::Size.sort_key(), Some(SortKey::Size));
    }

    #[test]
    fn narrow_panes_drop_columns_from_the_right_until_the_name_fits() {
        // 很宽：全部列。
        assert_eq!(adaptive_columns(true, 1200.0), full_columns(true));
        // 量不到：全部列（不凭空藏）。
        assert_eq!(adaptive_columns(true, 0.0), full_columns(true));
        // 480 宽的远端栏：藏掉所有者、权限，留下类型。
        let mid = adaptive_columns(true, 480.0);
        assert_eq!(
            mid,
            [Column::Name, Column::Size, Column::Mtime, Column::Kind]
        );
        // 再窄只剩名称 + 大小。
        let narrow = adaptive_columns(true, 300.0);
        assert_eq!(narrow, [Column::Name, Column::Size]);
        // 本地栏本来就没有权限和所有者。
        assert_eq!(
            adaptive_columns(false, 480.0),
            [Column::Name, Column::Size, Column::Mtime, Column::Kind]
        );
        // 名称一直在。
        assert!(adaptive_columns(true, 50.0).contains(&Column::Name));
    }

    #[test]
    fn short_sizes_match_the_front_end() {
        assert_eq!(size_short(0), "0 B");
        assert_eq!(size_short(1023), "1023 B");
        assert_eq!(size_short(1024), "1K");
        assert_eq!(size_short(1536), "2K");
        assert_eq!(size_short(5 * 1024 * 1024), "5M");
        assert_eq!(size_short(150 * 1024 * 1024), "150M");
        assert_eq!(size_short(3 * 1024 * 1024 * 1024), "3G");
    }

    #[test]
    fn short_times_depend_on_the_year() {
        // 2026-11-02 12:34:56 UTC。
        let t = 1_793_622_896;
        assert_eq!(time_short(Some(t), 2026, 0), "11-02 12:34");
        assert_eq!(time_short(Some(t), 2027, 0), "2026-11-02");
        assert_eq!(time_short(Some(t), 2026, 8 * 3600), "11-02 20:34");
        assert_eq!(time_short(Some(0), 2026, 0), "");
        assert_eq!(time_short(None, 2026, 0), "");
        assert_eq!(time_full(Some(t), 0), "2026-11-02 12:34:56");
        assert_eq!(time_full(None, 0), "—");
    }

    #[test]
    fn civil_dates_round_trip_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn cells_follow_the_entry() {
        let mut dir = entry("src", EntryKind::Dir);
        dir.size = Some(4096);
        assert_eq!(cell_text(Column::Size, &dir, 2026, 0), "");
        assert_eq!(cell_text(Column::Kind, &dir, 2026, 0), "目录");
        let mut f = file("a", 2048, 0);
        f.permissions = Some(0o100644);
        f.owner = Some("1000".into());
        assert_eq!(cell_text(Column::Size, &f, 2026, 0), "2K");
        assert_eq!(cell_text(Column::Perms, &f, 2026, 0), "0644");
        assert_eq!(cell_text(Column::Owner, &f, 2026, 0), "1000");
        assert_eq!(cell_text(Column::Mtime, &f, 2026, 0), "");
        let mut link = entry("l", EntryKind::Symlink);
        link.target_kind = Some(TargetKind::Dir);
        assert_eq!(cell_text(Column::Kind, &link, 2026, 0), "链接");
        assert!(link.is_enterable() && !link.is_file_like());
    }

    #[test]
    fn a_remote_link_is_drawn_by_its_target_type_with_the_target_in_the_tooltip() {
        let mut to_dir = entry("conf", EntryKind::Symlink);
        to_dir.target_kind = Some(TargetKind::Dir);
        to_dir.link_target = Some("/etc/conf.d".to_owned());
        let info = to_dir.link_info().expect("链接");
        assert_eq!(info.base, LinkBase::Folder);
        assert!(!info.broken);
        assert_eq!(info.tooltip(), "链接到：/etc/conf.d");

        let mut to_file = entry("resolv.conf", EntryKind::Symlink);
        to_file.target_kind = Some(TargetKind::File);
        to_file.link_target = Some("/run/systemd/resolve/stub-resolv.conf".to_owned());
        let info = to_file.link_info().expect("链接");
        assert_eq!(info.base, LinkBase::File);
        assert_eq!(
            info.tooltip(),
            "链接到：/run/systemd/resolve/stub-resolv.conf"
        );
        // 名称列只显示链接自己的名字：路径不在名字里。
        assert_eq!(to_file.name, "resolv.conf");
    }

    #[test]
    fn a_broken_remote_link_gets_the_warning_look_and_says_why() {
        let mut dead = entry("old", EntryKind::Symlink);
        dead.link_target = Some("/gone/away".to_owned());
        let info = dead.link_info().expect("链接");
        assert!(info.broken);
        assert_eq!(info.base, LinkBase::File);
        assert!(info.tooltip().starts_with("链接到：/gone/away"));
        assert!(info.tooltip().contains("断链"));
        // 连目标路径都没读到的链接：也按断链画，提示里没有路径。
        let unknown = entry("x", EntryKind::Symlink);
        let info = unknown.link_info().expect("链接");
        assert!(info.broken);
        assert!(!info.tooltip().contains("链接到"));
    }

    #[test]
    fn a_local_link_is_recognised_by_its_target_path_and_keeps_its_followed_kind() {
        let mut folder_link = entry("proj", EntryKind::Dir);
        folder_link.link_target = Some(r"D:\work\proj".to_owned());
        let info = folder_link.link_info().expect("本地链接");
        assert_eq!(info.base, LinkBase::Folder);
        assert!(!info.broken);
        // 操作还是按目标走：能进去。
        assert!(folder_link.is_enterable());

        let mut file_link = entry("a.txt", EntryKind::File);
        file_link.link_target = Some(r"D:\a.txt".to_owned());
        assert_eq!(file_link.link_info().expect("链接").base, LinkBase::File);

        // 目标读不了（本地跟随失败变成 Other）：断链。
        let mut dead = entry("dead", EntryKind::Other);
        dead.link_target = Some(r"D:\nowhere".to_owned());
        assert!(dead.link_info().expect("链接").broken);
    }

    #[test]
    fn ordinary_entries_are_not_links() {
        for kind in [
            EntryKind::File,
            EntryKind::Dir,
            EntryKind::Other,
            EntryKind::Drive,
        ] {
            assert!(entry("n", kind).link_info().is_none(), "{kind:?}");
        }
    }

    #[test]
    fn a_link_to_a_directory_still_opens_and_a_link_to_a_file_is_a_file() {
        let mut to_dir = entry("d", EntryKind::Symlink);
        to_dir.target_kind = Some(TargetKind::Dir);
        assert!(
            to_dir.is_enterable() && !to_dir.is_file_like(),
            "指向目录：进入"
        );
        let mut to_file = entry("f", EntryKind::Symlink);
        to_file.target_kind = Some(TargetKind::File);
        assert!(
            !to_file.is_enterable() && to_file.is_file_like(),
            "指向文件：按文件"
        );
    }

    #[test]
    fn enterable_and_openable() {
        assert!(entry("d", EntryKind::Dir).is_enterable());
        assert!(entry("C:", EntryKind::Drive).is_enterable());
        assert!(!entry("f", EntryKind::File).is_enterable());
        assert!(entry("f", EntryKind::File).is_file_like());
        // 断链：既不能进也不能开。
        let broken = entry("x", EntryKind::Symlink);
        assert!(!broken.is_enterable() && !broken.is_file_like());
        assert!(!entry("s", EntryKind::Other).is_file_like());
    }
}
