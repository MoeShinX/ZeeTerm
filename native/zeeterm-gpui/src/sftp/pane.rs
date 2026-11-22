//! 一栏文件列表的状态（本地栏和远端栏各一份）：路径、原始列表、排序后的显示列表、
//! 选择、历史、过滤框、滚动和键盘焦点。纯规则在 `model.rs`，这里只把它们和界面对象放在一起。

use std::cell::Cell;
use std::rc::Rc;

use ::gpui::{FocusHandle, Subscription, UniformListScrollHandle};
use gpui_kit::component::input::InputState;
use gpui_kit::*;

use super::model::{Entry, Filter, NavHistory, Selection, SortOrder, SortedIndex};

/// 正在手动输入路径（Ctrl+L、点路径栏空白处）。
pub(super) struct PathEdit {
    pub(super) input: Entity<InputState>,
    pub(super) _subscriptions: Vec<Subscription>,
}

pub(super) struct Pane {
    /// 当前目录：远端是 POSIX 路径（还没读出来是空）；Windows 本地空字符串 = “此电脑”（Linux 本地没有这一层，最上层是 `/`）。
    pub(super) path: String,
    /// 这个目录读回来的原始条目。
    pub(super) raw: Vec<Entry>,
    /// `raw` 按 `order` 排好的下标；过滤框每敲一个字只重新过滤，不重排（见 `SortedIndex`）。
    pub(super) sorted: SortedIndex,
    /// 排序 + 过滤之后实际显示的行。
    pub(super) rows: Vec<Entry>,
    pub(super) order: SortOrder,
    pub(super) filter: Filter,
    pub(super) filter_input: Entity<InputState>,
    /// 窄栏里过滤框收成放大镜按钮；点开后这里是 `true`，失焦且没输入文字时收回去。
    pub(super) filter_open: bool,
    pub(super) selection: Selection,
    pub(super) history: NavHistory,
    /// 正在读目录。
    pub(super) loading: bool,
    pub(super) path_edit: Option<PathEdit>,
    pub(super) scroll: UniformListScrollHandle,
    pub(super) focus: FocusHandle,
    /// 画面上量到的这一栏宽度（面包屑折叠、窄版列精简用）；`Cell` 是因为在绘制阶段写入。
    pub(super) measured_width: Rc<Cell<f32>>,
    /// 渲染时实际用的宽度：量到的和它差得多就重画一次。
    pub(super) used_width: f32,
    /// 要求选中这些路径（压缩 / 解压完成后选中新文件）：等它们出现在列表里再选。
    pub(super) select_request: Option<Vec<String>>,
    /// 这一次右键是不是点在了某一行上（行先处理，空白处的处理器看这个标记）。
    pub(super) right_hit_row: bool,
    /// 在已选中的多个条目之一上按下：拖动要带着整个选择，所以不能一按下就收成单选；
    /// 松手时没拖动才收成它（资源管理器的做法）。
    pub(super) pending_collapse: Option<String>,
}

impl Pane {
    pub(super) fn new(
        show_hidden: bool,
        filter_input: Entity<InputState>,
        focus: FocusHandle,
    ) -> Self {
        Self {
            path: String::new(),
            raw: Vec::new(),
            sorted: SortedIndex::default(),
            rows: Vec::new(),
            order: SortOrder::DEFAULT,
            filter: Filter {
                text: String::new(),
                show_hidden,
            },
            filter_input,
            filter_open: false,
            selection: Selection::default(),
            history: NavHistory::default(),
            loading: false,
            path_edit: None,
            scroll: UniformListScrollHandle::new(),
            focus,
            measured_width: Rc::new(Cell::new(0.0)),
            used_width: 0.0,
            select_request: None,
            right_hit_row: false,
            pending_collapse: None,
        }
    }

    /// 列表本身或排序方式变了：重排，再过滤。
    pub(super) fn rebuild(&mut self) {
        self.sorted.resort(&self.raw, self.order);
        self.refilter();
    }

    /// 只有过滤条件变了（过滤框文字、显示隐藏文件）：沿用排好的顺序重新过滤，选择只留下还在列表里的。
    pub(super) fn refilter(&mut self) {
        self.rows = self.sorted.visible(&self.raw, &self.filter);
        self.selection.retain_present(&self.rows);
        if let Some(wanted) = self.select_request.take()
            && !self.selection.select_paths(&self.rows, &wanted)
        {
            // 还没出现（刷新还没回来）：留着，下一次列表回来再试。
            self.select_request = Some(wanted);
        }
    }

    /// 换了一个目录 / 刷新：换上新列表。换目录选择作废；刷新留下还在的。
    pub(super) fn set_listing(&mut self, path: String, raw: Vec<Entry>, same_dir: bool) {
        if !same_dir {
            self.selection.clear();
        }
        self.path = path;
        self.raw = raw;
        self.loading = false;
        self.rebuild();
    }

    /// 选中的条目（克隆；菜单和操作都拿副本，免得借着 `self` 去改 `self`）。
    pub(super) fn selected(&self) -> Vec<Entry> {
        self.selection
            .entries(&self.rows)
            .into_iter()
            .cloned()
            .collect()
    }

    /// 键盘焦点所在的行号（没有焦点就是第一行）。
    pub(super) fn focused_index(&self) -> Option<usize> {
        let focused = self.selection.focused.as_ref()?;
        self.rows.iter().position(|row| &row.path == focused)
    }

    /// 键盘移动 / 点击之后把那一行滚进视野。
    pub(super) fn scroll_to(&self, index: usize) {
        self.scroll
            .scroll_to_item(index, ::gpui::ScrollStrategy::Nearest);
    }
}
