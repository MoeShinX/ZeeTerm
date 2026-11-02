//! 主机详情 / 新建面板的纯逻辑：表单校验、怎么处理密码和凭据、拼出保存草稿、凭据和认证方式
//! 是否配套。规则取自 0.2.14 的 `HostDetailsPanel.tsx` 和 `authIdentity.ts`，并保留 V2-F3 的
//! 修复（保存已有主机时未展示的字段 `tags` / `sort` 取库里的原值，不丢）。
//!
//! 不依赖 GPUI：单测直接盯这些规则。

use zssh_core::model::{
    AuthMethod, Host, HostDraft, Identity, IdentityDraft, IdentityKind, ProxyMode,
};

use super::host_store::normalize_address;

/// 这种凭据能不能配这种认证方式（同 `identityFitsAuth`，和后端 `check_identity_matches` 一致）。
///
/// 密码认证只能用密码凭据：私钥类凭据的秘密是私钥口令或整段私钥，配给密码认证就会被当成密码
/// 发给服务器。agent 认证不读凭据里的秘密，只列 agent 凭据，免得看起来像会用到。
pub fn identity_fits_auth(kind: IdentityKind, method: AuthMethod) -> bool {
    match method {
        AuthMethod::Password => kind == IdentityKind::Password,
        AuthMethod::Key => matches!(kind, IdentityKind::KeyFile | IdentityKind::KeyInline),
        AuthMethod::Agent => kind == IdentityKind::Agent,
    }
}

pub fn auth_label(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::Password => "密码",
        AuthMethod::Key => "私钥",
        AuthMethod::Agent => "ssh-agent",
    }
}

/// 表单里的值（输入框已经读成字符串）。
#[derive(Clone, Debug)]
pub struct FormInput<'a> {
    pub name: &'a str,
    pub address: &'a str,
    pub port: &'a str,
    pub username: &'a str,
    /// 密码框；留空表示不改已存的密码。
    pub password: &'a str,
    pub group: Option<i64>,
    pub auth: AuthMethod,
    pub identity: Option<i64>,
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
    /// 没有选凭据时，把密码存成一条新凭据。
    pub remember: bool,
    pub legacy: bool,
}

/// 保存前要对凭据库做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityAction {
    /// 不动凭据库。
    None,
    /// 选了凭据：用填的密码覆盖它的密码。
    Overwrite { id: i64, draft: IdentityDraftLite },
    /// 没选凭据且打开了「存成新凭据」：新建一条。
    Create { draft: IdentityDraftLite },
}

/// 密码凭据草稿里需要的几项（完整草稿在调用方拼，这里只描述要存什么）。
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityDraftLite {
    pub name: String,
    pub secret: String,
}

/// 手写：`secret` 是明文密码，不能进日志 / 断言输出。
impl std::fmt::Debug for IdentityDraftLite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityDraftLite")
            .field("name", &self.name)
            .field("secret", &"<已隐藏>")
            .finish()
    }
}

impl IdentityDraftLite {
    pub fn into_draft(self, id: Option<i64>) -> IdentityDraft {
        IdentityDraft {
            id,
            name: self.name,
            kind: IdentityKind::Password,
            key_path: None,
            secret: Some(self.secret),
            passphrase: None,
        }
    }
}

/// 保存要做的全部：先（可能）动凭据库，再存主机。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    pub identity: IdentityAction,
    pub draft: HostDraft,
}

