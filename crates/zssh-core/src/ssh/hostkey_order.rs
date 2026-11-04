//! 按 known_hosts 里已保存的密钥类型调整主机密钥算法的协商顺序（V2-C7a S3）。
//!
//! russh 的默认顺序是 Ed25519 > ECDSA > RSA。服务器同时有几把不同类型的主机密钥时，客户端的偏好
//! 决定出示哪一把：一台以前只存过 ECDSA 的主机，如果这次被协商成 Ed25519，就会落到"只记过别种类型"
//! 这条要人工核对的路上，白白多一次提示；更糟的是中间人可以借此挑一种我们没存过的类型来出示。
//! OpenSSH 的做法是把已保存类型排到最前，这里照做：没保存过就保持默认顺序。

use russh::keys::Algorithm;
use russh::Preferred;

use crate::ssh::interact::SavedHostKey;
use crate::ssh::known_hosts::KnownHosts;

/// 这台主机在 known_hosts 里已保存的密钥类型和指纹（每种类型取第一条普通记录）。
/// known_hosts 读不了、或者什么都没存，返回空：调用方按默认顺序协商。
pub(crate) fn saved_host_keys(
    known_hosts: &KnownHosts,
    host: &str,
    port: u16,
) -> Vec<SavedHostKey> {
    known_hosts
        .saved_key_types(host, port)
        .into_iter()
        .map(|(key_type, fingerprint)| SavedHostKey {
            key_type,
            fingerprint,
        })
        .collect()
}

/// 同一种密钥：RSA 的各种签名哈希（`ssh-rsa` / `rsa-sha2-256` / `rsa-sha2-512`）算一种。
fn same_family(a: &Algorithm, b: &Algorithm) -> bool {
    matches!((a, b), (Algorithm::Rsa { .. }, Algorithm::Rsa { .. })) || a == b
}

/// 把已保存类型对应的算法排到 `preferred.key` 最前，其余保持原来的相对顺序。
pub(crate) fn prefer_saved(preferred: &mut Preferred, saved: &[SavedHostKey]) {
    let saved_algorithms: Vec<Algorithm> = saved
        .iter()
        .filter_map(|key| key.key_type.parse::<Algorithm>().ok())
        .collect();
    if saved_algorithms.is_empty() {
        return;
    }
    let (first, rest): (Vec<Algorithm>, Vec<Algorithm>) = preferred
        .key
        .iter()
        .cloned()
        .partition(|algorithm| saved_algorithms.iter().any(|s| same_family(s, algorithm)));
    let mut ordered = first;
    ordered.extend(rest);
    preferred.key = ordered.into();
}

#[cfg(test)]
mod tests {
    use russh::keys::ssh_key::{EcdsaCurve, HashAlg};

    use super::*;
    use crate::ssh::known_hosts::fingerprint_of;

    fn saved(key_type: &str) -> SavedHostKey {
        SavedHostKey {
            key_type: key_type.to_owned(),
            fingerprint: "SHA256:x".to_owned(),
        }
    }

    fn ecdsa(curve: EcdsaCurve) -> Algorithm {
        Algorithm::Ecdsa { curve }
    }

    #[test]
    fn without_saved_keys_the_default_order_is_untouched() {
        let mut preferred = Preferred::default();
        let before = preferred.key.clone();
        prefer_saved(&mut preferred, &[]);
        assert_eq!(preferred.key, before);
        // 认不出的类型也不动顺序。
        prefer_saved(&mut preferred, &[saved("no-such-type")]);
        assert_eq!(preferred.key, before);
    }

    #[test]
    fn a_saved_type_goes_first_and_the_rest_keep_their_relative_order() {
        let default = Preferred::default().key;
        let mut preferred = Preferred::default();
        prefer_saved(&mut preferred, &[saved("ecdsa-sha2-nistp256")]);
        assert_eq!(preferred.key.first(), Some(&ecdsa(EcdsaCurve::NistP256)));
        let others: Vec<&Algorithm> = preferred.key.iter().skip(1).collect();
        let expected: Vec<&Algorithm> = default
            .iter()
            .filter(|a| **a != ecdsa(EcdsaCurve::NistP256))
            .collect();
        assert_eq!(others, expected);
        assert_eq!(preferred.key.len(), default.len(), "只排序，不增不减");
    }

    #[test]
    fn a_saved_rsa_key_moves_all_rsa_signature_hashes_up() {
        let mut preferred = Preferred::default();
        prefer_saved(&mut preferred, &[saved("ssh-rsa")]);
        let front: Vec<&Algorithm> = preferred
            .key
            .iter()
            .take_while(|a| matches!(a, Algorithm::Rsa { .. }))
            .collect();
        assert!(
            front.contains(&&Algorithm::Rsa {
                hash: Some(HashAlg::Sha512)
            }) && front.contains(&&Algorithm::Rsa {
                hash: Some(HashAlg::Sha256)
            }),
            "{:?}",
            preferred.key
        );
    }

    #[test]
    fn several_saved_types_all_precede_the_unsaved_ones() {
        let mut preferred = Preferred::default();
        prefer_saved(
            &mut preferred,
            &[saved("ecdsa-sha2-nistp521"), saved("ssh-ed25519")],
        );
        let head: Vec<&Algorithm> = preferred.key.iter().take(2).collect();
        assert!(head.contains(&&Algorithm::Ed25519));
        assert!(head.contains(&&ecdsa(EcdsaCurve::NistP521)));
    }

    #[test]
    fn saved_types_are_read_back_from_a_known_hosts_file() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("known_hosts");
        let known = KnownHosts::new(&path);
        assert!(
            saved_host_keys(&known, "web-1", 22).is_empty(),
            "文件不存在"
        );

        // 一条 ECDSA，非默认端口。
        let other = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
        )
        .expect("生成密钥");
        known
            .append("web-1", 2222, other.public_key())
            .expect("保存");
        // web-2：同一台主机存了两种类型，按文件里的顺序都列出来。
        let ed = russh::keys::PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .expect("生成密钥");
        known.append("web-2", 22, ed.public_key()).expect("保存");
        known.append("web-2", 22, other.public_key()).expect("保存");

        let found = saved_host_keys(&known, "web-1", 2222);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key_type, "ecdsa-sha2-nistp256");
        assert_eq!(found[0].fingerprint, fingerprint_of(other.public_key()));
        // 同一台机器的另一个端口、别的主机：互不相干。
        assert!(saved_host_keys(&known, "web-1", 22).is_empty());
        assert!(saved_host_keys(&known, "web-3", 22).is_empty());
        let both: Vec<String> = saved_host_keys(&known, "web-2", 22)
            .into_iter()
            .map(|key| key.key_type)
            .collect();
        assert_eq!(both, ["ssh-ed25519", "ecdsa-sha2-nistp256"]);
    }
}
