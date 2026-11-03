//! 主机和分组的右键菜单（0.2.14 `VaultView.tsx` 的 `HostMenu` / `GroupBranch` 菜单）。
//!
//! 菜单在打开时才生成，所以把画的时候就能确定的状态（能不能复制密码、分组列表、上移下移是否置灰）
//! 先装进 [`HostMenuSpec`] / [`GroupMenuSpec`] 带进闭包。

use ::gpui::ClickEvent;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::*;

use super::page_logic::{OrderState, flatten_groups, ungrouped_move_label};
use super::protocol_form::{Caps, SSH_ONLY_HINT};
use super::{CopyField, MainWindow};

/// 「移动到分组」子菜单里的一项。
#[derive(Clone)]
pub struct MoveTarget {
    pub group: Option<i64>,
    pub label: String,
    pub depth: usize,
    pub current: bool,
}

/// 只有 SSH 主机能用的菜单项：Telnet / 串口主机上置灰，悬停说明「仅 SSH 主机可用」。
pub fn ssh_only_item(
    label: &'static str,
    icon: Option<crate::icons::Ico>,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    if enabled {
        let item = PopupMenuItem::new(label).on_click(on_click);
        return match icon {
            Some(icon) => item.icon(icon),
            None => item,
        };
    }
    let item = PopupMenuItem::element(move |_window, _cx| {
        div()
            .id(label)
            .w_full()
            .tooltip(|window, cx| Tooltip::new(SSH_ONLY_HINT).build(window, cx))
            .child(label)
    })
    .disabled(true);
    match icon {
        Some(icon) => item.icon(icon),
        None => item,
    }
}

/// 一台主机的右键菜单要用的状态。
#[derive(Clone)]
pub struct HostMenuSpec {
    pub host_id: i64,
    /// 这台主机的协议决定哪些项能用（SFTP / 端口转发 / 安装公钥只有 SSH）。
    pub caps: Caps,
    pub can_copy_password: bool,
    pub targets: Vec<MoveTarget>,
    /// 上移 / 下移：`(能上移, 能下移)`。
    pub step: (bool, bool),
    /// 现在能不能调整顺序（置灰时在菜单项上写原因）。
    pub order: OrderState,
    /// 平铺模式下顶层主机多一项「按名字重新排序（顶层主机）」。
    pub reset_top: bool,
}

impl HostMenuSpec {
    pub fn new(window_state: &MainWindow, host_id: i64, order: OrderState) -> Self {
        let store = &window_state.store;
        let host = store.host(host_id);
        let current_group = host.and_then(|h| h.group_id);
        let flat = store.flat();
        let mut targets = vec![MoveTarget {
            group: None,
            label: ungrouped_move_label(flat).to_owned(),
            depth: 0,
            current: current_group.is_none(),
        }];
        for (group, depth) in flatten_groups(store.groups()) {
            targets.push(MoveTarget {
                group: Some(group.id),
                label: group.name.clone(),
                depth,
                current: current_group == Some(group.id),
            });
        }
        Self {
            host_id,
            caps: host.map_or(Caps::ALL, |h| Caps::of(h.protocol.protocol)),
            can_copy_password: host
                .is_some_and(|h| super::page_logic::can_copy_password(h, store.identities())),
            targets,
            step: window_state.host_step_state(host_id),
            order,
            reset_top: flat && current_group.is_none(),
        }
    }
}

fn run(
    entity: &Entity<MainWindow>,
    f: impl Fn(&mut MainWindow, &mut Window, &mut Context<MainWindow>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let entity = entity.clone();
    move |_, window, cx| {
        entity.update(cx, |this, cx| f(this, window, cx));
    }
}

fn with_hint(label: &str, order: OrderState) -> String {
    match order.hint() {
        Some(hint) => format!("{label}（{hint}）"),
        None => label.to_owned(),
    }
}

/// 主机右键菜单里的一项（从上到下的次序就是 [`host_menu_items`] 返回的次序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMenuItem {
    Connect,
    ConnectInNewTab,
    OpenSftp,
    CopyName,
    CopyAddress,
    CopyPassword,
    CopyInfo,
    InstallKey,
    Forwards,
    MoveToGroup,
    MoveUp,
    MoveDown,
    ResetTopOrder,
    Separator,
    /// 弹出主机属性对话框（原来的「编辑」）：紧挨在「删除」上面。
    Properties,
    Delete,
}