/// 检查表单、决定怎么处理密码，拼出保存草稿。
///
/// - 名称、地址、用户名不能为空；端口 1–65535；
/// - 只在认证方式是密码、并且填了「真的改过」的密码时才动凭据库：只是点小眼睛看了一下已存的密码、
///   没改（`revealed` 里取回来的值和框里一样），不算改密码，不重写保险库；
/// - 选了凭据就覆盖它的密码；没选凭据且 `remember` 打开就新建一条「<名称> 的密码」；
/// - 地址整理成裸地址（IPv6 去方括号）；已有主机的 `tags` / `sort` 取库里的原值（F3 X1）。
pub fn plan_save(
    input: &FormInput<'_>,
    existing: Option<&Host>,
    identities: &[Identity],
    revealed: Option<(i64, &str)>,
) -> Result<SavePlan, String> {
    let name = input.name.trim();
    let username = input.username.trim();
    if name.is_empty() || input.address.trim().is_empty() || username.is_empty() {
        return Err("名称、地址、用户名都不能为空".to_owned());
    }
    let port = input
        .port
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|port| (1..=65535).contains(port))
        .ok_or_else(|| "端口要在 1-65535 之间".to_owned())? as u16;

    let mut identity_id = input.identity;
    let mut action = IdentityAction::None;
    let unchanged_reveal = matches!(
        (revealed, input.identity),
        (Some((id, value)), Some(current)) if id == current && value == input.password
    );
    let password_changed = !input.password.is_empty() && !unchanged_reveal;
    if password_changed && input.auth == AuthMethod::Password {
        if let Some(id) = identity_id {
            let name = identities
                .iter()
                .find(|item| item.id == id)
                .map_or_else(|| format!("{name} 的密码"), |item| item.name.clone());
            action = IdentityAction::Overwrite {
                id,
                draft: IdentityDraftLite {
                    name,
                    secret: input.password.to_owned(),
                },
            };
        } else if input.remember {
            action = IdentityAction::Create {
                draft: IdentityDraftLite {
                    name: format!("{name} 的密码"),
                    secret: input.password.to_owned(),
                },
            };
        }
    }
    // 密码存成新凭据之后，调用方把新建出来的凭据 id 回填进草稿；这里先放着原来选的。
    let _ = &mut identity_id;

    Ok(SavePlan {
        identity: action,
        draft: HostDraft {
            protocol: None,
            id: existing.map(|host| host.id),
            group_id: input.group,
            name: name.to_owned(),
            host: normalize_address(input.address),
            port,
            username: username.to_owned(),
            auth_method: input.auth,
            identity_id,
            proxy_id: input.proxy,
            jump_host_id: input.jump,
            proxy_mode: input.proxy_mode,
            // 标签、排序这张表单不编辑：已有主机保持库里的原值，新建用默认值。
            tags: existing.map(|host| host.tags.clone()).unwrap_or_default(),
            sort: existing.map_or(0, |host| host.sort),
            legacy_algorithms: input.legacy,
        },
    })
}

/// 原来存的组合就不配套（旧数据或同步来的）：返回要取消选择的原因。
pub fn mismatch_note(identity: &Identity, method: AuthMethod) -> Option<String> {
    (!identity_fits_auth(identity.kind, method))
        .then(|| format!("凭据「{}」和认证方式不配套，已取消选择", identity.name))
}

/// 密码框旁边的提示。
pub fn password_hint(editing: bool, identity_has_secret: bool) -> &'static str {
    if editing && identity_has_secret {
        "留空表示不改保险库里的密码"
    } else {
        "留空表示不保存密码，连接时会提示"
    }
}

