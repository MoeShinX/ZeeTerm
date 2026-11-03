use std::cell::RefCell;
use std::time::{Duration, Instant};

use super::*;
use crate::sync::settings::{is_synced, sync_mode, SyncMode};

/// 测试用的小参数：省内存省时间；正式参数单独有一条用例。
const CHEAP: KdfCost = KdfCost {
    memory_kib: 8 * 1024,
    iterations: 1,
    parallelism: 1,
};

fn open_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::Builder::new()
        .prefix("zssh-app-lock-")
        .tempdir()
        .expect("临时目录");
    let db = Database::open(&dir.path().join("zssh.db")).expect("打开库");
    (dir, db)
}

// ---- 密码规则 ----

#[test]
fn a_new_password_needs_four_characters_and_two_matching_entries() {
    assert_eq!(
        check_new_password("abc", "abc"),
        Err(PasswordProblem::TooShort)
    );
    assert_eq!(check_new_password("", ""), Err(PasswordProblem::TooShort));
    assert_eq!(check_new_password("abcd", "abcd"), Ok(()));
    assert_eq!(
        check_new_password("abcd", "abce"),
        Err(PasswordProblem::Mismatch)
    );
    // 长度够但两次不一致，报不一致；太短的先报太短（一次只说一个原因）。
    assert_eq!(
        check_new_password("ab", "xy"),
        Err(PasswordProblem::TooShort)
    );
}

#[test]
fn password_length_counts_characters_not_bytes_and_spaces_are_kept() {
    // 4 个汉字 = 12 字节，按字符数算够长。
    assert_eq!(check_new_password("口令口令", "口令口令"), Ok(()));
    assert_eq!(
        check_new_password("口令口", "口令口"),
        Err(PasswordProblem::TooShort)
    );
    // 空格是密码的一部分：不去首尾空白，所以 4 个空格是合法密码，末尾多一个空格就和原来的不一致。
    assert_eq!(check_new_password("    ", "    "), Ok(()));
    assert_eq!(
        check_new_password("pass", "pass "),
        Err(PasswordProblem::Mismatch)
    );
    let longest = "x".repeat(MAX_PASSWORD_CHARS);
    assert_eq!(check_new_password(&longest, &longest), Ok(()));
    let too_long = "x".repeat(MAX_PASSWORD_CHARS + 1);
    assert_eq!(
        check_new_password(&too_long, &too_long),
        Err(PasswordProblem::TooLong)
    );
}

#[test]
fn every_problem_has_a_readable_message() {
    for problem in [
        PasswordProblem::TooShort,
        PasswordProblem::TooLong,
        PasswordProblem::Mismatch,
    ] {
        assert!(!problem.message().is_empty());
        assert_eq!(problem.to_string(), problem.message());
    }
    assert!(PasswordProblem::TooShort.message().contains('4'));
}

#[test]
fn idle_minutes_outside_the_choices_mean_never() {
    for choice in IDLE_CHOICES {
        assert_eq!(sanitize_idle_minutes(choice), choice);
    }
    assert_eq!(IDLE_CHOICES, [0, 5, 10, 15, 30, 60]);
    for odd in [1, 7, 61, 1000, u32::MAX] {
        assert_eq!(sanitize_idle_minutes(odd), 0, "{odd}");
    }
}

// ---- 哈希 ----

#[test]
fn the_right_password_verifies_and_a_wrong_one_does_not() {
    let record = PasswordRecord::create_with("correct horse", CHEAP).unwrap();
    assert!(record.verify("correct horse"));
    assert!(!record.verify("correct horsf"));
    assert!(!record.verify(""));
    assert!(!record.verify("correct horse "), "多一个空格就是另一个密码");
}

#[test]
fn the_record_never_holds_the_password_and_uses_a_fresh_salt_each_time() {
    let first = PasswordRecord::create_with("s3cret-pass", CHEAP).unwrap();
    let second = PasswordRecord::create_with("s3cret-pass", CHEAP).unwrap();
    assert_ne!(first.salt, second.salt, "盐必须每次随机");
    assert_ne!(first.hash, second.hash, "同一个密码两次的哈希不能相同");
    let json = first.to_json().unwrap();
    assert!(!json.contains("s3cret-pass"));
    assert!(json.contains("argon2id"));
    // Debug 输出（日志里可能打到）里也没有密码、盐、哈希。
    let debug = format!("{first:?}");
    assert!(!debug.contains("s3cret-pass"));
    assert!(!debug.contains(&first.hash));
    assert!(!debug.contains(&first.salt));
}

