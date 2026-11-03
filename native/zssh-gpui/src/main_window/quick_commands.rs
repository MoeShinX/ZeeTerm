//! 快捷命令的数据与规则（0.2.14 的 `ui/src/state/quickCommands.ts` + `sendCommand.ts`）：纯函数，
//! 不依赖 GPUI。
//!
//! 列表存在设置表的 `quick_commands` 里：值是「JSON 数组序列化成的字符串」（跟着配置同步，
//! core 的 `sync/settings.rs::check_quick_commands` 按同一规则校验），所以设置表里那一格是
//! 双层 JSON。读出来时坏的整体当空、坏的单条跳过，不让一条坏数据弄掉整个栏。

use serde_json::{Value, json};

/// 预设颜色 id（固定这 8 个；存进同步配置里，别改名）。
pub const COLORS: [&str; 8] = [
    "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink",
];

pub const NAME_MAX: usize = 100;
pub const COMMAND_MAX: usize = 8000;
pub const GROUP_MAX: usize = 50;
pub const COMMANDS_MAX: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickCommand {
    pub id: String,
    pub name: String,
    pub command: String,
    /// 分组名；`None` = 不分组（只在「全部」里）。
    pub group: Option<String>,
    /// 发送后自动回车（执行）；`false` 只填进去，让用户改完再按回车。
    pub enter: bool,
    /// 预设颜色 id；`None` = 默认（无色）。
    pub color: Option<String>,
}

/// 预设颜色的中文名和 RGB（淡底色 / 细边 / 小图标共用同一色相）。
pub fn color_style(id: &str) -> Option<(&'static str, u32)> {
    Some(match id {
        "red" => ("红", 0xEF4444),
        "orange" => ("橙", 0xF97316),
        "yellow" => ("黄", 0xEAB308),
        "green" => ("绿", 0x22C55E),
        "teal" => ("青", 0x14B8A6),
        "blue" => ("蓝", 0x3B82F6),
        "purple" => ("紫", 0xA855F7),
        "pink" => ("粉", 0xEC4899),
        _ => return None,
    })
}

/// 未知值（以后新版本加的颜色）当作没设。
pub fn normalize_color(value: Option<&str>) -> Option<String> {
    value
        .filter(|id| COLORS.contains(id))
        .map(ToOwned::to_owned)
}

/// 设置表里取出的文本 → 列表。`raw` 是已经去掉外层 JSON 引号的那段数组文本。
pub fn parse_commands(raw: &str) -> Vec<QuickCommand> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let id = object.get("id")?.as_str()?.to_owned();
            let name = object.get("name")?.as_str()?.to_owned();
            let command = object.get("command")?.as_str()?.to_owned();
            let group = object
                .get("group")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(ToOwned::to_owned);
            Some(QuickCommand {
                id,
                name,
                command,
                group,
                enter: object.get("enter").and_then(Value::as_bool) != Some(false),
                color: normalize_color(object.get("color").and_then(Value::as_str)),
            })
        })
        .collect()
}

/// 列表 → 要存进设置的字符串（数组文本；写设置表时还要再包一层 JSON 字符串）。
pub fn serialize_commands(list: &[QuickCommand]) -> String {
    let items: Vec<Value> = list
        .iter()
        .map(|item| {
            let mut object = serde_json::Map::new();
            object.insert("id".into(), json!(item.id));
            object.insert("name".into(), json!(item.name));
            object.insert("command".into(), json!(item.command));
            if let Some(group) = &item.group {
                object.insert("group".into(), json!(group));
            }
            object.insert("enter".into(), json!(item.enter));
            if let Some(color) = normalize_color(item.color.as_deref()) {
                object.insert("color".into(), json!(color));
            }
            Value::Object(object)
        })
        .collect();
    Value::Array(items).to_string()
}

/// 用到的分组，按第一次出现的顺序。
pub fn groups_of(list: &[QuickCommand]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for item in list {
        if let Some(group) = &item.group
            && !seen.contains(group)
        {
            seen.push(group.clone());
        }
    }
    seen
}

/// 栏上显示哪些：分组为空 = 全部；选的分组已经没有命令了也退回全部。
pub fn commands_in_group<'a>(list: &'a [QuickCommand], group: &str) -> Vec<&'a QuickCommand> {
    if group.is_empty() || !list.iter().any(|item| item.group.as_deref() == Some(group)) {
        return list.iter().collect();
    }
    list.iter()
        .filter(|item| item.group.as_deref() == Some(group))
        .collect()
}

