//! 更新链路串联：manifest -> 平台选择 -> 验签 -> 版本绑定 -> 安装计划。
//!
//! 不下载、不启动安装器；用测试密钥和内存里的包字节证明整条判定链一致。

use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use minisign::KeyPair;
use zssh_update::{InstallRequest, parse_manifest, plan_install, verify_signature};

fn keypair() -> (KeyPair, String) {
    let kp = KeyPair::generate_unencrypted_keypair().expect("生成测试密钥");
    let text = kp.pk.to_box().expect("公钥文本").to_string();
    let pubkey = base64::engine::general_purpose::STANDARD.encode(text);
    (kp, pubkey)
}

fn sign(kp: &KeyPair, data: &[u8], version: &str) -> String {
    let signed = minisign::sign(
        Some(&kp.pk),
        &kp.sk,
        data,
        Some(&format!("timestamp:1\tversion:{version}")),
        Some("zssh test"),
    )
    .expect("签名");
    base64::engine::general_purpose::STANDARD.encode(signed.to_string())
}

fn manifest_json(version: &str, signature: &str) -> String {
    format!(
        r#"{{
            "version": "{version}",
            "notes": "release notes",
            "pub_date": "2026-10-30T00:00:00.000Z",
            "platforms": {{
                "windows-x86_64": {{
                    "signature": "{signature}",
                    "url": "https://example.com/Z-SSH_{version}_x64-setup.exe"
                }},
                "windows-x86_64-nsis": {{
                    "signature": "{signature}",
                    "url": "https://example.com/Z-SSH_{version}_x64-setup.exe"
                }}
            }}
        }}"#
    )
}

fn temp_cache(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("zssh-update-chain-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("创建缓存目录");
    root
}

#[test]
fn manifest_to_install_plan_chain() {
    let (kp, pubkey) = keypair();
    let package_bytes = b"fake nsis installer bytes";
    let signature = sign(&kp, package_bytes, "0.3.0");
    let manifest = parse_manifest(&manifest_json("0.3.0", &signature)).expect("解析 manifest");

    let package = manifest
        .select("0.2.14")
        .expect("选择包")
        .expect("应有更新");
    assert_eq!(package.platform_key, "windows-x86_64-nsis");
    verify_signature(
        package_bytes,
        &package.signature,
        &pubkey,
        &package.version,
        true,
    )
    .expect("验签与版本绑定通过");

    let cache = temp_cache("ok");
    let installer = cache.join("Z-SSH_0.3.0_x64-setup.exe");
    fs::write(&installer, package_bytes).expect("写安装包");
    let plan = plan_install(&InstallRequest {
        installer,
        cache_root: cache,
        version: package.version.clone(),
        restart: true,
        current_exe_args: vec!["--hidden".to_owned()],
        sha256: zssh_update::sha256_hex(package_bytes),
    })
    .expect("生成安装计划");
    assert_eq!(plan.args, ["/P", "/UPDATE", "/R", "/ARGS", "--hidden"]);
}

#[test]
fn tampered_manifest_version_is_rejected_by_the_signature() {
    let (kp, pubkey) = keypair();
    let package_bytes = b"old package";
    // 包实际是 0.2.0 签的，但 manifest 声称 0.3.0。
    let signature = sign(&kp, package_bytes, "0.2.0");
    let manifest = parse_manifest(&manifest_json("0.3.0", &signature)).expect("解析 manifest");
    let package = manifest.select("0.2.14").unwrap().expect("应有更新");

    let err = verify_signature(
        package_bytes,
        &package.signature,
        &pubkey,
        &package.version,
        true,
    )
    .expect_err("版本绑定应拒绝");
    assert!(
        matches!(err, zssh_update::UpdateError::SignedVersionMismatch { .. }),
        "{err}"
    );
}

#[test]
fn tampered_package_bytes_are_rejected_before_install_planning() {
    let (kp, pubkey) = keypair();
    let signature = sign(&kp, b"original", "0.3.0");
    let manifest = parse_manifest(&manifest_json("0.3.0", &signature)).expect("解析 manifest");
    let package = manifest.select("0.2.14").unwrap().expect("应有更新");

    let err = verify_signature(
        b"modified",
        &package.signature,
        &pubkey,
        &package.version,
        true,
    )
    .expect_err("改包应拒绝");
    assert!(
        matches!(err, zssh_update::UpdateError::Signature(_)),
        "{err}"
    );
}