#[test]
fn the_standard_cost_is_not_weaker_than_the_offline_backup() {
    // 卡里要求“参数和离线备份一致或更强”：直接和同步包的默认值比。
    let backup = crate::sync::crypto::KdfParams::default();
    assert!(KdfCost::STANDARD.memory_kib >= backup.memory_kib);
    assert!(KdfCost::STANDARD.iterations >= backup.iterations);
    assert!(KdfCost::STANDARD.parallelism >= backup.parallelism);
    // 并且用正式参数真的走一遍。
    let record = PasswordRecord::create("正式参数的密码").unwrap();
    assert_eq!(record.cost, KdfCost::STANDARD);
    assert!(record.verify("正式参数的密码"));
    assert!(!record.verify("正式参数的密码!"));
}

#[test]
fn a_record_round_trips_through_json_and_still_verifies() {
    let record = PasswordRecord::create_with("round-trip", CHEAP).unwrap();
    let again = PasswordRecord::from_json(&record.to_json().unwrap()).expect("读回");
    assert_eq!(again, record);
    assert!(again.verify("round-trip"));
    assert_eq!(PasswordRecord::from_json("not json"), None);
    assert_eq!(PasswordRecord::from_json("{}"), None);
}

#[test]
fn a_damaged_record_never_verifies_and_never_panics() {
    let good = PasswordRecord::create_with("abcd1234", CHEAP).unwrap();

    let mut bad = good.clone();
    bad.version = 99;
    assert!(!bad.verify("abcd1234"), "不认识的版本");

    let mut bad = good.clone();
    bad.algorithm = "argon2i".to_owned();
    assert!(!bad.verify("abcd1234"));

    let mut bad = good.clone();
    bad.salt = "***not base64***".to_owned();
    assert!(!bad.verify("abcd1234"));

    let mut bad = good.clone();
    bad.hash = b64(&[0u8; 8]);
    assert!(!bad.verify("abcd1234"), "哈希长度不对");

    // 参数被改成天文数字：不能因此去分配几十 GB，直接判不通过。
    let mut bad = good.clone();
    bad.cost.memory_kib = u32::MAX;
    assert!(!bad.verify("abcd1234"));
    let mut bad = good.clone();
    bad.cost.iterations = 0;
    assert!(!bad.verify("abcd1234"));
    let mut bad = good;
    bad.cost.parallelism = 1000;
    assert!(!bad.verify("abcd1234"));

    assert!(
        PasswordRecord::create_with(
            "abcd",
            KdfCost {
                memory_kib: u32::MAX,
                iterations: 1,
                parallelism: 1
            }
        )
        .is_err(),
        "创建时同样拒绝越界参数"
    );
}

#[test]
fn the_constant_time_compare_agrees_with_plain_equality() {
    assert!(equal_constant_time(b"", b""));
    assert!(equal_constant_time(b"abc", b"abc"));
    assert!(!equal_constant_time(b"abc", b"abd"));
    assert!(!equal_constant_time(b"abc", b"ab"));
    assert!(!equal_constant_time(b"abc", b"abcd"));
}

// ---- 设置的读写 ----

#[test]
fn nothing_is_enabled_by_default() {
    let (_dir, db) = open_db();
    let config = LockConfig::load(&db);
    assert_eq!(config, LockConfig::default());
    assert_eq!(config.status, LockStatus::Disabled);
    assert!(!config.status.is_enabled());
    assert_eq!(config.idle_minutes, 0, "默认不自动锁定");
    assert!(!config.lock_on_tray, "默认关到托盘时不锁");
}

#[test]
fn enabling_changing_and_disabling_go_through_the_database() {
    let (_dir, db) = open_db();
    let record = PasswordRecord::create_with("first-pass", CHEAP).unwrap();
    save_password(&db, &record).unwrap();
    match LockConfig::load(&db).status {
        LockStatus::Enabled(loaded) => {
            assert!(loaded.verify("first-pass"));
            assert!(!loaded.verify("other"));
        }
        other => panic!("应该已启用：{other:?}"),
    }

    // 改密码：新记录覆盖旧的，旧密码不再有效。
    let changed = PasswordRecord::create_with("second-pass", CHEAP).unwrap();
    save_password(&db, &changed).unwrap();
    let LockStatus::Enabled(loaded) = LockConfig::load(&db).status else {
        panic!("改完仍是启用");
    };
    assert!(loaded.verify("second-pass"));
    assert!(!loaded.verify("first-pass"));

    // 关闭：记录清掉，回到没启用。
    clear_password(&db).unwrap();
    assert_eq!(LockConfig::load(&db).status, LockStatus::Disabled);
}