/// 密码框右边的小眼睛什么时候出现：选了存过密码的密码凭据（能从保险库取出来看），或者框里有字。
pub fn can_reveal(identity: Option<&Identity>, method: AuthMethod) -> bool {
    method == AuthMethod::Password
        && identity.is_some_and(|item| item.kind == IdentityKind::Password && item.has_secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: i64) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: Some(3),
            name: "web".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 2222,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: Some(7),
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: vec!["blue".to_owned(), "edge".to_owned()],
            sort: 4,
            legacy_algorithms: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn identity(id: i64, name: &str, kind: IdentityKind, has_secret: bool) -> Identity {
        Identity {
            id,
            name: name.to_owned(),
            kind,
            key_path: None,
            has_secret,
        }
    }

    fn input<'a>() -> FormInput<'a> {
        FormInput {
            name: "web",
            address: "192.0.2.10",
            port: "22",
            username: "ops",
            password: "",
            group: None,
            auth: AuthMethod::Password,
            identity: None,
            proxy_mode: ProxyMode::None,
            proxy: None,
            jump: None,
            remember: true,
            legacy: false,
        }
    }

    #[test]
    fn identities_must_fit_the_auth_method() {
        use IdentityKind::*;
        assert!(identity_fits_auth(Password, AuthMethod::Password));
        assert!(
            !identity_fits_auth(KeyFile, AuthMethod::Password),
            "私钥凭据不能当密码发出去"
        );
        assert!(!identity_fits_auth(KeyInline, AuthMethod::Password));
        assert!(identity_fits_auth(KeyFile, AuthMethod::Key));
        assert!(identity_fits_auth(KeyInline, AuthMethod::Key));
        assert!(!identity_fits_auth(Password, AuthMethod::Key));
        assert!(identity_fits_auth(Agent, AuthMethod::Agent));
        assert!(
            !identity_fits_auth(Password, AuthMethod::Agent),
            "agent 只列 agent 凭据"
        );
    }

    #[test]
    fn name_address_and_user_are_required_and_the_port_must_be_valid() {
        for (name, address, user) in [("", "a", "u"), ("n", "  ", "u"), ("n", "a", " ")] {
            let mut form = input();
            form.name = name;
            form.address = address;
            form.username = user;
            assert_eq!(
                plan_save(&form, None, &[], None).unwrap_err(),
                "名称、地址、用户名都不能为空"
            );
        }
        for port in ["", "0", "65536", "abc", "-1", "22.5"] {
            let mut form = input();
            form.port = port;
            assert_eq!(
                plan_save(&form, None, &[], None).unwrap_err(),
                "端口要在 1-65535 之间",
                "{port}"
            );
        }
        let mut form = input();
        form.port = " 65535 ";
        assert_eq!(
            plan_save(&form, None, &[], None).expect("合法").draft.port,
            65535
        );
    }

    #[test]
    fn saving_an_existing_host_keeps_the_tags_and_sort_it_does_not_show() {
        let existing = host(9);
        let mut form = input();
        form.name = "web renamed";
        form.legacy = true;
        let plan = plan_save(&form, Some(&existing), &[], None).expect("合法");
        assert_eq!(plan.draft.id, Some(9));
        assert_eq!(
            plan.draft.tags,
            vec!["blue".to_owned(), "edge".to_owned()],
            "标签不能丢"
        );
        assert_eq!(plan.draft.sort, 4);
        assert_eq!(plan.draft.name, "web renamed");
        let fresh = plan_save(&input(), None, &[], None).expect("合法");
        assert_eq!(fresh.draft.id, None);
        assert!(fresh.draft.tags.is_empty());
        assert_eq!(fresh.draft.sort, 0);
    }

    #[test]
    fn the_address_is_stored_bare_and_trimmed() {
        let mut form = input();
        form.address = "  [2001:db8::1] ";
        assert_eq!(
            plan_save(&form, None, &[], None).expect("合法").draft.host,
            "2001:db8::1"
        );
        form.address = " web.example ";
        assert_eq!(
            plan_save(&form, None, &[], None).expect("合法").draft.host,
            "web.example"
        );
    }

    #[test]
    fn a_typed_password_overwrites_the_chosen_identity_or_makes_a_new_one() {
        let identities = [identity(7, "shared", IdentityKind::Password, true)];
        let mut form = input();
        form.password = "s3cret";
        form.identity = Some(7);
        let plan = plan_save(&form, None, &identities, None).expect("合法");
        assert_eq!(
            plan.identity,
            IdentityAction::Overwrite {
                id: 7,
                draft: IdentityDraftLite {
                    name: "shared".to_owned(),
                    secret: "s3cret".to_owned()
                }
            }
        );
        assert_eq!(plan.draft.identity_id, Some(7));

        form.identity = None;
        let plan = plan_save(&form, None, &identities, None).expect("合法");
        assert!(matches!(
            &plan.identity,
            IdentityAction::Create { draft } if draft.name == "web 的密码"
        ));
        form.remember = false;
        assert_eq!(
            plan_save(&form, None, &identities, None)
                .expect("合法")
                .identity,
            IdentityAction::None,
            "没选凭据又不想存：不动凭据库"
        );
    }

    #[test]
    fn an_empty_password_or_a_non_password_login_never_touches_the_vault() {
        let identities = [identity(7, "shared", IdentityKind::Password, true)];
        let mut form = input();
        form.identity = Some(7);
        assert_eq!(
            plan_save(&form, None, &identities, None)
                .expect("合法")
                .identity,
            IdentityAction::None
        );
        form.password = "typed";
        form.auth = AuthMethod::Key;
        assert_eq!(
            plan_save(&form, None, &identities, None)
                .expect("合法")
                .identity,
            IdentityAction::None,
            "私钥登录时密码框里的字不能写进保险库"
        );
    }

    #[test]
    fn peeking_at_the_stored_password_is_not_a_change() {
        let identities = [identity(7, "shared", IdentityKind::Password, true)];
        let mut form = input();
        form.identity = Some(7);
        form.password = "stored";
        let plan = plan_save(&form, None, &identities, Some((7, "stored"))).expect("合法");
        assert_eq!(
            plan.identity,
            IdentityAction::None,
            "取出来看了一眼，没改，不重写"
        );
        // 取回来的是 A 凭据的密码，现在选的是 B：不能当成没改。
        form.identity = Some(8);
        let other = [
            identity(7, "a", IdentityKind::Password, true),
            identity(8, "b", IdentityKind::Password, true),
        ];
        let plan = plan_save(&form, None, &other, Some((7, "stored"))).expect("合法");
        assert!(matches!(
            plan.identity,
            IdentityAction::Overwrite { id: 8, .. }
        ));
        // 取出来之后又改了：算改。
        form.identity = Some(7);
        form.password = "stored2";
        let plan = plan_save(&form, None, &other, Some((7, "stored"))).expect("合法");
        assert!(matches!(
            plan.identity,
            IdentityAction::Overwrite { id: 7, .. }
        ));
    }

    #[test]
    fn a_mismatched_saved_identity_is_dropped_with_a_note() {
        let key = identity(5, "deploy key", IdentityKind::KeyFile, true);
        assert_eq!(
            mismatch_note(&key, AuthMethod::Password).as_deref(),
            Some("凭据「deploy key」和认证方式不配套，已取消选择")
        );
        assert_eq!(mismatch_note(&key, AuthMethod::Key), None);
    }

    #[test]
    fn password_hint_and_the_eye_follow_what_is_stored() {
        assert_eq!(password_hint(true, true), "留空表示不改保险库里的密码");
        assert_eq!(
            password_hint(false, true),
            "留空表示不保存密码，连接时会提示"
        );
        assert_eq!(
            password_hint(true, false),
            "留空表示不保存密码，连接时会提示"
        );
        let stored = identity(1, "p", IdentityKind::Password, true);
        let empty = identity(2, "q", IdentityKind::Password, false);
        let key = identity(3, "k", IdentityKind::KeyInline, true);
        assert!(can_reveal(Some(&stored), AuthMethod::Password));
        assert!(!can_reveal(Some(&empty), AuthMethod::Password));
        assert!(!can_reveal(Some(&key), AuthMethod::Password));
        assert!(!can_reveal(Some(&stored), AuthMethod::Key));
        assert!(!can_reveal(None, AuthMethod::Password));
    }

    #[test]
    fn debug_output_never_shows_the_password() {
        let lite = IdentityDraftLite {
            name: "n".to_owned(),
            secret: "hunter2".to_owned(),
        };
        assert!(!format!("{lite:?}").contains("hunter2"));
    }

    #[test]
    fn saving_through_the_plan_keeps_hidden_fields_and_rewrites_the_vault_only_on_a_real_change() {
        use zssh_core::model::{GroupDraft, IdentityDraft};
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");
        let group = db
            .group_save(&GroupDraft {
                id: None,
                name: "g".to_owned(),
                parent_id: None,
                sort: 0,
            })
            .expect("分组");
        let vault = db
            .identity_save(&IdentityDraft {
                id: None,
                name: "shared".to_owned(),
                kind: IdentityKind::Password,
                key_path: None,
                secret: Some("stored".to_owned()),
                passphrase: None,
            })
            .expect("凭据");
        let mut first = input();
        first.group = Some(group.id);
        first.identity = Some(vault.id);
        let created = db
            .host_save(
                &plan_save(&first, None, std::slice::from_ref(&vault), None)
                    .expect("合法")
                    .draft,
            )
            .expect("新建");
        // 给它一个标签和排序（表单不编辑它们）。
        let mut tagged = zssh_core::model::HostDraft {
            protocol: None,
            id: Some(created.id),
            group_id: created.group_id,
            name: created.name.clone(),
            host: created.host.clone(),
            port: created.port,
            username: created.username.clone(),
            auth_method: created.auth_method,
            identity_id: created.identity_id,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: created.proxy_mode,
            tags: vec!["blue".to_owned()],
            sort: 0,
            legacy_algorithms: false,
        };
        tagged.sort = 0;
        let tagged = db.host_save(&tagged).expect("加标签");

        // 只改名字再保存：标签还在；点小眼睛看过（取回的明文没改）不重写保险库。
        let mut renamed = input();
        renamed.name = "web renamed";
        renamed.group = tagged.group_id;
        renamed.identity = Some(vault.id);
        renamed.password = "stored";
        let plan = plan_save(
            &renamed,
            Some(&tagged),
            std::slice::from_ref(&vault),
            Some((vault.id, "stored")),
        )
        .expect("合法");
        assert_eq!(plan.identity, IdentityAction::None);
        let saved = db.host_save(&plan.draft).expect("保存");
        assert_eq!(saved.name, "web renamed");
        assert_eq!(
            saved.tags,
            vec!["blue".to_owned()],
            "标签不能因为改了名字就丢"
        );
        assert_eq!(
            db.identity_saved_password(vault.id)
                .expect("取密码")
                .as_str(),
            "stored"
        );

        // 真的改了密码：覆盖选中的凭据。
        renamed.password = "changed";
        let plan = plan_save(
            &renamed,
            Some(&saved),
            std::slice::from_ref(&vault),
            Some((vault.id, "stored")),
        )
        .expect("合法");
        let IdentityAction::Overwrite { id, draft } = plan.identity else {
            panic!("应该覆盖凭据");
        };
        db.identity_save(&draft.into_draft(Some(id))).expect("覆盖");
        assert_eq!(
            db.identity_saved_password(vault.id)
                .expect("取密码")
                .as_str(),
            "changed"
        );
    }
}