impl HostMenuItem {
    /// 菜单上写的字（分隔线没有字）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Connect => "连接",
            Self::ConnectInNewTab => "在新标签连接",
            Self::OpenSftp => "打开 SFTP",
            Self::CopyName => "复制主机名",
            Self::CopyAddress => "复制地址",
            Self::CopyPassword => "复制密码",
            Self::CopyInfo => "复制连接信息",
            Self::InstallKey => "安装公钥…",
            Self::Forwards => "端口转发…",
            Self::MoveToGroup => "移动到分组",
            Self::MoveUp => "上移",
            Self::MoveDown => "下移",
            Self::ResetTopOrder => "按名字重新排序（顶层主机）",
            Self::Separator => "",
            Self::Properties => "属性…",
            Self::Delete => "删除",
        }
    }
}

/// 这台主机的右键菜单有哪些项、什么次序：「属性…」在「删除」正上方（两者和上面的移动 / 排序之间
/// 一条分隔线），其余项的次序不变；没法复制密码的主机没有「复制密码」，平铺模式下顶层主机多一项
/// 「按名字重新排序」。
pub fn host_menu_items(spec: &HostMenuSpec) -> Vec<HostMenuItem> {
    use HostMenuItem::*;
    let mut items = vec![Connect, ConnectInNewTab, OpenSftp, CopyName, CopyAddress];
    if spec.can_copy_password {
        items.push(CopyPassword);
    }
    items.extend([
        CopyInfo,
        InstallKey,
        Forwards,
        MoveToGroup,
        MoveUp,
        MoveDown,
    ]);
    if spec.reset_top {
        items.push(ResetTopOrder);
    }
    items.extend([Separator, Properties, Delete]);
    items
}

