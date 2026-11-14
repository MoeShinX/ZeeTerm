use super::*;

fn small(entries: usize, per_target: usize, total: usize) -> HistoryStore {
    HistoryStore::with_limits(Limits {
        entries_per_target: entries,
        bytes_per_target: per_target,
        bytes_total: total,
    })
}

fn texts(hits: &[HistoryHit]) -> Vec<&str> {
    hits.iter().map(|hit| hit.text.as_str()).collect()
}

#[test]
fn a_typed_command_is_recorded_per_target_and_repeats_only_bump_the_count() {
    let mut store = HistoryStore::new();
    assert_eq!(
        store.record("t1", "ls -la", Provenance::Typed),
        Recorded::New
    );
    assert_eq!(
        store.record("t1", "ls -la", Provenance::Typed),
        Recorded::Bumped
    );
    assert_eq!(
        store.record("t2", "ls -la", Provenance::Typed),
        Recorded::New,
        "另一个目标有自己的一份"
    );
    let hits = store.matches("t1", "ls");
    assert_eq!(texts(&hits), vec!["ls -la"]);
    assert_eq!(hits[0].uses, 2);
    assert_eq!(store.matches("t2", "ls")[0].uses, 1);
    assert_eq!(store.stats().targets, 2);
    assert_eq!(store.stats().entries, 2);
}

#[test]
fn history_never_crosses_targets() {
    let mut store = HistoryStore::new();
    store.record("t1", "systemctl status nginx", Provenance::Typed);
    assert!(store.matches("t2", "sys").is_empty());
    assert!(store.matches("", "sys").is_empty());
    assert_eq!(store.matches("t1", "sys").len(), 1);
}

#[test]
fn matching_is_a_strict_case_sensitive_prefix() {
    let mut store = HistoryStore::new();
    for cmd in ["git status", "git stash", "gitk", "Git status", "git"] {
        store.record("t", cmd, Provenance::Typed);
    }
    let hits = store.matches("t", "git s");
    let mut got = texts(&hits);
    got.sort_unstable();
    assert_eq!(got, vec!["git stash", "git status"]);
    assert!(
        store.matches("t", "git status").is_empty(),
        "和输入完全相同的不是候选"
    );
    assert!(store.matches("t", "status").is_empty(), "不是前缀就不匹配");
    assert!(store.matches("t", "GIT").is_empty(), "区分大小写");
}

#[test]
fn leading_space_empty_control_and_oversized_commands_are_never_recorded() {
    let mut store = HistoryStore::new();
    assert_eq!(
        store.record("t", " ls", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::LeadingSpace)
    );
    assert_eq!(
        store.record("t", "\tls", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::LeadingSpace)
    );
    assert_eq!(
        store.record("t", "   ", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Empty)
    );
    assert_eq!(
        store.record("t", "", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Empty)
    );
    assert_eq!(
        store.record("t", "echo a\necho b", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Control)
    );
    assert_eq!(
        store.record("t", "echo \u{1b}[31m", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Control)
    );
    assert_eq!(
        store.record("t", &"a".repeat(MAX_TRACKED_BYTES + 1), Provenance::Typed),
        Recorded::Rejected(RejectedBecause::TooLong)
    );
    assert_eq!(store.stats(), Stats::default());
}

#[test]
fn explicitly_marked_and_password_phase_commands_are_not_recorded() {
    let mut store = HistoryStore::new();
    assert_eq!(
        store.record("t", "deploy prod", Provenance::MarkedSensitive),
        Recorded::Rejected(RejectedBecause::MarkedSensitive)
    );
    assert_eq!(
        store.record("t", "hunter2", Provenance::PasswordPhase),
        Recorded::Rejected(RejectedBecause::PasswordPhase)
    );
    assert_eq!(store.stats().entries, 0);
}