#[test]
fn a_record_that_cannot_be_read_counts_as_enabled_not_as_off() {
    let (_dir, db) = open_db();
    db.setting_set(HASH_KEY, "{\"v\":1,\"junk\":true}").unwrap();
    let config = LockConfig::load(&db);
    assert_eq!(config.status, LockStatus::Unreadable);
    assert!(
        config.status.is_enabled(),
        "读不懂的记录不能悄悄把锁关掉，那等于绕过密码"
    );
}

#[test]
fn idle_and_tray_settings_round_trip_and_clamp() {
    let (_dir, db) = open_db();
    save_idle_minutes(&db, 15).unwrap();
    save_lock_on_tray(&db, true).unwrap();
    let config = LockConfig::load(&db);
    assert_eq!(config.idle_minutes, 15);
    assert!(config.lock_on_tray);

    // 不在几档里的值存的时候就收回到 0；库里被改成怪值读出来也是 0。
    save_idle_minutes(&db, 7).unwrap();
    assert_eq!(LockConfig::load(&db).idle_minutes, 0);
    db.setting_set(IDLE_MINUTES_KEY, "\"soon\"").unwrap();
    assert_eq!(LockConfig::load(&db).idle_minutes, 0);
    db.setting_set(IDLE_MINUTES_KEY, "30").unwrap();
    assert_eq!(LockConfig::load(&db).idle_minutes, 30);
    db.setting_set(LOCK_ON_TRAY_KEY, "\"yes\"").unwrap();
    assert!(!LockConfig::load(&db).lock_on_tray);
}

#[test]
fn every_lock_setting_stays_on_this_machine() {
    for key in SETTING_KEYS {
        assert_eq!(
            sync_mode(key),
            Some(SyncMode::LocalOnly),
            "{key} 要在同步白名单里登记为本机键"
        );
        assert!(!is_synced(key), "{key} 不能进同步包 / 离线备份");
    }
    assert_eq!(SETTING_KEYS, [HASH_KEY, IDLE_MINUTES_KEY, LOCK_ON_TRAY_KEY]);
}

#[test]
fn the_password_record_is_left_out_of_the_sync_snapshot() {
    // 不只是白名单里这么写：真的把设置打进同步快照，看密码记录有没有跟着出去。
    let (_dir, db) = open_db();
    save_password(
        &db,
        &PasswordRecord::create_with("abcd1234", CHEAP).unwrap(),
    )
    .unwrap();
    save_idle_minutes(&db, 10).unwrap();
    save_lock_on_tray(&db, true).unwrap();
    // 再放一个确实会同步的设置做对照，证明快照确实读了设置表。
    db.setting_set("terminal_font_size", "14").unwrap();
    let snapshot = crate::sync::snapshot::snapshot_local(
        &db,
        &crate::sync::sealer::TestSealer(1),
        "dev-a",
        None,
        1,
    )
    .expect("打快照");
    let text = serde_json::to_string(&snapshot).expect("序列化");
    assert!(text.contains("terminal_font_size"), "对照项应该在快照里");
    for key in SETTING_KEYS {
        assert!(!text.contains(key), "{key} 不应该出现在同步快照里");
    }
}

// ---- 输错后的退避 ----

#[test]
fn the_wait_starts_at_the_fifth_wrong_try_and_doubles_up_to_a_minute() {
    let secs = |failures| delay_after(failures).as_secs();
    for failures in 0..FREE_ATTEMPTS {
        assert_eq!(
            secs(failures),
            0,
            "前 {FREE_ATTEMPTS} 次以内（含第 4 次）不等"
        );
    }
    assert_eq!(secs(5), 5);
    assert_eq!(secs(6), 10);
    assert_eq!(secs(7), 20);
    assert_eq!(secs(8), 40);
    assert_eq!(secs(9), 60, "80 秒被上限收到 60");
    assert_eq!(secs(10), 60);
    assert_eq!(secs(1000), 60);
    assert_eq!(secs(u32::MAX), 60);
}