/// 主机的右键菜单。
pub fn host_menu(
    menu: PopupMenu,
    spec: &HostMenuSpec,
    entity: &Entity<MainWindow>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let id = spec.host_id;
    let order_locked = spec.order != OrderState::Manual;
    let mut menu = menu;
    for item in host_menu_items(spec) {
        menu = match item {
            HostMenuItem::Connect => menu.item(
                PopupMenuItem::new(item.label())
                    .icon(IconName::Play)
                    .on_click(run(entity, move |this, window, cx| {
                        this.open_host(id, window, cx)
                    })),
            ),
            HostMenuItem::ConnectInNewTab => menu.item(PopupMenuItem::new(item.label()).on_click(
                run(entity, move |this, window, cx| {
                    this.open_host_in_new_tab(id, window, cx);
                }),
            )),
            HostMenuItem::OpenSftp => menu.item(ssh_only_item(
                item.label(),
                Some(crate::icons::Ico::FolderTree),
                spec.caps.sftp,
                run(entity, move |this, window, cx| {
                    this.open_sftp(id, window, cx)
                }),
            )),
            HostMenuItem::CopyName => menu.item(PopupMenuItem::new(item.label()).on_click(run(
                entity,
                move |this, window, cx| {
                    this.copy_host_field(id, CopyField::Name, window, cx);
                },
            ))),
            HostMenuItem::CopyAddress => menu.item(PopupMenuItem::new(item.label()).on_click(run(
                entity,
                move |this, window, cx| {
                    this.copy_host_field(id, CopyField::Address, window, cx);
                },
            ))),
            HostMenuItem::CopyPassword => menu.item(PopupMenuItem::new(item.label()).on_click(
                run(entity, move |this, window, cx| {
                    this.copy_host_password(id, window, cx)
                }),
            )),
            HostMenuItem::CopyInfo => menu.item(
                PopupMenuItem::new(item.label()).on_click(run(entity, move |this, window, cx| {
                    this.copy_host_info(id, window, cx)
                })),
            ),
            HostMenuItem::InstallKey => menu.item(ssh_only_item(
                item.label(),
                Some(crate::icons::Ico::KeyRound),
                spec.caps.ssh,
                run(entity, move |this, window, cx| {
                    this.open_install_key_dialog(id, window, cx);
                }),
            )),
            HostMenuItem::Forwards => menu.item(ssh_only_item(
                item.label(),
                Some(crate::icons::Ico::ArrowLeftRight),
                spec.caps.port_forward,
                run(entity, move |this, window, cx| {
                    this.open_forwards(Some(id), window, cx)
                }),
            )),
            HostMenuItem::MoveToGroup => {
                let targets = spec.targets.clone();
                let move_entity = entity.clone();
                menu.submenu(item.label(), window, cx, move |sub, _window, _cx| {
                    let mut sub = sub
                        .min_w(::gpui::px(180.0))
                        .max_h(::gpui::px(320.0))
                        .scrollable(true);
                    for (index, target) in targets.iter().enumerate() {
                        let group = target.group;
                        let label = format!("{}{}", "　".repeat(target.depth), target.label);
                        sub = sub.item(
                            PopupMenuItem::new(label)
                                .checked(target.current)
                                .disabled(target.current)
                                .on_click(run(&move_entity, move |this, window, cx| {
                                    this.move_host_ui(id, group, window, cx);
                                })),
                        );
                        if index == 0 && targets.len() > 1 {
                            sub = sub.separator();
                        }
                    }
                    sub
                })
            }
            HostMenuItem::MoveUp => menu.item(
                PopupMenuItem::new(with_hint(item.label(), spec.order))
                    .icon(IconName::ArrowUp)
                    .disabled(order_locked || !spec.step.0)
                    .on_click(run(entity, move |this, window, cx| {
                        this.step_host(id, -1, window, cx)
                    })),
            ),
            HostMenuItem::MoveDown => menu.item(
                PopupMenuItem::new(with_hint(item.label(), spec.order))
                    .icon(IconName::ArrowDown)
                    .disabled(order_locked || !spec.step.1)
                    .on_click(run(entity, move |this, window, cx| {
                        this.step_host(id, 1, window, cx)
                    })),
            ),
            HostMenuItem::ResetTopOrder => menu.item(
                PopupMenuItem::new(item.label())
                    .icon(crate::icons::Ico::ArrowDownAZ)
                    .on_click(run(entity, |this, window, cx| {
                        this.reset_layer_order(None, window, cx)
                    })),
            ),
            HostMenuItem::Separator => menu.separator(),
            HostMenuItem::Properties => menu.item(
                PopupMenuItem::new(item.label())
                    .icon(crate::icons::Ico::Pencil)
                    .on_click(run(entity, move |this, window, cx| {
                        this.edit_host(id, window, cx)
                    })),
            ),
            HostMenuItem::Delete => menu.item(
                PopupMenuItem::new(item.label())
                    .icon(crate::icons::Ico::Trash)
                    .on_click(run(entity, move |this, _window, cx| {
                        this.ask_delete_host(id, cx)
                    })),
            ),
        };
    }
    menu
}
/// 分组行的右键菜单需要的状态。
#[derive(Clone)]
pub struct GroupMenuSpec {
    pub group_id: i64,
    pub step: (bool, bool),
    /// 搜索时上移 / 下移置灰（过滤后的列表不连续）。
    pub locked: bool,
}

impl GroupMenuSpec {
    pub fn new(window_state: &MainWindow, group_id: i64, locked: bool) -> Self {
        Self {
            group_id,
            step: window_state.group_step_state(group_id),
            locked,
        }
    }
}