/// 换行和 Tab 以外的控制字符（ESC 之类会改终端状态，不让存进命令里）。
fn has_bad_control(text: &str) -> bool {
    text.chars()
        .any(|ch| (ch < ' ' || ch == '\u{7f}') && !matches!(ch, '\n' | '\r' | '\t'))
}

/// 编辑对话框里出错的字段：错误提示画在这个字段下面；`Form` 是不属于某个字段的（比如条数到顶），
/// 画在对话框底部。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Group,
    Command,
    Form,
}

/// 编辑框的内容能不能存；能存返回 `None`，不能存返回出错的字段和原因。
pub fn validate_field(name: &str, command: &str, group: &str) -> Option<(Field, String)> {
    let name = name.trim();
    if name.is_empty() {
        return Some((Field::Name, "名称不能为空".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Some((Field::Name, format!("名称不能超过 {NAME_MAX} 个字")));
    }
    if has_bad_control(name) {
        return Some((Field::Name, "名称里不能有控制字符".into()));
    }
    if command.trim().is_empty() {
        return Some((Field::Command, "命令不能为空".into()));
    }
    if command.chars().count() > COMMAND_MAX {
        return Some((Field::Command, format!("命令不能超过 {COMMAND_MAX} 个字")));
    }
    if has_bad_control(command) {
        return Some((
            Field::Command,
            "命令里只能有换行和 Tab 这两种控制字符".into(),
        ));
    }
    if group.trim().chars().count() > GROUP_MAX {
        return Some((Field::Group, format!("分组名不能超过 {GROUP_MAX} 个字")));
    }
    if has_bad_control(group) {
        return Some((Field::Group, "分组名里不能有控制字符".into()));
    }
    None
}

/// 编辑对话框打开时各字段的初值（新建 = 空白，编辑 = 这一条现在的样子）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seed {
    /// `None` = 新建。
    pub id: Option<String>,
    pub name: String,
    pub group: String,
    pub command: String,
    pub color: Option<String>,
    pub enter: bool,
}

/// 对话框打开时光标放哪：新建先填名称，编辑多半是改命令内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Name,
    Command,
}

impl Seed {
    pub fn blank() -> Self {
        Self {
            id: None,
            name: String::new(),
            group: String::new(),
            command: String::new(),
            color: None,
            enter: false,
        }
    }

    pub fn of(command: &QuickCommand) -> Self {
        Self {
            id: Some(command.id.clone()),
            name: command.name.clone(),
            group: command.group.clone().unwrap_or_default(),
            command: command.command.clone(),
            color: command.color.clone(),
            enter: command.enter,
        }
    }

    pub fn focus(&self) -> Focus {
        if self.id.is_some() {
            Focus::Command
        } else {
            Focus::Name
        }
    }
}

/// 对话框标题。
pub fn dialog_title(editing: bool) -> &'static str {
    if editing {
        "编辑命令"
    } else {
        "新建命令"
    }
}

/// 对话框里现在填的内容（输入框的文字 + 颜色 / 开关）。
pub struct Filled<'a> {
    pub id: Option<&'a str>,
    pub name: &'a str,
    pub group: &'a str,
    pub command: &'a str,
    pub color: Option<&'a str>,
    pub enter: bool,
}

/// 保存：检查内容和条数上限，通过就做出要写进列表的那一条（名称、分组去首尾空白，命令原样，
/// 新建时才调用 `make_id`）。不通过返回出错字段和原因，列表不动。
pub fn build_item(
    filled: &Filled<'_>,
    existing: &[QuickCommand],
    make_id: impl FnOnce() -> String,
) -> Result<QuickCommand, (Field, String)> {
    if let Some(problem) = validate_field(filled.name, filled.command, filled.group) {
        return Err(problem);
    }
    if filled.id.is_none() && existing.len() >= COMMANDS_MAX {
        return Err((Field::Form, format!("快捷命令最多 {COMMANDS_MAX} 条")));
    }
    let group = filled.group.trim();
    Ok(QuickCommand {
        id: filled.id.map_or_else(make_id, str::to_owned),
        name: filled.name.trim().to_owned(),
        command: filled.command.to_owned(),
        group: (!group.is_empty()).then(|| group.to_owned()),
        enter: filled.enter,
        color: filled.color.map(str::to_owned),
    })
}

/// 发进终端的内容：换行统一成回车（和在终端里按回车一样），要执行的末尾再补一个回车。
pub fn payload(text: &str, enter: bool) -> String {
    let unified = text.replace("\r\n", "\r").replace('\n', "\r");
    let body = unified.trim_end_matches('\r');
    if enter {
        format!("{body}\r")
    } else {
        body.to_owned()
    }
}

