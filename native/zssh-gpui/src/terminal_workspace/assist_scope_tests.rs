//! 命令历史的范围（按主机分开 / 所有主机共用）：窗格记录、查询、清除当前目标各走哪个桶，
//! 切换范围时两个桶互不影响、旧范围的候选不残留。

// 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`（导致展开时无限递归）。
use super::assist_tests::{Remote, run_line, shell_pane_for};
use super::{Pane, WriteOrigin};
use std::time::{Duration, Instant};
use zssh_core::terminal_assist::history::{self, GLOBAL_BUCKET, HistoryScope};

/// 在提示符上、聚焦、按给定范围的窗格。
fn pane_on(host_id: Option<i64>, scope: HistoryScope) -> (Pane, Remote) {
    let (mut pane, remote) = shell_pane_for(host_id);
    pane.assist.set_history_scope(scope);
    pane.assist.set_suggest_active(true);
    remote.output(&mut pane, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    (pane, remote)
}

/// 某个桶里以 `prefix` 开头的命令（直接查共用的内存历史）。
fn bucket_texts(pane: &Pane, bucket: &str, prefix: &str) -> Vec<String> {
    history::lock(pane.assist.history())
        .matches(bucket, prefix)
        .into_iter()
        .map(|hit| hit.text)
        .collect()
}

fn type_text(pane: &mut Pane, text: &str) {
    for ch in text.chars() {
        pane.write_as(ch.to_string().as_bytes(), WriteOrigin::TypedText);
    }
}

/// 让输入稳定下来（80 ms 稳定计时用假时钟，不真等）。
fn settle(pane: &mut Pane) {
    let origin = Instant::now();
    pane.observe_input_at(origin);
    pane.observe_input_at(origin + Duration::from_millis(10_000));
}

fn shown(pane: &Pane) -> Vec<String> {
    pane.assist
        .suggest_view()
        .map(|view| view.items.iter().map(|item| item.text.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn in_the_shared_scope_a_command_run_on_one_host_is_suggested_on_another() {
    let (mut a, remote_a) = pane_on(Some(1), HistoryScope::Global);
    let (mut b, _remote_b) = pane_on(Some(2), HistoryScope::Global);
    run_line(&mut a, &remote_a, "ls /srv/data");
    assert_eq!(
        bucket_texts(&a, GLOBAL_BUCKET, "ls"),
        vec!["ls /srv/data".to_owned()],
        "记进了共用桶"
    );
    // 另一台主机的窗格（主机 2）敲前缀就能看到。
    type_text(&mut b, "ls /s");
    settle(&mut b);
    assert_eq!(shown(&b), vec!["ls /srv/data".to_owned()]);
}

#[test]
fn per_host_scope_still_keeps_hosts_apart() {
    let (mut a, remote_a) = pane_on(Some(1), HistoryScope::Host);
    let (mut b, _remote_b) = pane_on(Some(2), HistoryScope::Host);
    run_line(&mut a, &remote_a, "ls /srv/data");
    assert_eq!(
        bucket_texts(&a, "host:1", "ls"),
        vec!["ls /srv/data".to_owned()]
    );
    assert!(bucket_texts(&a, GLOBAL_BUCKET, "ls").is_empty());
    type_text(&mut b, "ls /s");
    settle(&mut b);
    assert!(shown(&b).is_empty(), "按主机分开时别的主机的命令不串过来");
}

#[test]
fn switching_the_scope_leaves_both_buckets_alone() {
    let (mut pane, remote) = pane_on(Some(1), HistoryScope::Host);
    run_line(&mut pane, &remote, "ls /host-only");
    // 切到共用：共用桶起初是空的（按主机的历史不会被搬过去）。
    pane.assist.set_history_scope(HistoryScope::Global);
    assert!(bucket_texts(&pane, GLOBAL_BUCKET, "ls").is_empty());
    run_line(&mut pane, &remote, "ls /shared");
    assert_eq!(
        bucket_texts(&pane, GLOBAL_BUCKET, "ls"),
        vec!["ls /shared".to_owned()]
    );
    // 切回按主机：原来的历史还在，共用期间的命令没有混进来。
    pane.assist.set_history_scope(HistoryScope::Host);
    assert_eq!(
        bucket_texts(&pane, "host:1", "ls"),
        vec!["ls /host-only".to_owned()]
    );
    assert_eq!(
        bucket_texts(&pane, GLOBAL_BUCKET, "ls"),
        vec!["ls /shared".to_owned()],
        "共用桶也没丢"
    );
}

#[test]
fn switching_the_scope_closes_the_candidates_of_the_old_scope() {
    let (mut pane, remote) = pane_on(Some(1), HistoryScope::Host);
    run_line(&mut pane, &remote, "git status");
    type_text(&mut pane, "gi");
    settle(&mut pane);
    assert_eq!(shown(&pane), vec!["git status".to_owned()]);
    pane.assist.set_history_scope(HistoryScope::Global);
    assert!(shown(&pane).is_empty(), "旧范围的结果不能残留在浮层上");
    // 同一个输入按新范围重新检索：共用桶里没有它，仍然没有候选。
    settle(&mut pane);
    assert!(shown(&pane).is_empty());
    // 再切回来，同一个输入重新检索，原来的候选回来了。
    pane.assist.set_history_scope(HistoryScope::Host);
    settle(&mut pane);
    assert_eq!(shown(&pane), vec!["git status".to_owned()]);
}

#[test]
fn a_search_still_in_flight_when_the_scope_changes_is_discarded() {
    let (mut pane, remote) = pane_on(Some(1), HistoryScope::Host);
    run_line(&mut pane, &remote, "git status");
    type_text(&mut pane, "gi");
    pane.assist.defer_search_jobs();
    let origin = Instant::now();
    pane.observe_input_at(origin);
    pane.observe_input_at(origin + Duration::from_millis(10_000));
    assert!(
        pane.assist.search_in_flight(),
        "检索已经发出去，结果还没回来"
    );
    pane.assist.set_history_scope(HistoryScope::Global);
    assert!(!pane.assist.search_in_flight(), "换范围就取消在途的检索");
    pane.assist.run_deferred_jobs();
    pane.observe_input_at(origin + Duration::from_millis(20_000));
    assert!(shown(&pane).is_empty(), "迟到的旧结果不能展示");
}

#[test]
fn clearing_the_current_target_clears_the_bucket_of_the_current_scope() {
    let (mut pane, remote) = pane_on(Some(1), HistoryScope::Host);
    run_line(&mut pane, &remote, "ls /host-only");
    pane.assist.set_history_scope(HistoryScope::Global);
    run_line(&mut pane, &remote, "ls /shared");

    // 共用范围：清的是共用桶，按主机的那份不动。
    assert_eq!(pane.assist.clear_target_history(), 1);
    assert!(bucket_texts(&pane, GLOBAL_BUCKET, "ls").is_empty());
    assert_eq!(
        bucket_texts(&pane, "host:1", "ls"),
        vec!["ls /host-only".to_owned()]
    );

    // 按主机范围：清的是这台主机的桶；共用桶（这里重新记一条）不动。
    run_line(&mut pane, &remote, "ls /shared-again");
    pane.assist.set_history_scope(HistoryScope::Host);
    assert_eq!(pane.assist.clear_target_history(), 1);
    assert!(bucket_texts(&pane, "host:1", "ls").is_empty());
    assert_eq!(
        bucket_texts(&pane, GLOBAL_BUCKET, "ls"),
        vec!["ls /shared-again".to_owned()]
    );
}

#[test]
fn clear_all_empties_both_scopes() {
    let (mut pane, remote) = pane_on(Some(1), HistoryScope::Host);
    run_line(&mut pane, &remote, "ls /host-only");
    pane.assist.set_history_scope(HistoryScope::Global);
    run_line(&mut pane, &remote, "ls /shared");
    assert_eq!(history::lock(pane.assist.history()).clear_all(), 2);
    assert!(bucket_texts(&pane, "host:1", "ls").is_empty());
    assert!(bucket_texts(&pane, GLOBAL_BUCKET, "ls").is_empty());
}

#[test]
fn a_pane_without_a_target_neither_records_nor_clears_in_either_scope() {
    for scope in [HistoryScope::Host, HistoryScope::Global] {
        let (mut pane, remote) = pane_on(None, scope);
        assert_eq!(pane.assist.tracker().context().target, "");
        run_line(&mut pane, &remote, "ls /nowhere");
        let stats = history::lock(pane.assist.history()).stats();
        assert_eq!(stats.entries, 0, "{scope:?}：离线面板不记录");
        assert_eq!(pane.assist.clear_target_history(), 0, "{scope:?}");
    }
}

#[test]
fn the_real_target_forms_never_equal_the_shared_bucket_key() {
    // 窗格实际算出来的目标身份只有 host:<编号>、ssh://…、空串三种（见 `Pane::target_identity`）。
    for host in [1_i64, 7, 99, i64::MAX] {
        let (mut pane, remote) = pane_on(Some(host), HistoryScope::Host);
        run_line(&mut pane, &remote, "ls /x");
        let target = pane.assist.tracker().context().target.clone();
        assert_eq!(target, format!("host:{host}"));
        assert_ne!(target, GLOBAL_BUCKET);
    }
}