#[test]
fn commands_with_secrets_are_refused_at_collection_and_ordinary_ones_pass() {
    use crate::terminal_assist::sensitive::SensitiveReason;
    let mut store = HistoryStore::new();
    assert_eq!(
        store.record("t", "mysql -u root -pHunter2", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Sensitive(SensitiveReason::SecretFlag))
    );
    assert_eq!(
        store.record("t", "export AWS_SECRET_ACCESS_KEY=abc", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Sensitive(SensitiveReason::SecretEnv))
    );
    assert_eq!(
        store.record("t", "git clone https://u:p@h/r.git", Provenance::Typed),
        Recorded::Rejected(RejectedBecause::Sensitive(SensitiveReason::CredentialUrl))
    );
    assert_eq!(store.stats().entries, 0);
    assert_eq!(
        store.record("t", "ssh -p 2222 deploy@build-box", Provenance::Typed),
        Recorded::New
    );
}

#[test]
fn a_target_keeps_at_most_200_entries_and_drops_the_least_recently_used_first() {
    let mut store = HistoryStore::new();
    for i in 0..MAX_ENTRIES_PER_TARGET {
        store.record("t", &format!("cmd-{i:03}"), Provenance::Typed);
    }
    assert_eq!(store.target_stats("t").entries, 200);
    // 再用一次最老的那条：它变成最近使用的，下一条该淘汰的是 cmd-001。
    assert_eq!(
        store.record("t", "cmd-000", Provenance::Typed),
        Recorded::Bumped
    );
    assert_eq!(
        store.record("t", "cmd-new", Provenance::Typed),
        Recorded::New
    );
    assert_eq!(store.target_stats("t").entries, 200);
    let survivors = store.matches("t", "cmd-00");
    assert!(
        !survivors.iter().any(|hit| hit.text == "cmd-001"),
        "最久没用的被回收"
    );
    assert!(survivors.iter().any(|hit| hit.text == "cmd-002"));
    assert!(
        survivors.iter().any(|hit| hit.text == "cmd-000"),
        "刚被用过的最老那条还在"
    );
    assert_eq!(survivors.len(), 9, "000、002..009");
    assert_eq!(store.matches("t", "cmd-ne").len(), 1);
}

#[test]
fn a_target_keeps_at_most_1_mib_counting_the_per_entry_overhead() {
    let mut store = HistoryStore::new();
    // 条目接近 tracker 上限：200 条已经逼近 1 MiB。
    let body = "x".repeat(MAX_TRACKED_BYTES - 16);
    for i in 0..MAX_ENTRIES_PER_TARGET {
        store.record("t", &format!("{i:04}{body}"), Provenance::Typed);
    }
    let stats = store.target_stats("t");
    assert!(stats.bytes <= MAX_BYTES_PER_TARGET, "{} 字节", stats.bytes);
    assert!(stats.entries <= MAX_ENTRIES_PER_TARGET);
    // 缩小预算后字节上限本身也生效：条数还没到，字节先到。
    let mut tight = small(200, 10_000, usize::MAX);
    for i in 0..50 {
        tight.record(
            "t",
            &format!("{i:02}{}", "y".repeat(1000)),
            Provenance::Typed,
        );
    }
    let stats = tight.target_stats("t");
    assert!(stats.bytes <= 10_000);
    assert_eq!(stats.entries, 10_000 / (1002 + ENTRY_OVERHEAD));
    assert!(
        !tight.matches("t", "49").is_empty(),
        "最新的一条一定还在，被回收的是最旧的"
    );
    assert!(tight.matches("t", "00").is_empty());
}