pub fn group_menu(menu: PopupMenu, spec: &GroupMenuSpec, entity: &Entity<MainWindow>) -> PopupMenu {
    let id = spec.group_id;
    menu.item(
        PopupMenuItem::new("新建子分组")
            .icon(crate::icons::Ico::FolderPlus)
            .on_click(run(entity, move |this, window, cx| {
                this.new_group(Some(id), window, cx)
            })),
    )
    .item(
        PopupMenuItem::new("重命名")
            .icon(crate::icons::Ico::Pencil)
            .on_click(run(entity, move |this, window, cx| {
                this.rename_group(id, window, cx)
            })),
    )
    .separator()
    .item(
        PopupMenuItem::new("上移")
            .icon(IconName::ArrowUp)
            .disabled(spec.locked || !spec.step.0)
            .on_click(run(entity, move |this, window, cx| {
                this.step_group(id, -1, window, cx)
            })),
    )
    .item(
        PopupMenuItem::new("下移")
            .icon(IconName::ArrowDown)
            .disabled(spec.locked || !spec.step.1)
            .on_click(run(entity, move |this, window, cx| {
                this.step_group(id, 1, window, cx)
            })),
    )
    .item(
        PopupMenuItem::new("按名字重新排序")
            .icon(crate::icons::Ico::ArrowDownAZ)
            .on_click(run(entity, move |this, window, cx| {
                this.reset_layer_order(Some(id), window, cx)
            })),
    )
    .separator()
    .item(
        PopupMenuItem::new("删除分组")
            .icon(crate::icons::Ico::Trash)
            .on_click(run(entity, move |this, _window, cx| {
                this.ask_delete_group(id, cx)
            })),
    )
}

/// 「未分组」行的右键菜单。
pub fn ungrouped_menu(menu: PopupMenu, entity: &Entity<MainWindow>) -> PopupMenu {
    menu.item(
        PopupMenuItem::new("新建分组")
            .icon(crate::icons::Ico::FolderPlus)
            .on_click(run(entity, |this, window, cx| {
                this.new_group(None, window, cx)
            })),
    )
    .item(
        PopupMenuItem::new("重命名（建成分组）…")
            .icon(crate::icons::Ico::Pencil)
            .on_click(run(entity, |this, window, cx| {
                this.adopt_ungrouped(window, cx)
            })),
    )
    .separator()
    .item(
        PopupMenuItem::new("按名字重新排序")
            .icon(crate::icons::Ico::ArrowDownAZ)
            .on_click(run(entity, |this, window, cx| {
                this.reset_layer_order(None, window, cx)
            })),
    )
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{HostMenuItem, HostMenuSpec, host_menu_items};
    use crate::main_window::page_logic::OrderState;
    use crate::main_window::protocol_form::Caps;

    fn spec() -> HostMenuSpec {
        HostMenuSpec {
            host_id: 1,
            caps: Caps::ALL,
            can_copy_password: true,
            targets: Vec::new(),
            step: (true, true),
            order: OrderState::Manual,
            reset_top: false,
        }
    }

    fn labels(spec: &HostMenuSpec) -> Vec<&'static str> {
        host_menu_items(spec)
            .into_iter()
            .map(HostMenuItem::label)
            .collect()
    }

    #[test]
    fn properties_sits_right_above_delete_and_edit_is_gone() {
        let items = labels(&spec());
        assert_eq!(items[items.len() - 2..], ["属性…", "删除"]);
        assert!(!items.contains(&"编辑"), "「编辑」改名成了「属性…」");
        assert_eq!(
            items.iter().filter(|label| **label == "属性…").count(),
            1,
            "只有一个入口"
        );
    }

    #[test]
    fn the_other_items_keep_their_order() {
        assert_eq!(
            labels(&spec()),
            [
                "连接",
                "在新标签连接",
                "打开 SFTP",
                "复制主机名",
                "复制地址",
                "复制密码",
                "复制连接信息",
                "安装公钥…",
                "端口转发…",
                "移动到分组",
                "上移",
                "下移",
                "",
                "属性…",
                "删除"
            ]
        );
    }

    #[test]
    fn a_separator_divides_the_move_items_from_properties_and_delete() {
        let items = host_menu_items(&spec());
        let at = items
            .iter()
            .position(|item| *item == HostMenuItem::Properties)
            .expect("有属性");
        assert_eq!(items[at - 1], HostMenuItem::Separator);
    }

    #[test]
    fn optional_items_appear_only_when_they_apply() {
        let mut host = spec();
        host.can_copy_password = false;
        assert!(!labels(&host).contains(&"复制密码"));
        host.reset_top = true;
        let items = labels(&host);
        assert!(items.contains(&"按名字重新排序（顶层主机）"));
        assert_eq!(
            items[items.len() - 2..],
            ["属性…", "删除"],
            "属性仍在删除正上方"
        );
    }
}
