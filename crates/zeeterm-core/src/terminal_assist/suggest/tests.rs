use super::*;

fn hit(text: &str, uses: u32, last_used: u64) -> HistoryHit {
    HistoryHit {
        text: text.to_owned(),
        uses,
        last_used,
    }
}

fn quick(text: &str) -> QuickSource {
    QuickSource {
        text: text.to_owned(),
        excluded: false,
    }
}

fn texts(list: &[Candidate]) -> Vec<&str> {
    list.iter().map(|c| c.text.as_str()).collect()
}

#[test]
fn input_needs_at_least_two_characters_and_no_leading_space_or_controls() {
    assert!(!eligible_input(""));
    assert!(!eligible_input("l"));
    assert!(eligible_input("ls"));
    assert!(eligible_input("中文"), "按字符数而不是字节数");
    assert!(!eligible_input("é"), "一个字符（两个字节）不够");
    assert!(!eligible_input(" l"), "前导空格表示别记，也就不提示");
    assert!(!eligible_input("ls\tx"));
    assert!(!eligible_input(&"a".repeat(MAX_TRACKED_BYTES + 1)));
    assert!(search("l", vec![hit("ls", 1, 1)], &[quick("lsblk")]).is_empty());
}

#[test]
fn only_strict_prefix_extensions_are_candidates() {
    let history = vec![
        hit("git status", 1, 1),
        hit("git", 1, 2),
        hit("Git status", 1, 3),
        hit("digit", 1, 4),
        hit("git stash", 1, 5),
    ];
    let got = search("git s", history, &[quick("git show"), quick("echo git s")]);
    let mut names = texts(&got);
    names.sort_unstable();
    assert_eq!(names, vec!["git show", "git stash", "git status"]);
    assert!(
        search("git status", vec![hit("git status", 1, 1)], &[]).is_empty(),
        "和输入相同的不是候选"
    );
}

#[test]
fn history_is_ranked_by_use_count_then_recency_then_text() {
    let history = vec![
        hit("make clean", 1, 9),
        hit("make test", 5, 1),
        hit("make build", 5, 7),
        hit("make all", 1, 9),
    ];
    let got = search("make ", history, &[]);
    assert_eq!(
        texts(&got),
        vec!["make build", "make test", "make all", "make clean"],
        "次数多的在前；同次数最近的在前；再按文字"
    );
    assert!(got.iter().all(|c| c.source == CandidateSource::History));
}

#[test]
fn quick_commands_follow_history_in_list_order_and_duplicates_collapse() {
    let history = vec![hit("docker ps -a", 3, 1)];
    let quicks = [
        quick("docker images"),
        quick("docker ps -a"),
        quick("docker compose up -d"),
    ];
    let got = search("docker ", history, &quicks);
    assert_eq!(
        texts(&got),
        vec!["docker ps -a", "docker images", "docker compose up -d"]
    );
    assert_eq!(got[0].source, CandidateSource::History, "重复的留历史那条");
    assert_eq!(got[1].source, CandidateSource::QuickCommand);
}

#[test]
fn at_most_eight_candidates() {
    let history: Vec<HistoryHit> = (0..20)
        .map(|i| hit(&format!("echo {i:02}"), 1, i as u64))
        .collect();
    let quicks: Vec<QuickSource> = (0..20).map(|i| quick(&format!("echo q{i:02}"))).collect();
    let got = search("echo ", history, &quicks);
    assert_eq!(got.len(), MAX_CANDIDATES);
    assert_eq!(MAX_CANDIDATES, 8);
}

#[test]
fn sensitive_history_and_quick_commands_never_appear_even_if_stored_or_unmarked() {
    let history = vec![
        hit("mysql -u root -pHunter2", 9, 9),
        hit("mysql -u root", 1, 1),
        hit("export DB_TOKEN=abc", 5, 5),
    ];
    let quicks = [
        quick("mysql -u app --password=pw"),
        QuickSource {
            text: "mysql -u app -h prod".to_owned(),
            excluded: true,
        },
        quick("mysql -h staging"),
        quick("mysql -e 'select 1\nselect 2'"),
    ];
    let got = search("mysql ", history, &quicks);
    assert_eq!(texts(&got), vec!["mysql -u root", "mysql -h staging"]);
    // 排除的快捷命令整条不出现：不是只遮住标签。
    let shown: String = got
        .iter()
        .map(|c| c.text.clone())
        .collect::<Vec<_>>()
        .join("|");
    assert!(!shown.contains("Hunter2"));
    assert!(!shown.contains("prod"));
    assert!(!shown.contains("pw"));
    assert!(!shown.contains("TOKEN"));
}

