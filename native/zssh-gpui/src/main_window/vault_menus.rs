//! 主机和分组的右键菜单（0.2.14 `VaultView.tsx` 的 `HostMenu` / `GroupBranch` 菜单）。
//!
//! 菜单在打开时才生成，所以把画的时候就能确定的状态（能不能复制密码、分组列表、上移下移是否置灰）
//! 先装进 [`HostMenuSpec`] / [`GroupMenuSpec`] 带进闭包。

use ::gpui::ClickEvent;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::*;

use super::page_logic::{OrderState, flatten_groups, ungrouped_move_label};
use super::{CopyField, MainWindow};

/// 「移动到分组」子菜单里的一项。
#[derive(Clone)]
pub struct MoveTarget {
    pub group: Option<i64>,
    pub label: String,
    pub depth: usize,
    pub current: bool,
}

/// 一台主机的右键菜单要用的状态。
#[derive(Clone)]
pub struct HostMenuSpec {
    pub host_id: i64,
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
    let mut menu = menu
        .item(
            PopupMenuItem::new("连接")
                .icon(IconName::Play)
                .on_click(run(entity, move |this, window, cx| {
                    this.open_host(id, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new("在新标签连接").on_click(run(entity, move |this, window, cx| {
                this.open_host_in_new_tab(id, window, cx);
            })),
        )
        .item(
            PopupMenuItem::new("打开 SFTP")
                .icon(crate::icons::Ico::FolderTree)
                .on_click(run(entity, move |this, window, cx| {
                    this.open_sftp(id, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new("编辑")
                .icon(crate::icons::Ico::Pencil)
                .on_click(run(entity, move |this, window, cx| {
                    this.edit_host(id, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new("复制主机名").on_click(run(entity, move |this, window, cx| {
                this.copy_host_field(id, CopyField::Name, window, cx);
            })),
        )
        .item(
            PopupMenuItem::new("复制地址").on_click(run(entity, move |this, window, cx| {
                this.copy_host_field(id, CopyField::Address, window, cx);
            })),
        );
    if spec.can_copy_password {
        menu = menu.item(
            PopupMenuItem::new("复制密码").on_click(run(entity, move |this, window, cx| {
                this.copy_host_password(id, window, cx)
            })),
        );
    }
    menu = menu
        .item(
            PopupMenuItem::new("复制连接信息").on_click(run(entity, move |this, window, cx| {
                this.copy_host_info(id, window, cx)
            })),
        )
        .item(
            PopupMenuItem::new("安装公钥…")
                .icon(crate::icons::Ico::KeyRound)
                .on_click(run(entity, move |this, window, cx| {
                    this.open_install_key_dialog(id, window, cx);
                })),
        )
        .item(
            PopupMenuItem::new("端口转发…")
                .icon(crate::icons::Ico::ArrowLeftRight)
                .on_click(run(entity, move |this, _window, cx| {
                    this.open_forwards(Some(id), cx)
                })),
        );
    let targets = spec.targets.clone();
    let move_entity = entity.clone();
    menu = menu.submenu("移动到分组", window, cx, move |sub, _window, _cx| {
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
    });
    menu = menu
        .item(
            PopupMenuItem::new(with_hint("上移", spec.order))
                .icon(IconName::ArrowUp)
                .disabled(order_locked || !spec.step.0)
                .on_click(run(entity, move |this, window, cx| {
                    this.step_host(id, -1, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new(with_hint("下移", spec.order))
                .icon(IconName::ArrowDown)
                .disabled(order_locked || !spec.step.1)
                .on_click(run(entity, move |this, window, cx| {
                    this.step_host(id, 1, window, cx)
                })),
        );
    if spec.reset_top {
        menu = menu.item(
            PopupMenuItem::new("按名字重新排序（顶层主机）")
                .icon(crate::icons::Ico::ArrowDownAZ)
                .on_click(run(entity, |this, window, cx| {
                    this.reset_layer_order(None, window, cx)
                })),
        );
    }
    menu.separator().item(
        PopupMenuItem::new("删除")
            .icon(crate::icons::Ico::Trash)
            .on_click(run(entity, move |this, _window, cx| {
                this.ask_delete_host(id, cx)
            })),
    )
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
