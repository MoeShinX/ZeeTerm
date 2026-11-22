//! 更新链路串联：manifest -> 平台选择 -> 验签 -> 版本绑定 -> 安装计划。
//!
//! 不下载、不启动安装器；用测试密钥和内存里的包字节证明整条判定链一致。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use minisign::KeyPair;
use zeeterm_update::{
    InstallRequest, file_name_from_url, parse_manifest, plan_install, verify_signature,
};

fn keypair() -> (KeyPair, String) {
    let kp = KeyPair::generate_unencrypted_keypair().expect("生成测试密钥");
    let text = kp.pk.to_box().expect("公钥文本").to_string();
    let pubkey = base64::engine::general_purpose::STANDARD.encode(text);
    (kp, pubkey)
}

fn sign(kp: &KeyPair, data: &[u8], version: &str) -> String {
    sign_named(kp, data, version, &format!("Z-SSH_{version}_x64-setup.exe"))
}

/// 和线上发布的签名一样，trusted comment 里带 `file:`（见 `trusted_comment`）。
fn sign_named(kp: &KeyPair, data: &[u8], version: &str, file: &str) -> String {
    let signed = minisign::sign(
        Some(&kp.pk),
        &kp.sk,
        data,
        Some(&format!("timestamp:1\tfile:{file}\tversion:{version}")),
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

/// 随机唯一的缓存目录；返回的 `TempDir` 要拿到用例结束，drop 时整棵删掉。
fn temp_cache(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("zeeterm-update-chain-{name}-"))
        .tempdir()
        .expect("创建缓存目录");
    let root = dir.path().to_path_buf();
    (dir, root)
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
        &file_name_from_url(&package.url).expect("下载地址里有文件名"),
        true,
    )
    .expect("验签与版本绑定通过");

    let (_dir_cache, cache) = temp_cache("ok");
    let installer = cache.join("Z-SSH_0.3.0_x64-setup.exe");
    fs::write(&installer, package_bytes).expect("写安装包");
    let plan = plan_install(&InstallRequest {
        installer,
        cache_root: cache,
        version: package.version.clone(),
        restart: true,
        current_exe_args: vec!["--hidden".to_owned()],
        sha256: zeeterm_update::sha256_hex(package_bytes),
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
        &file_name_from_url(&package.url).expect("下载地址里有文件名"),
        true,
    )
    .expect_err("版本绑定应拒绝");
    assert!(
        matches!(
            err,
            zeeterm_update::UpdateError::SignedVersionMismatch { .. }
        ),
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
        &file_name_from_url(&package.url).expect("下载地址里有文件名"),
        true,
    )
    .expect_err("改包应拒绝");
    assert!(
        matches!(err, zeeterm_update::UpdateError::Signature(_)),
        "{err}"
    );
}

/// 清单不签名：中间人把 `url` 换成同一版本里别的包（签名本身有效），验签必须因为文件名对不上而拒绝。
#[test]
fn a_manifest_pointing_at_another_package_is_rejected_by_the_file_name() {
    let (kp, pubkey) = keypair();
    let arm_bytes = b"package for another architecture";
    // 别的架构的包，版本一样，签名是有效的。
    let arm_signature = sign_named(&kp, arm_bytes, "0.3.0", "Z-SSH_0.3.0_arm64-setup.exe");
    // 清单里这个平台的地址是 x64 的包，签名却被换成了 arm64 包的。
    let manifest = parse_manifest(&manifest_json("0.3.0", &arm_signature)).expect("解析 manifest");
    let package = manifest.select("0.2.14").unwrap().expect("应有更新");
    assert!(package.url.ends_with("Z-SSH_0.3.0_x64-setup.exe"));

    let err = verify_signature(
        arm_bytes,
        &package.signature,
        &pubkey,
        &package.version,
        &file_name_from_url(&package.url).expect("下载地址里有文件名"),
        true,
    )
    .expect_err("文件名对不上应拒绝");
    assert!(
        matches!(err, zeeterm_update::UpdateError::SignedFileMismatch { .. }),
        "{err}"
    );
}