#[test]
fn the_process_budget_reclaims_from_the_least_recently_accessed_target_first() {
    let mut store = small(200, usize::MAX, 4 * (501 + ENTRY_OVERHEAD));
    let body = "z".repeat(500);
    store.record("old", &format!("a{body}"), Provenance::Typed);
    store.record("old", &format!("b{body}"), Provenance::Typed);
    store.record("mid", &format!("c{body}"), Provenance::Typed);
    store.record("new", &format!("d{body}"), Provenance::Typed);
    assert_eq!(store.stats().entries, 4);
    // 读一下 old：它不再是最久没访问的，mid 变成第一个被回收的。
    assert_eq!(store.matches("old", "a").len(), 1);
    store.record("new", &format!("e{body}"), Provenance::Typed);
    assert_eq!(store.stats().entries, 4);
    assert_eq!(
        store.target_stats("mid").entries,
        0,
        "最久没被访问的目标先被整个回收"
    );
    assert_eq!(store.target_stats("old").entries, 2);
    assert_eq!(store.target_stats("new").entries, 2);
    // 再来一条：这回最久没访问的是 old，里面最旧的条目 a 先走。
    store.record("new", &format!("f{body}"), Provenance::Typed);
    assert_eq!(store.target_stats("old").entries, 1);
    assert!(store.matches("old", "a").is_empty());
    assert_eq!(store.matches("old", "b").len(), 1);
    assert!(store.stats().bytes <= store.limits().bytes_total);
}

#[test]
fn the_real_budgets_are_200_entries_1_mib_and_8_mib() {
    let limits = Limits::default();
    assert_eq!(limits.entries_per_target, 200);
    assert_eq!(limits.bytes_per_target, 1024 * 1024);
    assert_eq!(limits.bytes_total, 8 * 1024 * 1024);
    // 十个目标都写满：总量不会超过 8 MiB，最旧访问的目标被回收，刚写入的目标不会。
    let mut store = HistoryStore::new();
    let body = "q".repeat(MAX_TRACKED_BYTES - 8);
    for target in 0..12 {
        for i in 0..MAX_ENTRIES_PER_TARGET {
            store.record(
                &format!("t{target}"),
                &format!("{i:03}{body}"),
                Provenance::Typed,
            );
        }
    }
    let stats = store.stats();
    assert!(stats.bytes <= MAX_BYTES_TOTAL, "{} 字节", stats.bytes);
    assert!(
        store.target_stats("t9").entries > 0,
        "刚写入的目标不会被先回收"
    );
    assert!(
        store.target_stats("t0").entries < MAX_ENTRIES_PER_TARGET,
        "最旧访问的目标先被回收"
    );
}

#[test]
fn clearing_one_target_or_everything_frees_the_budget() {
    let mut store = HistoryStore::new();
    store.record("a", "ls", Provenance::Typed);
    store.record("a", "pwd", Provenance::Typed);
    store.record("b", "ls", Provenance::Typed);
    assert_eq!(store.clear_target("a"), 2);
    assert_eq!(store.clear_target("a"), 0);
    assert!(store.matches("a", "l").is_empty());
    assert_eq!(store.stats().entries, 1);
    assert_eq!(store.clear_all(), 1);
    assert_eq!(store.stats(), Stats::default());
}

#[test]
fn debug_output_never_contains_a_command_or_a_target_name() {
    let mut store = HistoryStore::new();
    store.record(
        "deploy@build-box:22",
        "rsync -a ./dist build-box:/srv",
        Provenance::Typed,
    );
    let hits = store.matches("deploy@build-box:22", "rsync");
    let printed = format!("{store:?} {hits:?}");
    assert!(!printed.contains("rsync"), "{printed}");
    assert!(!printed.contains("build-box"), "{printed}");
    assert!(!printed.contains("deploy"), "{printed}");
}

#[test]
fn the_shared_store_is_one_per_process() {
    let a = shared();
    let b = shared();
    assert!(Arc::ptr_eq(&a, &b));
}

/// 清除（某个目标或全部）都让清除修订前进；记录新命令不动它。界面靠它认出旧检索结果作废（复查 R2）。
#[test]
fn every_clear_advances_the_clear_revision_and_recording_does_not() {
    let mut store = HistoryStore::new();
    assert_eq!(store.clear_revision(), 0);
    store.record("t1", "ls -la", Provenance::Typed);
    assert_eq!(store.clear_revision(), 0, "记录不算清除");
    store.clear_target("t1");
    assert_eq!(store.clear_revision(), 1);
    store.clear_target("nobody");
    assert_eq!(store.clear_revision(), 2, "清一个空目标也前进（保守）");
    store.clear_all();
    assert_eq!(store.clear_revision(), 3);
}