#[test]
fn nothing_is_preselected_and_the_suffix_is_what_remains_after_the_typed_prefix() {
    let got = search("git s", vec![hit("git status", 1, 1)], &[]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].suffix_after("git s"), Some("tatus"));
    assert_eq!(got[0].suffix_after("git status"), None);
    assert_eq!(got[0].suffix_after("git t"), None);
    assert_eq!(got[0].suffix_after("Git s"), None, "区分大小写");
    let wide = Candidate {
        source: CandidateSource::History,
        text: "echo 你好世界".to_owned(),
    };
    assert_eq!(wide.suffix_after("echo 你"), Some("好世界"));
}

#[test]
fn debug_output_does_not_contain_the_command() {
    let got = search("deploy ", vec![hit("deploy --env prod-east", 1, 1)], &[]);
    let printed = format!("{got:?} {:?}", quick("deploy --env prod-east"));
    assert!(!printed.contains("prod-east"), "{printed}");
}

#[test]
fn collecting_searching_and_filtering_emit_no_log_line_with_a_command() {
    use crate::terminal_assist::history::{HistoryStore, Provenance};
    let secret = "mysql -u root -pHunter2xyz";
    let ordinary = "mysql -u root -h db-east";
    let ((), logs) = crate::test_util::capture_logs(|| {
        let mut store = HistoryStore::new();
        store.record("deploy@build-box:22", secret, Provenance::Typed);
        store.record("deploy@build-box:22", ordinary, Provenance::Typed);
        let hits = store.matches("deploy@build-box:22", "mysql ");
        let got = search("mysql ", hits, &[quick(secret), quick(ordinary)]);
        assert_eq!(texts(&got), vec![ordinary]);
        store.clear_target("deploy@build-box:22");
        store.clear_all();
    });
    for forbidden in ["Hunter2xyz", "db-east", "mysql", "build-box"] {
        assert!(
            !logs.contains(forbidden),
            "日志里不能有命令或目标名: {logs}"
        );
    }
}

#[test]
fn a_quick_command_is_suggestible_only_when_unmarked_and_clean() {
    assert!(quick_command_suggestible("systemctl status nginx", false));
    assert!(!quick_command_suggestible("systemctl status nginx", true));
    assert!(!quick_command_suggestible("curl -u a:b https://x", false));
    assert!(!quick_command_suggestible("echo a\necho b", false));
    assert!(!quick_command_suggestible("   ", false));
}

#[test]
fn the_settle_timer_fires_once_after_the_revision_is_stable_for_80_ms() {
    let t0 = Instant::now();
    let mut timer = SettleTimer::new();
    assert!(!timer.poll(5, t0), "第一次看到只开始计时");
    assert!(!timer.poll(5, t0 + Duration::from_millis(79)));
    assert!(timer.poll(5, t0 + Duration::from_millis(80)));
    assert!(
        !timer.poll(5, t0 + Duration::from_millis(500)),
        "同一修订号只放行一次"
    );
    assert!(timer.has_fired_for(5));
    // 输入又变了：重新计时。
    assert!(!timer.poll(6, t0 + Duration::from_millis(600)));
    assert!(!timer.has_fired_for(6));
    assert!(!timer.poll(6, t0 + Duration::from_millis(679)));
    assert!(timer.poll(6, t0 + Duration::from_millis(680)));
    // 条件不满足后忘掉计时。
    timer.reset();
    assert!(!timer.poll(6, t0 + Duration::from_secs(5)));
    assert!(timer.poll(6, t0 + Duration::from_secs(6)));
}

#[test]
fn a_revision_that_keeps_changing_never_fires() {
    let t0 = Instant::now();
    let mut timer = SettleTimer::new();
    for step in 0..30u64 {
        assert!(
            !timer.poll(step, t0 + Duration::from_millis(step * 20)),
            "每 20 ms 变一次，永远不够 80 ms"
        );
    }
}
