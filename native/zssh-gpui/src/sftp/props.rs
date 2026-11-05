//! 属性对话框和权限对话框的纯规则（0.2.14 `SftpPropertiesDialog` / `SftpPermissionsDialog`
//! 与 `state/sftpProperties.ts`）：要显示哪些行、大小怎么写、“计算大小”的状态怎么随事件走、
//! 勾选框和八进制怎么联动。

use zssh_core::sftp::size::SizeTotals;

use super::menu::Side;
use super::model::{Entry, EntryKind, TargetKind, time_full};
use crate::i18n::t;

// ---------------------------------------------------------------------------
// 权限：勾选框 <-> 八进制
// ---------------------------------------------------------------------------

/// 一组读 / 写 / 执行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Rwx {
    pub(super) read: bool,
    pub(super) write: bool,
    pub(super) execute: bool,
}

impl Rwx {
    fn digit(self) -> u32 {
        u32::from(self.read) * 4 + u32::from(self.write) * 2 + u32::from(self.execute)
    }

    fn from_digit(digit: u32) -> Self {
        Self {
            read: digit & 4 != 0,
            write: digit & 2 != 0,
            execute: digit & 1 != 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PermMatrix {
    pub(super) owner: Rwx,
    pub(super) group: Rwx,
    pub(super) others: Rwx,
}

/// 九个勾选框里的哪一个（点击时用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PermBit {
    OwnerRead,
    OwnerWrite,
    OwnerExecute,
    GroupRead,
    GroupWrite,
    GroupExecute,
    OthersRead,
    OthersWrite,
    OthersExecute,
}

impl PermMatrix {
    /// 勾选矩阵 → 低 9 位。
    pub(super) fn to_mode(self) -> u32 {
        self.owner.digit() * 64 + self.group.digit() * 8 + self.others.digit()
    }

    /// 低 9 位 → 勾选矩阵；超出 0o777 不是权限位，返回 `None`。
    pub(super) fn from_mode(mode: u32) -> Option<Self> {
        (mode <= 0o777).then(|| Self {
            owner: Rwx::from_digit((mode >> 6) & 7),
            group: Rwx::from_digit((mode >> 3) & 7),
            others: Rwx::from_digit(mode & 7),
        })
    }

    pub(super) fn toggled(mut self, bit: PermBit) -> Self {
        let slot = match bit {
            PermBit::OwnerRead => &mut self.owner.read,
            PermBit::OwnerWrite => &mut self.owner.write,
            PermBit::OwnerExecute => &mut self.owner.execute,
            PermBit::GroupRead => &mut self.group.read,
            PermBit::GroupWrite => &mut self.group.write,
            PermBit::GroupExecute => &mut self.group.execute,
            PermBit::OthersRead => &mut self.others.read,
            PermBit::OthersWrite => &mut self.others.write,
            PermBit::OthersExecute => &mut self.others.execute,
        };
        *slot = !*slot;
        self
    }

    pub(super) fn get(self, bit: PermBit) -> bool {
        match bit {
            PermBit::OwnerRead => self.owner.read,
            PermBit::OwnerWrite => self.owner.write,
            PermBit::OwnerExecute => self.owner.execute,
            PermBit::GroupRead => self.group.read,
            PermBit::GroupWrite => self.group.write,
            PermBit::GroupExecute => self.group.execute,
            PermBit::OthersRead => self.others.read,
            PermBit::OthersWrite => self.others.write,
            PermBit::OthersExecute => self.others.execute,
        }
    }
}

/// 对话框的初始权限位：服务器给的 mode 带文件类型位（`0o100644`），只取低 9 位；没有就是 `0o644`。
pub(super) fn initial_permission_bits(mode: Option<u32>) -> u32 {
    mode.map_or(0o644, |mode| mode & 0o777)
}

/// 八进制框里显示的三位数字。
pub(super) fn octal_text(mode: u32) -> String {
    format!("{:03o}", mode & 0o777)
}

// ---------------------------------------------------------------------------
// 属性
// ---------------------------------------------------------------------------

/// 属性对话框的对象：选中的条目；点在空白处就是当前目录本身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PropTarget {
    pub(super) name: String,
    pub(super) path: String,
    pub(super) kind: EntryKind,
    pub(super) size: Option<u64>,
    pub(super) mtime: Option<u64>,
    pub(super) permissions: Option<u32>,
    pub(super) owner: Option<String>,
    pub(super) link_target: Option<String>,
    pub(super) target_kind: Option<TargetKind>,
}

fn base_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    if name.is_empty() {
        if trimmed.is_empty() { path } else { trimmed }.to_owned()
    } else {
        name.to_owned()
    }
}

