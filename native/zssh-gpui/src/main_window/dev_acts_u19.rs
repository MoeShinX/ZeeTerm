//! 调试驱动里 V2-U19（从 Z-SSH 迁移）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩两个空函数。
//! 全部用临时目录里造的假数据，不碰真实的 Z-SSH / ZeeTerm 数据目录：
//!
//! - `u19_fake_zssh:<目录>`：在目录里造一份假的 Z-SSH 数据（`data.db` + `known_hosts`），示例地址取文档保留段；
//! - `u19_fake_backup:<路径>|<密码>`：造一份假的 `.zsshbak`（和 Z-SSH 导出的格式相同）；
//! - `u19_hold:<目录>` / `u19_release`：像正在运行的 Z-SSH 一样占着 `data.db`（读写、允许别人读写），再放手；
//! - `u19_rescan`：让主窗口重新探测一次本机有没有 Z-SSH 数据（欢迎页提示卡用）；
//! - `u19_tip_wait:<yes|no>`：等欢迎页提示卡现在是 / 不是显示的；`u19_tip:<import|dismiss|link>` 点它上面的按钮；
//! - `u19_dismissed:<yes|no>`：检查库里「不再提示」的标记；
//! - `migrate_open[:start]`：打开设置 → 同步并滚到最上面（带 `start` 就顺手读本机 Z-SSH 并预览）；
//! - `migrate_do:<动作>[:<参数>]`、`migrate_wait:<项>:<值>`、`migrate_assert:<项>:<值>`：操作和检查迁移块
//!   （项名以 `_has` 结尾是「包含」，`_not` 取反；见 `migrate/dev.rs`）。

#[cfg(debug_assertions)]
mod real {
    use std::sync::Mutex;

    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;