/// 多行命令（去掉首尾空白后中间还有换行）：发之前要不要先确认。
pub fn is_multiline(text: &str) -> bool {
    text.trim().contains(['\r', '\n'])
}

/// 多行发送确认框要说的话：`(标题, 正文)`。
pub fn multiline_confirm_text(name: &str, text: &str, all: bool) -> (String, String) {
    let lines = text.trim().split(['\n']).count();
    (
        format!("发送多行命令「{name}」？"),
        format!(
            "{lines} 行，每一行都会回车执行{}。",
            if all {
                "，发到这个标签的所有分屏"
            } else {
                ""
            }
        ),
    )
}

pub fn new_id(now_ms: u128, counter: u64) -> String {
    format!("qc-{now_ms:x}-{counter:06x}")
}

/// 新增或替换（按 id）。
pub fn upsert(list: &[QuickCommand], item: QuickCommand) -> Vec<QuickCommand> {
    let mut next = list.to_vec();
    match next.iter().position(|existing| existing.id == item.id) {
        Some(index) => next[index] = item,
        None => next.push(item),
    }
    next
}

/// 在同一分组内上移 / 下移一位。和同组里相邻的那条交换它们在整个列表里的位置，别的分组的命令
/// 原地不动，所以每个分组里的相对顺序（也就是选了这个分组后命令栏里的顺序）恰好变了这一位。
/// 已经是组内第一 / 最后一条时原样返回 `None`。
pub fn move_in_group(list: &[QuickCommand], id: &str, delta: isize) -> Option<Vec<QuickCommand>> {
    let index = list.iter().position(|item| item.id == id)?;
    let group = &list[index].group;
    let mut target = index as isize + delta;
    while target >= 0 && (target as usize) < list.len() && &list[target as usize].group != group {
        target += delta;
    }
    if target < 0 || target as usize >= list.len() {
        return None;
    }
    let mut next = list.to_vec();
    next.swap(index, target as usize);
    Some(next)
}

/// 这条在自己的分组里还能不能上移 / 下移：`(上移, 下移)`。
pub fn can_move(list: &[QuickCommand], id: &str) -> (bool, bool) {
    (
        move_in_group(list, id, -1).is_some(),
        move_in_group(list, id, 1).is_some(),
    )
}

/// 按名字、命令内容、分组过滤（不分大小写）；搜索词为空返回全部。
pub fn search<'a>(list: &'a [QuickCommand], query: &str) -> Vec<&'a QuickCommand> {
    let text = query.trim().to_lowercase();
    if text.is_empty() {
        return list.iter().collect();
    }
    list.iter()
        .filter(|item| {
            item.name.to_lowercase().contains(&text)
                || item.command.to_lowercase().contains(&text)
                || item
                    .group
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&text)
        })
        .collect()
}

pub struct Section<'a> {
    /// 分组名；空串 = 未分组。
    pub group: String,
    pub items: Vec<&'a QuickCommand>,
}