/// 选中的条目（没选 = 当前目录）→ 对话框的对象列表。盘符不算。
pub(super) fn properties_targets(entries: &[Entry], current_dir: &str) -> Vec<PropTarget> {
    if entries.is_empty() {
        return if current_dir.is_empty() {
            Vec::new()
        } else {
            vec![PropTarget {
                name: base_name(current_dir),
                path: current_dir.to_owned(),
                kind: EntryKind::Dir,
                size: None,
                mtime: None,
                permissions: None,
                owner: None,
                link_target: None,
                target_kind: None,
            }]
        };
    }
    entries
        .iter()
        .filter(|entry| !entry.is_drive())
        .map(|entry| PropTarget {
            name: entry.name.clone(),
            path: entry.path.clone(),
            kind: entry.kind,
            size: entry.size,
            mtime: entry.mtime,
            permissions: entry.permissions,
            owner: entry.owner.clone(),
            link_target: entry.link_target.clone(),
            target_kind: entry.target_kind,
        })
        .collect()
}

/// 要不要给“计算大小”：只有文件夹（指向文件夹的链接不跟随）。
pub(super) fn can_calculate(target: &PropTarget) -> bool {
    target.kind == EntryKind::Dir
}

pub(super) fn kind_label(target: &PropTarget) -> String {
    match target.kind {
        EntryKind::Dir => t(super::SftpMsg::PropsKindFolder).to_owned(),
        EntryKind::File => t(super::SftpMsg::PropsKindFile).to_owned(),
        // 目标路径在单独的「链接到」一行里（V2-U1 第 9 条），类型这一行只说它是什么链接。
        EntryKind::Symlink => match target.target_kind {
            Some(TargetKind::Dir) => t(super::SftpMsg::PropsKindSymlinkDir).to_owned(),
            Some(TargetKind::File) => t(super::SftpMsg::PropsKindSymlinkFile).to_owned(),
            None => t(super::SftpMsg::PropsKindSymlinkBroken).to_owned(),
        },
        EntryKind::Drive => t(super::SftpMsg::PropsKindDrive).to_owned(),
        EntryKind::Other => t(super::SftpMsg::PropsKindOther).to_owned(),
    }
}

/// 属性里的「链接到」一行：链接指向的路径；断链再写一句。不是链接 / 读不到路径返回 `None`。
/// 远端的链接是 `Symlink`，本地的（符号链接、目录联接）是跟随之后的类型加上读到的目标路径。
pub(super) fn link_row(target: &PropTarget) -> Option<String> {
    let link = target.link_target.as_ref()?;
    let broken = match target.kind {
        EntryKind::Symlink => target.target_kind.is_none(),
        EntryKind::Dir | EntryKind::File => false,
        _ => true,
    };
    Some(if broken {
        format!("{link}{}", t(super::SftpMsg::PropsLinkBrokenSuffix))
    } else {
        link.clone()
    })
}

/// 属性里的大小写法：不到 1 KB 写字节，其余一位小数（100 以上取整）。
pub(super) fn size_long(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = "B";
    for next in ["KB", "MB", "GB", "TB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value >= 100.0 {
        format!("{} {unit}", value.round() as u64)
    } else {
        format!("{value:.1} {unit}")
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 字节数的完整写法：`1.5 MB（1,572,864 字节）`；不到 1 KB 就只写字节。
pub(super) fn bytes_text(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}{}", t(super::SftpMsg::PropsBytesSuffix))
    } else {
        format!(
            "{}{}{}{}",
            size_long(bytes),
            t(super::SftpMsg::PropsBytesParenOpen),
            thousands(bytes),
            t(super::SftpMsg::PropsBytesParenClose)
        )
    }
}