// ---- 历史范围：按主机 / 所有主机共用 ----

#[test]
fn the_bucket_key_is_the_target_per_host_and_one_fixed_key_when_global() {
    for target in ["host:7", "host:99", "ssh://web-1@192.0.2.10:22"] {
        assert_eq!(bucket_key(target, HistoryScope::Host), Some(target));
        assert_eq!(
            bucket_key(target, HistoryScope::Global),
            Some(GLOBAL_BUCKET)
        );
    }
    // 共用桶键对所有目标都是同一个。
    assert_eq!(
        bucket_key("host:1", HistoryScope::Global),
        bucket_key("ssh://a@b:1", HistoryScope::Global)
    );
}

#[test]
fn an_empty_target_is_never_recorded_or_queried_in_either_scope() {
    assert_eq!(bucket_key("", HistoryScope::Host), None);
    assert_eq!(bucket_key("", HistoryScope::Global), None);
}

#[test]
fn the_global_bucket_cannot_collide_with_any_real_target_form() {
    // 真实目标只有 `host:<编号>`、`ssh://<用户>@<地址>:<端口>` 两种形式（空串不进桶）。
    assert!(!GLOBAL_BUCKET.is_empty());
    assert!(!GLOBAL_BUCKET.starts_with(HOST_TARGET_PREFIX));
    assert!(!GLOBAL_BUCKET.starts_with(QUICK_TARGET_PREFIX));
    // 即使主机名 / 用户名恰好写成共用桶键的样子，拼出来的目标仍以固定前缀开头，不会等于它。
    for hostile in [GLOBAL_BUCKET, "global:", "*", ""] {
        let quick = format!("{QUICK_TARGET_PREFIX}{hostile}@{hostile}:22");
        let host = format!("{HOST_TARGET_PREFIX}{hostile}");
        assert_ne!(quick, GLOBAL_BUCKET);
        assert_ne!(host, GLOBAL_BUCKET);
    }
}

#[test]
fn the_scope_setting_reads_global_and_treats_everything_else_as_per_host() {
    assert_eq!(HistoryScope::default(), HistoryScope::Host);
    assert_eq!(HistoryScope::parse("global"), HistoryScope::Global);
    assert_eq!(HistoryScope::parse(" global "), HistoryScope::Global);
    assert_eq!(HistoryScope::parse("host"), HistoryScope::Host);
    for garbage in ["", "Global", "all", "1", "true", "\u{0}"] {
        assert_eq!(
            HistoryScope::parse(garbage),
            HistoryScope::Host,
            "{garbage}"
        );
    }
    for scope in [HistoryScope::Host, HistoryScope::Global] {
        assert_eq!(HistoryScope::parse(scope.id()), scope);
    }
}

#[test]
fn the_two_scopes_keep_separate_buckets_and_global_follows_the_same_limits() {
    let mut store = small(2, 1 << 20, 1 << 20);
    let host = bucket_key("host:1", HistoryScope::Host).unwrap();
    let global = bucket_key("host:1", HistoryScope::Global).unwrap();
    store.record(host, "ls /srv", Provenance::Typed);
    store.record(global, "uptime", Provenance::Typed);
    assert_eq!(texts(&store.matches(host, "")), vec!["ls /srv"]);
    assert_eq!(texts(&store.matches(global, "")), vec!["uptime"]);
    // 共用桶的条数上限和别的桶一样（这里测试里调成 2 条）。
    store.record(global, "df -h", Provenance::Typed);
    store.record(global, "free -m", Provenance::Typed);
    assert_eq!(store.target_stats(global).entries, 2);
    assert_eq!(
        store.target_stats(host).entries,
        1,
        "共用桶的回收不碰按主机的桶"
    );
    // 清共用桶不影响按主机的历史，反之亦然。
    assert_eq!(store.clear_target(global), 2);
    assert_eq!(texts(&store.matches(host, "")), vec!["ls /srv"]);
}