/// 按分组归拢（管理面板的显示）：分组按第一次出现的顺序，未分组放最后；组内保持列表里的相对顺序。
pub fn sections<'a>(list: &[&'a QuickCommand]) -> Vec<Section<'a>> {
    let mut out: Vec<Section<'a>> = Vec::new();
    let mut ungrouped: Vec<&'a QuickCommand> = Vec::new();
    for item in list {
        match &item.group {
            Some(group) => match out.iter_mut().find(|section| &section.group == group) {
                Some(section) => section.items.push(item),
                None => out.push(Section {
                    group: group.clone(),
                    items: vec![item],
                }),
            },
            None => ungrouped.push(item),
        }
    }
    if !ungrouped.is_empty() {
        out.push(Section {
            group: String::new(),
            items: ungrouped,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(name: &str, command: &str, group: &str) -> Option<String> {
        validate_field(name, command, group).map(|(_, reason)| reason)
    }

    fn cmd(id: &str, name: &str, command: &str, group: Option<&str>) -> QuickCommand {
        QuickCommand {
            id: id.into(),
            name: name.into(),
            command: command.into(),
            group: group.map(Into::into),
            enter: true,
            color: None,
        }
    }

    #[test]
    fn round_trip_keeps_fields_and_drops_unknown_colors() {
        let mut item = cmd("a", "端口", "ss -tlnp", Some("排查"));
        item.color = Some("blue".into());
        item.enter = false;
        let text = serialize_commands(&[item.clone()]);
        assert_eq!(parse_commands(&text), vec![item]);

        let future = r#"[{"id":"a","name":"x","command":"ls","color":"mauve"}]"#;
        assert_eq!(parse_commands(future)[0].color, None, "未知颜色当没设");
        assert!(parse_commands(future)[0].enter, "没写 enter 就是执行");
    }

    #[test]
    fn broken_data_never_takes_the_bar_down() {
        assert!(parse_commands("not json").is_empty());
        assert!(parse_commands("{}").is_empty());
        let mixed =
            r#"[1,{"id":"a","name":"x"},{"id":"b","name":"ok","command":"ls","group":"  "}]"#;
        let list = parse_commands(mixed);
        assert_eq!(list.len(), 1, "坏的单条跳过");
        assert_eq!(list[0].group, None, "只有空白的分组当不分组");
    }

    #[test]
    fn payload_unifies_newlines_and_appends_one_enter() {
        assert_eq!(payload("ls\r\nwhoami\n", true), "ls\rwhoami\r");
        assert_eq!(payload("ls\n\n\n", true), "ls\r", "末尾多余的回车不叠加");
        assert_eq!(payload("ls\n", false), "ls", "只填入不回车");
    }

    #[test]
    fn multiline_means_a_break_inside_the_trimmed_text() {
        assert!(!is_multiline("ls -l\n"));
        assert!(is_multiline("cd /tmp\nls"));
        let (title, body) = multiline_confirm_text("清理", "a\nb\nc", true);
        assert!(title.contains("清理"));
        assert!(body.starts_with("3 行") && body.contains("所有分屏"));
        assert!(
            !multiline_confirm_text("x", "a\nb", false)
                .1
                .contains("所有分屏")
        );
    }

    #[test]
    fn validation_follows_the_sync_rules() {
        assert_eq!(validate("", "ls", ""), Some("名称不能为空".into()));
        assert_eq!(validate("n", "  ", ""), Some("命令不能为空".into()));
        assert!(validate("n", "ls\u{1b}[2J", "").is_some(), "ESC 不让存");
        assert!(validate("n", "a\tb\nc", "").is_none(), "换行和 Tab 可以");
        assert!(validate(&"x".repeat(101), "ls", "").is_some());
        assert!(validate("n", &"x".repeat(8001), "").is_some());
        assert!(validate("n", "ls", &"g".repeat(51)).is_some());
        assert!(validate("n", "ls", "组\u{7}").is_some());
    }

    #[test]
    fn groups_and_the_group_filter() {
        let list = vec![
            cmd("1", "a", "a", Some("B")),
            cmd("2", "b", "b", None),
            cmd("3", "c", "c", Some("A")),
            cmd("4", "d", "d", Some("B")),
        ];
        assert_eq!(groups_of(&list), vec!["B", "A"]);
        assert_eq!(commands_in_group(&list, "").len(), 4);
        assert_eq!(commands_in_group(&list, "B").len(), 2);
        assert_eq!(
            commands_in_group(&list, "没有的组").len(),
            4,
            "组没了退回全部"
        );
    }

    #[test]
    fn moving_swaps_inside_the_group_only() {
        let list = vec![
            cmd("1", "a", "a", Some("B")),
            cmd("2", "b", "b", Some("A")),
            cmd("3", "c", "c", Some("B")),
        ];
        let moved = move_in_group(&list, "3", -1).expect("同组的上一条是 1");
        assert_eq!(
            moved.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["3", "2", "1"],
            "跳过中间别的分组，别的分组原地不动"
        );
        assert_eq!(can_move(&list, "1"), (false, true));
        assert_eq!(can_move(&list, "2"), (false, false), "组里只有一条");
        assert!(move_in_group(&list, "nope", 1).is_none());
    }

    #[test]
    fn search_and_sections() {
        let list = vec![
            cmd("1", "端口", "ss -tlnp", Some("排查")),
            cmd("2", "更新", "apt update", None),
            cmd("3", "磁盘", "df -h", Some("排查")),
        ];
        assert_eq!(search(&list, "SS").len(), 1, "不分大小写、也搜命令内容");
        assert_eq!(search(&list, "排查").len(), 2, "也搜分组");
        assert_eq!(search(&list, "  ").len(), 3);
        let all = search(&list, "");
        let parts = sections(&all);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].group, "排查");
        assert_eq!(parts[1].group, "", "未分组放最后");
    }

    #[test]
    fn upsert_replaces_by_id() {
        let list = vec![cmd("1", "a", "a", None)];
        let replaced = upsert(&list, cmd("1", "新", "a", None));
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].name, "新");
        assert_eq!(upsert(&list, cmd("2", "b", "b", None)).len(), 2);
    }

    #[test]
    fn colors_have_names() {
        for id in COLORS {
            assert!(color_style(id).is_some(), "{id}");
        }
        assert!(color_style("mauve").is_none());
    }

    #[test]
    fn ids_do_not_collide_within_a_millisecond() {
        assert_ne!(new_id(5, 1), new_id(5, 2));
    }

    #[test]
    fn the_dialog_opens_blank_for_new_and_prefilled_for_edit() {
        let blank = Seed::blank();
        assert_eq!(blank.id, None);
        assert_eq!(blank.focus(), Focus::Name, "新建先填名称");
        assert!(blank.name.is_empty() && blank.group.is_empty() && blank.command.is_empty());
        assert!(!blank.enter && blank.color.is_none());

        let mut item = cmd("7", "端口", "ss -tlnp\nuptime", Some("排查"));
        item.enter = false;
        item.color = Some("blue".into());
        let seed = Seed::of(&item);
        assert_eq!(seed.id.as_deref(), Some("7"));
        assert_eq!(dialog_title(false), "新建命令");
        assert_eq!(dialog_title(seed.id.is_some()), "编辑命令");
        assert_eq!(seed.focus(), Focus::Command, "编辑直接落在命令框");
        assert_eq!(seed.name, "端口");
        assert_eq!(seed.group, "排查");
        assert_eq!(seed.command, "ss -tlnp\nuptime");
        assert_eq!(seed.color.as_deref(), Some("blue"));
        assert!(!seed.enter);
        assert_eq!(
            Seed::of(&cmd("8", "x", "y", None)).group,
            "",
            "没分组 = 空串"
        );
    }

    fn filled<'a>(
        id: Option<&'a str>,
        name: &'a str,
        command: &'a str,
        group: &'a str,
    ) -> Filled<'a> {
        Filled {
            id,
            name,
            group,
            command,
            color: None,
            enter: true,
        }
    }

    #[test]
    fn saving_a_new_command_trims_name_and_group_and_keeps_the_command_as_typed() {
        let item = build_item(
            &filled(None, "  端口 ", "ss -tlnp\n", " 排查 "),
            &[],
            || "qc-new".into(),
        )
        .unwrap();
        assert_eq!(item.id, "qc-new");
        assert_eq!(item.name, "端口");
        assert_eq!(item.group.as_deref(), Some("排查"));
        assert_eq!(item.command, "ss -tlnp\n", "命令里的换行原样保留");
        let none = build_item(&filled(None, "a", "ls", "   "), &[], || "i".into()).unwrap();
        assert_eq!(none.group, None, "全空白的分组 = 不分组");
    }

    #[test]
    fn saving_an_edit_keeps_the_id_and_never_asks_for_a_new_one() {
        let list = vec![cmd("1", "a", "a", None)];
        let item = build_item(&filled(Some("1"), "b", "ls", ""), &list, || {
            panic!("编辑不该生成新 id")
        })
        .unwrap();
        assert_eq!(item.id, "1");
        let next = upsert(&list, item);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].name, "b");
    }

    #[test]
    fn validation_errors_point_at_the_field_that_is_wrong() {
        let err = |name: &str, command: &str, group: &str| {
            build_item(&filled(None, name, command, group), &[], || "i".into())
                .unwrap_err()
                .0
        };
        assert_eq!(err("", "ls", ""), Field::Name);
        assert_eq!(err("n", "  ", ""), Field::Command);
        assert_eq!(err("n", "ls\u{1b}[2J", ""), Field::Command);
        assert_eq!(err("n", "ls", &"g".repeat(51)), Field::Group);
        // 名称和命令都不对时先报名称（和字段从上到下的顺序一致）。
        assert_eq!(err("", "", ""), Field::Name);
        assert_eq!(validate("", "ls", ""), Some("名称不能为空".into()));
    }

    #[test]
    fn the_command_limit_applies_to_new_commands_only() {
        let full: Vec<QuickCommand> = (0..COMMANDS_MAX)
            .map(|i| cmd(&i.to_string(), "n", "ls", None))
            .collect();
        let (field, reason) =
            build_item(&filled(None, "n", "ls", ""), &full, || "x".into()).unwrap_err();
        assert_eq!(field, Field::Form);
        assert!(reason.contains("500"));
        assert!(
            build_item(&filled(Some("3"), "n2", "ls", ""), &full, || "x".into()).is_ok(),
            "满了也能改已有的"
        );
    }
}