/// `3 个文件，2 个文件夹`（根目录自己不算在里面）。
pub(super) fn counts_text(totals: &SizeTotals) -> String {
    let folders = totals.dirs.saturating_sub(1);
    format!(
        "{}{}{}{}",
        totals.files,
        t(super::SftpMsg::PropsFilesCount),
        folders,
        t(super::SftpMsg::PropsFoldersCount)
    )
}

/// 多个对象的合计。
pub(super) fn sum_totals(list: &[SizeTotals]) -> SizeTotals {
    list.iter().fold(SizeTotals::default(), |mut sum, item| {
        sum.files += item.files;
        sum.dirs += item.dirs;
        sum.bytes += item.bytes;
        sum.links += item.links;
        sum.unreadable += item.unreadable;
        sum
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CalcPhase {
    Idle,
    Running,
    Done,
    Cancelled,
    Failed,
}

/// 一个对象的“计算大小”状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CalcState {
    pub(super) phase: CalcPhase,
    /// 发起这次计算时界面生成的编号，事件带着它回来：同时算两个文件夹、重新点过
    /// “计算大小”时，旧的事件不会改新的状态。
    pub(super) request: u64,
    pub(super) totals: SizeTotals,
    pub(super) error: Option<String>,
}

impl CalcState {
    pub(super) fn idle() -> Self {
        Self {
            phase: CalcPhase::Idle,
            request: 0,
            totals: SizeTotals::default(),
            error: None,
        }
    }

    pub(super) fn running(request: u64) -> Self {
        Self {
            phase: CalcPhase::Running,
            request,
            totals: SizeTotals::default(),
            error: None,
        }
    }

    /// 事件 → 新状态。只接受编号对得上的；已经结束的不会被后到的进度改回“进行中”。
    pub(super) fn apply(
        &mut self,
        request: u64,
        phase: CalcPhase,
        totals: SizeTotals,
        error: Option<String>,
    ) {
        if self.request != request || self.request == 0 {
            return;
        }
        match (self.phase, phase) {
            (CalcPhase::Done | CalcPhase::Failed, _) => return,
            (CalcPhase::Cancelled, CalcPhase::Running) => return,
            _ => {}
        }
        self.phase = phase;
        self.totals = totals;
        self.error = error;
    }
}

/// “计算大小”那一行的文字。
pub(super) fn calc_text(state: &CalcState) -> String {
    let base = format!(
        "{}{}{}",
        bytes_text(state.totals.bytes),
        crate::i18n::comma(),
        counts_text(&state.totals)
    );
    let mut extras = Vec::new();
    if state.totals.links > 0 {
        extras.push(format!(
            "{}{}{}",
            t(super::SftpMsg::PropsSkippedLinksPrefix),
            state.totals.links,
            t(super::SftpMsg::PropsSkippedLinksSuffix)
        ));
    }
    if state.totals.unreadable > 0 {
        extras.push(format!(
            "{}{}",
            state.totals.unreadable,
            t(super::SftpMsg::PropsUnreadableFoldersSuffix)
        ));
    }
    let suffix = if extras.is_empty() {
        String::new()
    } else {
        format!(
            "{}{}{}",
            t(super::SftpMsg::ParenOpen),
            extras.join(t(super::SftpMsg::PropsComma)),
            t(super::SftpMsg::ParenClose)
        )
    };
    match state.phase {
        CalcPhase::Idle => t(super::SftpMsg::PropsNotComputed).to_owned(),
        CalcPhase::Running => {
            format!(
                "{}{base}{suffix}",
                t(super::SftpMsg::PropsCalcRunningPrefix)
            )
        }
        CalcPhase::Cancelled => format!(
            "{}{base}{suffix}{}",
            t(super::SftpMsg::PropsCalcCancelledPrefix),
            t(super::SftpMsg::PropsCalcCancelledSuffix)
        ),
        CalcPhase::Failed => format!(
            "{}{}",
            t(super::SftpMsg::PropsCalcFailedPrefix),
            state
                .error
                .as_deref()
                .unwrap_or(t(super::SftpMsg::PropsUnknownError))
        ),
        CalcPhase::Done => format!("{base}{suffix}"),
    }
}

/// 多个文件夹合计的状态：有在算的就是进行中，其次失败 / 取消，全完成才是完成。
pub(super) fn aggregate(states: &[CalcState]) -> CalcState {
    let phase = if states.iter().any(|s| s.phase == CalcPhase::Running) {
        CalcPhase::Running
    } else if states.iter().any(|s| s.phase == CalcPhase::Failed) {
        CalcPhase::Failed
    } else if states.iter().any(|s| s.phase == CalcPhase::Cancelled) {
        CalcPhase::Cancelled
    } else if !states.is_empty() && states.iter().all(|s| s.phase == CalcPhase::Done) {
        CalcPhase::Done
    } else {
        CalcPhase::Idle
    };
    let totals: Vec<SizeTotals> = states.iter().map(|s| s.totals.clone()).collect();
    CalcState {
        phase,
        request: 0,
        totals: sum_totals(&totals),
        error: states.iter().find_map(|s| s.error.clone()),
    }
}

/// 单个对象的信息行。
pub(super) fn info_rows(
    target: &PropTarget,
    side: Side,
    utc_offset_secs: i64,
) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        (t(super::SftpMsg::PropsRowName), target.name.clone()),
        (t(super::SftpMsg::PropsRowLocation), target.path.clone()),
        (t(super::SftpMsg::PropsRowType), kind_label(target)),
    ];
    if let Some(link) = link_row(target) {
        rows.push((t(super::SftpMsg::PropsRowLinkTo), link));
    }
    if target.kind == EntryKind::File
        && let Some(size) = target.size
    {
        rows.push((t(super::SftpMsg::PropsRowSize), bytes_text(size)));
    }
    rows.push((
        t(super::SftpMsg::PropsRowMtime),
        time_full(target.mtime, utc_offset_secs),
    ));
    if side == Side::Remote {
        rows.push((
            t(super::SftpMsg::PropsRowPerms),
            target
                .permissions
                .map_or_else(|| "—".to_owned(), |mode| format!("{:04o}", mode & 0o7777)),
        ));
        if let Some(owner) = &target.owner {
            rows.push((t(super::SftpMsg::PropsRowOwner), owner.clone()));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use zssh_core::sftp::size::SizeTotals;

    use super::super::menu::Side;
    use super::super::model::{Entry, EntryKind};
    use super::{
        CalcPhase, CalcState, PermBit, PermMatrix, aggregate, bytes_text, calc_text, can_calculate,
        counts_text, info_rows, initial_permission_bits, octal_text, properties_targets, size_long,
        sum_totals,
    };

    fn totals(files: u64, dirs: u64, bytes: u64) -> SizeTotals {
        SizeTotals {
            files,
            dirs,
            bytes,
            links: 0,
            unreadable: 0,
        }
    }

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.to_owned(),
            path: format!("/srv/{name}"),
            kind,
            target_kind: None,
            link_target: None,
            size: Some(1_572_864),
            mtime: Some(1_793_622_896),
            permissions: Some(0o100640),
            owner: Some("1000".to_owned()),
            hidden: false,
            name_is_utf8: true,
        }
    }

    #[test]
    fn matrix_and_octal_round_trip_for_every_mode() {
        for mode in 0..=0o777u32 {
            let matrix = PermMatrix::from_mode(mode).expect("合法权限位");
            assert_eq!(matrix.to_mode(), mode);
        }
        assert!(PermMatrix::from_mode(0o1000).is_none());
    }

    #[test]
    fn each_bit_means_read_4_write_2_execute_1() {
        let matrix = PermMatrix::default().toggled(PermBit::OwnerRead);
        assert_eq!(matrix.to_mode(), 0o400);
        assert_eq!(
            PermMatrix::default().toggled(PermBit::GroupWrite).to_mode(),
            0o020
        );
        assert_eq!(
            PermMatrix::default()
                .toggled(PermBit::OthersExecute)
                .to_mode(),
            0o001
        );
        let back = matrix.toggled(PermBit::OwnerRead);
        assert_eq!(back, PermMatrix::default());
        assert!(matrix.get(PermBit::OwnerRead) && !matrix.get(PermBit::OwnerWrite));
    }

    #[test]
    fn the_initial_mode_drops_the_file_type_bits() {
        assert_eq!(initial_permission_bits(Some(0o100644)), 0o644);
        assert_eq!(initial_permission_bits(Some(0o040755)), 0o755);
        assert_eq!(initial_permission_bits(None), 0o644);
        // 带类型位的和不带的得到同一个勾选矩阵。
        assert_eq!(
            PermMatrix::from_mode(initial_permission_bits(Some(0o100644))),
            PermMatrix::from_mode(0o644)
        );
        assert_eq!(octal_text(0o100604), "604");
        assert_eq!(octal_text(0o7), "007");
    }

    #[test]
    fn sizes_are_written_like_the_front_end() {
        assert_eq!(size_long(0), "0 B");
        assert_eq!(size_long(1536), "1.5 KB");
        assert_eq!(size_long(1_572_864), "1.5 MB");
        assert_eq!(size_long(150 * 1024 * 1024), "150 MB");
        assert_eq!(bytes_text(512), "512 字节");
        assert_eq!(bytes_text(1_572_864), "1.5 MB（1,572,864 字节）");
        assert_eq!(bytes_text(1024), "1.0 KB（1,024 字节）");
    }

    #[test]
    fn counts_do_not_include_the_root_folder_itself() {
        assert_eq!(counts_text(&totals(3, 3, 0)), "3 个文件，2 个文件夹");
        assert_eq!(counts_text(&totals(0, 0, 0)), "0 个文件，0 个文件夹");
        let sum = sum_totals(&[totals(1, 2, 10), totals(4, 1, 5)]);
        assert_eq!((sum.files, sum.dirs, sum.bytes), (5, 3, 15));
    }

    #[test]
    fn calc_events_are_matched_by_request_number() {
        let mut state = CalcState::running(7);
        // 别的请求的事件不动它。
        state.apply(8, CalcPhase::Done, totals(9, 9, 9), None);
        assert_eq!(state.phase, CalcPhase::Running);
        state.apply(7, CalcPhase::Running, totals(1, 1, 100), None);
        assert_eq!(state.totals.files, 1);
        state.apply(7, CalcPhase::Done, totals(2, 2, 200), None);
        assert_eq!(state.phase, CalcPhase::Done);
        // 完成之后晚到的进度不能改回去。
        state.apply(7, CalcPhase::Running, totals(1, 1, 100), None);
        assert_eq!(state.phase, CalcPhase::Done);
        assert_eq!(state.totals.bytes, 200);
    }

    #[test]
    fn a_cancelled_calculation_ignores_late_progress_but_keeps_the_partial_count() {
        let mut state = CalcState::running(1);
        state.apply(1, CalcPhase::Cancelled, totals(5, 2, 50), None);
        state.apply(1, CalcPhase::Running, totals(1, 1, 1), None);
        assert_eq!(state.phase, CalcPhase::Cancelled);
        assert!(calc_text(&state).contains("不是完整结果"));
        assert!(calc_text(&state).contains("5 个文件"));
    }

    #[test]
    fn idle_states_ignore_events() {
        let mut state = CalcState::idle();
        state.apply(0, CalcPhase::Done, totals(1, 1, 1), None);
        assert_eq!(state.phase, CalcPhase::Idle);
        assert_eq!(calc_text(&state), "未计算");
    }

    #[test]
    fn calc_text_reports_skipped_links_and_unreadable_folders() {
        let mut state = CalcState::running(1);
        let mut t = totals(10, 3, 2048);
        t.links = 2;
        t.unreadable = 1;
        state.apply(1, CalcPhase::Done, t, None);
        assert_eq!(
            calc_text(&state),
            "2.0 KB（2,048 字节），10 个文件，2 个文件夹（跳过 2 个符号链接，1 个文件夹读不了）"
        );
        let mut failed = CalcState::running(2);
        failed.apply(
            2,
            CalcPhase::Failed,
            SizeTotals::default(),
            Some("没权限".into()),
        );
        assert_eq!(calc_text(&failed), "计算失败：没权限");
    }

    #[test]
    fn the_aggregate_is_running_while_any_folder_is_still_counting() {
        let mut a = CalcState::running(1);
        a.apply(1, CalcPhase::Done, totals(1, 1, 10), None);
        let b = CalcState::running(2);
        let all = aggregate(&[a.clone(), b]);
        assert_eq!(all.phase, CalcPhase::Running);
        assert_eq!(all.totals.bytes, 10);
        assert_eq!(aggregate(&[a]).phase, CalcPhase::Done);
        assert_eq!(aggregate(&[]).phase, CalcPhase::Idle);
        let mut c = CalcState::running(3);
        c.apply(3, CalcPhase::Cancelled, totals(1, 1, 1), None);
        assert_eq!(aggregate(&[c]).phase, CalcPhase::Cancelled);
    }

    #[test]
    fn targets_come_from_the_selection_or_the_current_directory() {
        let selected = properties_targets(&[entry("a", EntryKind::File)], "/srv");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "a");
        let current = properties_targets(&[], "/srv/site");
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].name, "site");
        assert_eq!(current[0].kind, EntryKind::Dir);
        assert!(can_calculate(&current[0]));
        assert!(properties_targets(&[], "").is_empty());
        // 盘符不算。
        assert!(properties_targets(&[entry("C:", EntryKind::Drive)], "").is_empty());
        // 根目录的名字就是它自己。
        assert_eq!(properties_targets(&[], "/")[0].name, "/");
        assert_eq!(properties_targets(&[], r"C:\")[0].name, "C:");
    }

    #[test]
    fn only_folders_can_be_counted() {
        let targets = properties_targets(
            &[entry("f", EntryKind::File), entry("d", EntryKind::Dir)],
            "/srv",
        );
        assert!(!can_calculate(&targets[0]));
        assert!(can_calculate(&targets[1]));
    }

    #[test]
    fn info_rows_show_permissions_and_owner_on_the_remote_side_only() {
        let target = &properties_targets(&[entry("a.txt", EntryKind::File)], "/srv")[0];
        let remote = info_rows(target, Side::Remote, 0);
        let labels: Vec<&str> = remote.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            ["名称", "位置", "类型", "大小", "修改时间", "权限", "所有者"]
        );
        assert!(remote.contains(&("权限", "0640".to_owned())));
        assert!(remote.contains(&("修改时间", "2026-11-02 12:34:56".to_owned())));
        let local = info_rows(target, Side::Local, 0);
        assert!(
            local
                .iter()
                .all(|(label, _)| *label != "权限" && *label != "所有者")
        );
        // 文件夹不写大小（要点“计算大小”）。
        let dir = &properties_targets(&[entry("d", EntryKind::Dir)], "/srv")[0];
        assert!(
            info_rows(dir, Side::Remote, 0)
                .iter()
                .all(|(l, _)| *l != "大小")
        );
    }

    #[test]
    fn symlinks_show_where_they_point_in_their_own_row() {
        use super::TargetKind;
        let mut link = entry("l", EntryKind::Symlink);
        link.link_target = Some("../real".to_owned());
        link.target_kind = Some(TargetKind::File);
        let target = &properties_targets(&[link], "/srv")[0];
        assert_eq!(super::kind_label(target), "符号链接（指向文件）");
        let rows = info_rows(target, Side::Remote, 0);
        assert!(
            rows.contains(&("链接到", "../real".to_owned())),
            "目标路径在「链接到」一行里：{rows:?}"
        );
        // 类型那一行不再带路径。
        assert!(!super::kind_label(target).contains("../real"));
    }

    #[test]
    fn a_broken_link_says_so_in_the_properties() {
        let mut link = entry("old", EntryKind::Symlink);
        link.link_target = Some("/gone".to_owned());
        let target = &properties_targets(&[link], "/srv")[0];
        assert_eq!(super::kind_label(target), "符号链接（断链）");
        assert_eq!(
            super::link_row(target).as_deref(),
            Some("/gone（目标不存在或读不了）")
        );
    }

    #[test]
    fn a_local_link_keeps_its_followed_kind_and_gets_the_link_row() {
        let mut folder = entry("proj", EntryKind::Dir);
        folder.link_target = Some(r"D:\work\proj".to_owned());
        let target = &properties_targets(&[folder], r"C:\x")[0];
        assert_eq!(super::kind_label(target), "文件夹");
        assert_eq!(super::link_row(target).as_deref(), Some(r"D:\work\proj"));
        let rows = info_rows(target, Side::Local, 0);
        assert!(rows.iter().any(|(label, _)| *label == "链接到"));
        // 普通文件没有这一行。
        let plain = &properties_targets(&[entry("f", EntryKind::File)], "/srv")[0];
        assert!(super::link_row(plain).is_none());
    }
}
