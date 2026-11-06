//! Linux 本机秘密的真实路径测试（Secret Service + 后备密钥文件）。
//!
//! lib 内的单元测试用固定测试密钥，碰不到真正的密钥加载；这里是集成测试，链接的是正式构建的库。
//! `vm-gates.sh` 把测试跑在一次性的 D-Bus 会话 + 临时钥匙串里。
//! 密钥缓存是进程级的，所以每种场景各用一个进程：并发那条在本进程里跑，"没有钥匙串服务"那条由子进程跑。

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Barrier};

use zssh_core::store::secrets::{
    init_vault, key_state, protect, unprotect, warm_up_vault, KeyBackend, KeyState,
};

const CHILD_DIR_VAR: &str = "ZSSH_TEST_VAULT_CHILD_DIR";

/// 钥匙串里有没有已存在的条目（只读探测，不改它）。
fn keyring_entry_exists() -> Option<bool> {
    let entry = keyring::Entry::new("app.zeeterm", "local-vault-key").ok()?;
    match entry.get_password() {
        Ok(_) => Some(true),
        Err(keyring::Error::NoEntry) => Some(false),
        Err(_) => None,
    }
}

/// N 个线程同时第一次加密：只能建一把密钥，之后所有密文都解得开。
/// 在一次性钥匙串里单独跑这个测试时，开头条目是不存在的，这就是"首次创建竞态"的真实场景。
#[test]
fn concurrent_first_use_shares_one_key() {
    let dir = tempfile::tempdir().expect("临时目录");
    init_vault(dir.path());
    let existed_before = keyring_entry_exists();

    const THREADS: usize = 16;
    let barrier = Arc::new(Barrier::new(THREADS));
    let sealed: Vec<(String, Vec<u8>)> = (0..THREADS)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let plaintext = format!("secret-{index}");
                barrier.wait();
                let blob = protect(plaintext.as_bytes()).expect("并发第一次加密");
                (plaintext, blob)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().expect("线程"))
        .collect();

    // 所有线程的密文都用同一把密钥：任何一份都能被任何线程解开。
    for (plaintext, blob) in &sealed {
        let opened = unprotect(blob).expect("同一把密钥必须解得开");
        assert_eq!(opened, plaintext.as_bytes());
    }

    match key_state() {
        KeyState::Ready(KeyBackend::SystemKeyring) => {
            assert!(
                !dir.path().join("vault.key").exists(),
                "钥匙串可用时不该在数据目录建文件"
            );
            if existed_before == Some(false) {
                // 开头没有条目：现在应该恰好有一个（不是 Ambiguous，也不是被覆盖成别的）。
                assert_eq!(keyring_entry_exists(), Some(true));
            }
        }
        KeyState::Ready(KeyBackend::LocalFile) => {
            eprintln!("跳过钥匙串断言：这个环境没有 Secret Service，已走后备文件");
        }
        other => panic!("密钥状态不对：{other:?}"),
    }
}

/// 子进程入口：模拟"没有钥匙串服务"的桌面（没有 D-Bus 会话），验证后备密钥文件路径。
/// 平时 `#[ignore]`，由下面的测试带环境变量拉起。
#[test]
#[ignore = "helper for without_a_secret_service_the_key_goes_to_a_0600_file"]
fn child_without_secret_service() {
    let Some(dir) = std::env::var_os(CHILD_DIR_VAR) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    init_vault(&dir);
    assert_eq!(key_state(), KeyState::Loading, "还没用过，不该已有状态");

    let blob = protect("没有钥匙串也能保存的密码".as_bytes()).expect("应降级到后备文件");
    assert_eq!(
        unprotect(&blob).expect("解密"),
        "没有钥匙串也能保存的密码".as_bytes()
    );
    assert_eq!(key_state(), KeyState::Ready(KeyBackend::LocalFile));

    let key_file = dir.join("vault.key");
    assert_eq!(std::fs::read(&key_file).expect("密钥文件").len(), 32);
    let mode = std::fs::metadata(&key_file)
        .expect("元数据")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // 预热在已有缓存时是空操作，状态不变。
    warm_up_vault();
    assert_eq!(key_state(), KeyState::Ready(KeyBackend::LocalFile));
}

#[test]
fn without_a_secret_service_the_key_goes_to_a_0600_file() {
    let data = tempfile::tempdir().expect("数据目录");
    // 空的运行目录：既没有 D-Bus 会话，zbus 回退去找的 $XDG_RUNTIME_DIR/bus 也不存在。
    let runtime = tempfile::tempdir().expect("运行目录");
    let output = std::process::Command::new(std::env::current_exe().expect("当前可执行文件"))
        .args([
            "--ignored",
            "--exact",
            "child_without_secret_service",
            "--nocapture",
        ])
        .env(CHILD_DIR_VAR, data.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .output()
        .expect("拉起子进程");
    assert!(
        output.status.success(),
        "子进程失败：\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(data.path().join("vault.key").exists());
}
