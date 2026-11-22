//! 测试助手自己的测试：`ZSSH_TEST_SOCKS5` 写错要报错，不能静默改成直连；
//! 代理地址不进测试输出。不联网。

mod common;

use common::{parse_socks5, TestServer};

#[test]
fn socks5_setting_is_validated_and_never_printed() {
    // 合法写法
    assert_eq!(
        parse_socks5("proxy.invalid:1080").unwrap(),
        ("proxy.invalid".to_owned(), 1080)
    );
    assert_eq!(
        parse_socks5(" 127.0.0.1:10808 ").unwrap(),
        ("127.0.0.1".to_owned(), 10808)
    );

    // 写错的几种：没端口、端口不是数字、端口越界、没主机
    for bad in [
        "proxy.invalid",
        "proxy.invalid:abc",
        "proxy.invalid:70000",
        ":1080",
    ] {
        let err = parse_socks5(bad).expect_err("应报错");
        assert!(err.contains("ZSSH_TEST_SOCKS5"), "错误要点名变量：{err}");
        assert!(!err.contains("proxy.invalid"), "错误不能回显配置值：{err}");
    }

    // 整条 from_env 链路：环境变量写错 → Err（而不是 None 后直连）。
    // 本文件只有这一个测试函数，改进程环境变量不会和别的测试抢。
    std::env::set_var("ZSSH_TEST_HOST", "server.invalid");
    std::env::set_var("ZSSH_TEST_USER", "tester");
    std::env::set_var("ZSSH_TEST_PASSWORD", "not-a-real-password");
    std::env::set_var("ZSSH_TEST_PORT", "22");
    std::env::set_var("ZSSH_TEST_SOCKS5", "proxy.invalid");
    let err = TestServer::from_env()
        .err()
        .expect("写错的代理设置必须让 from_env 失败");
    assert!(err.contains("ZSSH_TEST_SOCKS5"));

    // 写对了：错误文本里的代理地址被换掉，服务器地址也一样。
    std::env::set_var("ZSSH_TEST_SOCKS5", "proxy.invalid:1080");
    let server = TestServer::from_env().expect("合法设置");
    let text = server.sanitize("connect to proxy.invalid:1080 failed, server.invalid refused");
    assert!(!text.contains("proxy.invalid"), "{text}");
    assert!(!text.contains("server.invalid"), "{text}");
    // panic 钩子用的全局替换同样要覆盖代理地址
    let scrubbed = common::scrub("via proxy.invalid:1080 to server.invalid");
    assert!(!scrubbed.contains("proxy.invalid"), "{scrubbed}");
    assert!(!scrubbed.contains("server.invalid"), "{scrubbed}");

    // 不设代理：照旧直连（没有 Err）
    std::env::remove_var("ZSSH_TEST_SOCKS5");
    TestServer::from_env().expect("没设代理");
}