    const KNOWN: &[&str] = &[
        "u19_fake_zssh",
        "u19_fake_backup",
        "u19_hold",
        "u19_release",
        "u19_rescan",
        "u19_tip_wait",
        "u19_tip",
        "u19_dismissed",
        "migrate_open",
        "migrate_do",
        "migrate_wait",
        "migrate_assert",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    /// 占着"Z-SSH 的库文件"的句柄（读写、允许别人读写，和 SQLite 打开库的方式一样）。
    static HELD: Mutex<Option<std::fs::File>> = Mutex::new(None);

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        // 值里可能有 `:`（Windows 路径），所以只拆前两段，剩下的整体当值。
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(run_inner(this, name, a, b, window, cx))
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        a: &str,
        b: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        // `u19_fake_zssh:C:/dir` 这样的路径里有冒号：把 a 和 b 重新接回去。
        let joined = if b.is_empty() {
            a.to_owned()
        } else {
            format!("{a}:{b}")
        };
        match name {
            "u19_fake_zssh" => fake_zssh(std::path::Path::new(&joined))?,
            "u19_fake_backup" => {
                let (path, password) = joined
                    .split_once('|')
                    .ok_or("格式：u19_fake_backup:<路径>|<密码>")?;
                fake_backup(std::path::Path::new(path), password)?;
            }
            "u19_hold" => {
                // Windows 上用 share_mode 允许别的进程同时打开；macOS / Linux 的普通打开
                // 本来就共享，不需要这一步。
                let mut options = std::fs::OpenOptions::new();
                options.read(true).write(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    options.share_mode(3);
                }
                let file = options
                    .open(std::path::Path::new(&joined).join("data.db"))
                    .map_err(|err| format!("占不住：{err}"))?;
                *HELD.lock().unwrap_or_else(|p| p.into_inner()) = Some(file);
            }
            "u19_release" => {
                *HELD.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
            "u19_rescan" => {
                this.migrate_hint.scanned = false;
                this.migrate_hint.found = false;
                this.scan_zssh_once(window, cx);
            }
            "u19_tip_wait" => {
                let want = yes_no(a)?;
                if this.zssh_tip_visible() != want {
                    return Ok(Outcome::Pending);
                }
            }
            "u19_tip" => match a {
                "import" => this.open_migrate(true, window, cx),
                "dismiss" => this.dismiss_zssh_tip(cx),
                "link" => {
                    let start = this.migrate_hint.found;
                    this.open_migrate(start, window, cx);
                }
                _ => return Err("按钮写 import / dismiss / link".to_owned()),
            },
            "u19_dismissed" => {
                let want = yes_no(a)?;
                let have = this
                    .store
                    .db()
                    .map(crate::migrate::is_dismissed)
                    .map_err(|err| err.to_string())?;
                if have != want {
                    return Err(format!("「不再提示」标记是 {have}，预期 {want}"));
                }
            }
            "migrate_open" => this.open_migrate(a == "start", window, cx),
            "migrate_do" => {
                let migrate = migrate(this)?;
                let (action, arg) = (a, b.to_owned());
                migrate.update(cx, |m, cx| m.dev_do(action, &arg, window, cx))?;
            }
            "migrate_wait" | "migrate_assert" => {
                let migrate = migrate(this)?;
                let key = a;
                let query = key
                    .strip_suffix("_not")
                    .unwrap_or(key)
                    .trim_end_matches("_has");
                let have = migrate.read(cx).dev_query(query, cx)?;
                let (base, negate) = key
                    .strip_suffix("_not")
                    .map_or((key, false), |base| (base, true));
                let ok = if base.ends_with("_has") {
                    have.contains(b)
                } else {
                    have == b
                };
                if ok == negate {
                    if name == "migrate_wait" {
                        return Ok(Outcome::Pending);
                    }
                    return Err(format!("{a} 和预期不同（现在是：{}）", short(&have)));
                }
            }
            _ => return Err(format!("不认识的动作：{name}")),
        }
        Ok(Outcome::Done)
    }

    /// 报错里只带一小段（屏幕文字可能很长）。
    fn short(text: &str) -> String {
        text.chars().take(80).collect()
    }

    fn yes_no(raw: &str) -> Result<bool, String> {
        match raw {
            "yes" => Ok(true),
            "no" => Ok(false),
            _ => Err("参数只能是 yes 或 no".to_owned()),
        }
    }

    fn migrate(this: &MainWindow) -> Result<Entity<crate::migrate::MigrateDemo>, String> {
        this.migrate_settings
            .clone()
            .ok_or_else(|| "迁移块还没有建（先 act:migrate_open）".to_owned())
    }

    /// 假的 Z-SSH 数据：两个分组、两条凭据（一条密码、一条内联私钥）、一个代理、五台主机（含跳板机和代理）、
    /// 三条快捷命令、一条已信任的主机指纹。库用 ZeeTerm 自己的格式写（老版本 v11 的读取由核心测试覆盖）。
    fn fake_zssh(dir: &std::path::Path) -> Result<(), String> {
        use zssh_core::model::{
            AuthMethod, GroupDraft, HostDraft, IdentityDraft, IdentityKind, ProxyDraft, ProxyKind,
            ProxyMode,
        };
        use zssh_core::store::Database;

        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let db = Database::open(&dir.join("data.db")).map_err(|e| e.to_string())?;
        let err = |e: zssh_core::error::ZsshError| e.to_string();
        let prod = db
            .group_save(&GroupDraft {
                id: None,
                name: "生产".to_owned(),
                parent_id: None,
                sort: 0,
            })
            .map_err(err)?;
        let lab = db
            .group_save(&GroupDraft {
                id: None,
                name: "实验室".to_owned(),
                parent_id: None,
                sort: 1,
            })
            .map_err(err)?;
        let password = db
            .identity_save(&IdentityDraft {
                id: None,
                name: "运维密码".to_owned(),
                kind: IdentityKind::Password,
                key_path: None,
                secret: Some("example-not-a-real-password".to_owned()),
                passphrase: None,
            })
            .map_err(err)?;
        let key = db
            .identity_save(&IdentityDraft {
                id: None,
                name: "部署密钥".to_owned(),
                kind: IdentityKind::KeyInline,
                key_path: None,
                secret: Some("EXAMPLE-NOT-A-REAL-KEY".to_owned()),
                passphrase: None,
            })
            .map_err(err)?;
        let proxy = db
            .proxy_save(&ProxyDraft {
                id: None,
                name: "办公室出口".to_owned(),
                kind: ProxyKind::Socks5,
                host: "198.51.100.1".to_owned(),
                port: 1080,
                username: None,
                secret: None,
            })
            .map_err(err)?;
        let key_id = Some(key.id);
        let host = |name: &str,
                    address: &str,
                    group: Option<i64>,
                    identity: Option<i64>,
                    proxy_id: Option<i64>,
                    jump: Option<i64>| {
            db.host_save(&HostDraft {
                protocol: None,
                id: None,
                group_id: group,
                name: name.to_owned(),
                host: address.to_owned(),
                port: 22,
                username: "ops".to_owned(),
                auth_method: if identity == key_id {
                    AuthMethod::Key
                } else {
                    AuthMethod::Password
                },
                identity_id: identity,
                proxy_id,
                jump_host_id: jump,
                proxy_mode: if proxy_id.is_some() {
                    ProxyMode::Custom
                } else {
                    ProxyMode::Default
                },
                tags: Vec::new(),
                sort: 0,
                legacy_algorithms: false,
            })
            .map(|h| h.id)
            .map_err(|e| e.to_string())
        };
        let gateway = host(
            "gateway-01",
            "192.0.2.1",
            Some(prod.id),
            Some(password.id),
            None,
            None,
        )?;
        host(
            "web-01",
            "192.0.2.11",
            Some(prod.id),
            Some(password.id),
            None,
            Some(gateway),
        )?;
        host(
            "web-02",
            "192.0.2.12",
            Some(prod.id),
            Some(password.id),
            None,
            Some(gateway),
        )?;
        host(
            "db-01",
            "192.0.2.21",
            Some(prod.id),
            Some(key.id),
            Some(proxy.id),
            None,
        )?;
        host("lab-box", "192.0.2.31", Some(lab.id), None, None, None)?;
        let quick = serde_json::to_string(
            r#"[{"id":"q1","name":"磁盘","command":"df -h","enter":true},{"id":"q2","name":"进程","command":"ps aux","enter":true},{"id":"q3","name":"日志","command":"journalctl -n 100","enter":true}]"#,
        )
        .map_err(|e| e.to_string())?;
        db.setting_set("quick_commands", &quick).map_err(err)?;
        drop(db);
        std::fs::write(
            dir.join("known_hosts"),
            "192.0.2.11 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n",
        )
        .map_err(|e| e.to_string())
    }

    /// 假的备份文件：用 ZeeTerm 的离线导出写（和 Z-SSH 导出的是同一种文件）。数据取自一个临时库。
    fn fake_backup(path: &std::path::Path, password: &str) -> Result<(), String> {
        use zssh_core::store::Database;
        use zssh_core::sync::engine::SyncEngine;
        use zssh_core::sync::sealer::DpapiSealer;

        let scratch = path.with_extension("src");
        fake_zssh(&scratch)?;
        let db = Database::open(&scratch.join("data.db")).map_err(|e| e.to_string())?;
        let engine = SyncEngine::new(
            &db,
            &DpapiSealer,
            "fake-zssh-device",
            &scratch,
            Some(scratch.join("known_hosts")),
            None,
        );
        let bytes = engine.export_offline(password).map_err(|e| e.to_string())?;
        drop(db);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, bytes).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(())
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