#[test]
fn the_backoff_blocks_attempts_until_the_wait_is_over() {
    let start = Instant::now();
    let mut backoff = Backoff::new();
    for _ in 0..4 {
        assert_eq!(backoff.record_failure(start), Duration::ZERO);
        assert_eq!(backoff.remaining(start), None, "前 4 次可以马上再试");
    }
    assert_eq!(backoff.failures(), 4);

    // 第 5 次：等 5 秒。
    assert_eq!(backoff.record_failure(start), Duration::from_secs(5));
    assert_eq!(backoff.remaining(start), Some(Duration::from_secs(5)));
    assert_eq!(
        backoff.remaining(start + Duration::from_secs(3)),
        Some(Duration::from_secs(2))
    );
    assert_eq!(backoff.remaining(start + Duration::from_secs(5)), None);
    assert_eq!(backoff.remaining(start + Duration::from_secs(99)), None);

    // 等完又输错：10 秒。
    let later = start + Duration::from_secs(6);
    assert_eq!(backoff.record_failure(later), Duration::from_secs(10));
    assert_eq!(backoff.remaining(later), Some(Duration::from_secs(10)));
}

#[test]
fn a_correct_password_clears_the_count() {
    let now = Instant::now();
    let mut backoff = Backoff::new();
    for _ in 0..6 {
        backoff.record_failure(now);
    }
    assert!(backoff.remaining(now).is_some());
    backoff.record_success();
    assert_eq!(backoff.failures(), 0);
    assert_eq!(backoff.remaining(now), None);
    // 清零之后重新数：又要错满 5 次才开始等。
    for _ in 0..4 {
        backoff.record_failure(now);
    }
    assert_eq!(backoff.remaining(now), None);
}

// ---- 忘记密码：Windows 验证的三种结果 ----

/// 桩：按预设返回，并记下两条路各被调了几次。
struct Stub {
    hello: OsCheck,
    credentials: OsCheck,
    calls: RefCell<(u32, u32)>,
}

impl Stub {
    fn new(hello: OsCheck, credentials: OsCheck) -> Self {
        Self {
            hello,
            credentials,
            calls: RefCell::new((0, 0)),
        }
    }
}

impl OsVerifier for Stub {
    fn windows_hello(&self, _message: &str) -> OsCheck {
        self.calls.borrow_mut().0 += 1;
        self.hello
    }

    fn windows_credentials(&self, _message: &str) -> OsCheck {
        self.calls.borrow_mut().1 += 1;
        self.credentials
    }
}

#[test]
fn windows_hello_passing_resets_without_asking_for_the_password() {
    let stub = Stub::new(OsCheck::Verified, OsCheck::Failed);
    assert_eq!(verify_os_user(&stub, "验证"), OsCheck::Verified);
    assert_eq!(*stub.calls.borrow(), (1, 0), "Hello 过了就不再弹凭据框");
}

#[test]
fn failing_or_cancelling_hello_stops_there() {
    for decided in [OsCheck::Failed, OsCheck::Cancelled] {
        let stub = Stub::new(decided, OsCheck::Verified);
        assert_eq!(verify_os_user(&stub, "验证"), decided);
        assert_eq!(
            *stub.calls.borrow(),
            (1, 0),
            "用户已经表态（没通过 / 取消）就不能换一条路再问一遍，更不能因此放行"
        );
    }
}

#[test]
fn without_windows_hello_the_credential_box_decides() {
    for (credentials, expected) in [
        (OsCheck::Verified, OsCheck::Verified),
        (OsCheck::Failed, OsCheck::Failed),
        (OsCheck::Cancelled, OsCheck::Cancelled),
        (OsCheck::Unavailable, OsCheck::Unavailable),
    ] {
        let stub = Stub::new(OsCheck::Unavailable, credentials);
        assert_eq!(verify_os_user(&stub, "验证"), expected);
        assert_eq!(*stub.calls.borrow(), (1, 1));
    }
}

#[test]
fn when_neither_way_works_the_answer_is_unavailable_not_verified() {
    let stub = Stub::new(OsCheck::Unavailable, OsCheck::Unavailable);
    assert_eq!(verify_os_user(&stub, "验证"), OsCheck::Unavailable);
}
