//! 按 known_hosts 里已保存的密钥类型调整主机密钥算法的协商顺序（V2-C7a S3）。
//!
//! russh 的默认顺序是 Ed25519 > ECDSA > RSA。服务器同时有几把不同类型的主机密钥时，客户端的偏好
//! 决定出示哪一把：一台以前只存过 ECDSA 的主机，如果这次被协商成 Ed25519，就会落到"只记过别种类型"
//! 这条要人工核对的路上，白白多一次提示；更糟的是中间人可以借此挑一种我们没存过的类型来出示。
//! OpenSSH 的做法是把已保存类型排到最前，这里照做：没保存过就保持默认顺序。
//!
//! `known_hosts.rs` 只提供"拿一把具体的公钥去查"的接口，没有"列出这台主机存了哪些类型"。这里用
//! 每种类型各一把固定的探测公钥去查：查到同类型的记录（`Changed`，或恰好相等的 `Match`）就说明存过这个类型，
//! 顺带拿到那条记录的指纹。探测公钥只是拿来问"有没有这个类型"，不是任何人的主机密钥。

use russh::keys::{Algorithm, PublicKey};
use russh::Preferred;

use crate::ssh::interact::SavedHostKey;
use crate::ssh::known_hosts::{fingerprint_of, HostKeyLookup, KnownHosts};

/// 探测公钥（OpenSSH 格式），第一把必须是 Ed25519：它查不到任何记录就说明这台主机什么都没存。
const PROBES: [&str; 5] = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILieUUCUMqodzbGSNH03oxF8UkibDWiPKvjw238iaSXh",
    "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBDz6jONAgWFAQy3/7v/h08eXDytYBw2xNSwDh0KeUXwNjnYdnI4UCTMgXbT1xUSEGlXc3aYzoRsSRFLAj/rVASE=",
    "ecdsa-sha2-nistp384 AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBOaSBIQEnRTnqj672Bk1BFLL0s+RPx/utkAgfs/Dngf6LAhoheLIVKj3Wu3ADdkiU2EvsWNjvMQuDbFY8lw78TGhG7kF8GCukkGBLvV4uZAFQMNcp3l3MJckSukrU0q/lA==",
    "ecdsa-sha2-nistp521 AAAAE2VjZHNhLXNoYTItbmlzdHA1MjEAAAAIbmlzdHA1MjEAAACFBADn4w4Ex6frHB1jGU2OCaLqNmUcT/6Mmey4cS2fRB8cGDov47BDOvdiOv8od4dqts1vHfIC2Q7wocvjfaeT9Vzl0AFJJsgBBrko54cQzk6yB7b1IvstYfQEct1aYtH5uPbXakZqR5QBkUX3Jxg7ng+KJQ00mXj6UXUMF1gOCE/UvrBKqA==",
    "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDPpzGqEQMzvzpw0zgyo11NZZyLkFuyP0kopWxuJeJ/9TuDPi9pGN/HR4z9GFlkFdpVWT65/lhcNv2TtOXWG3SyA2miQ124mr3kt4yH71PZp0I+OpV/rKikdIyLiS4IA7Ah1bosD73oUZYLL379+dt5ICNX2r4KObjvBwkTpON7IoAxiTX4LVQg1l9o9r1XNUSILimV9Mni1xHO4u5ajkrlBPY7Ow33mf1qxVQyAACQ/KRakCJffeITBpFy67cSSYPIiojK7Fu+4srJdS2K9RptOnFJBFR0mAHjSwzqxs6N52j3iwUtymHpOoJ+FFofZTy0QEs6IRemN7c35uB4kVMp",
];

/// 这台主机在 known_hosts 里已保存的密钥类型和指纹（每种类型取第一条普通记录）。
/// known_hosts 读不了、或者什么都没存，返回空：调用方按默认顺序协商。
pub(crate) fn saved_host_keys(
    known_hosts: &KnownHosts,
    host: &str,
    port: u16,
) -> Vec<SavedHostKey> {
    let mut saved = Vec::new();
    for (index, probe) in PROBES.iter().enumerate() {
        let Ok(key) = PublicKey::from_openssh(probe) else {
            continue;
        };
        let fingerprint = match known_hosts.lookup(host, port, &key) {
            HostKeyLookup::Changed {
                stored_fingerprint, ..
            } => stored_fingerprint,
            HostKeyLookup::Match => fingerprint_of(&key),
            // 第一把探测公钥是 Ed25519：连它都没得到"别种类型"，这台主机就是什么都没存。
            HostKeyLookup::Unknown if index == 0 => return Vec::new(),
            HostKeyLookup::Unreadable { .. } => return Vec::new(),
            _ => continue,
        };
        saved.push(SavedHostKey {
            key_type: key.algorithm().to_string(),
            fingerprint,
        });
    }
    saved
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
    fn every_probe_parses_and_is_the_type_it_claims_to_be() {
        let kinds: Vec<String> = PROBES
            .iter()
            .map(|probe| {
                PublicKey::from_openssh(probe)
                    .expect("探测公钥能解析")
                    .algorithm()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "ssh-ed25519",
                "ecdsa-sha2-nistp256",
                "ecdsa-sha2-nistp384",
                "ecdsa-sha2-nistp521",
                "ssh-rsa"
            ]
        );
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

        let ecdsa_key = PublicKey::from_openssh(PROBES[1]).expect("解析");
        // 用别的 ECDSA 密钥（不是探测公钥）存一条，非默认端口。
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
        known.append("web-2", 22, &ecdsa_key).expect("保存");

        let found = saved_host_keys(&known, "web-1", 2222);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key_type, "ecdsa-sha2-nistp256");
        assert_eq!(found[0].fingerprint, fingerprint_of(other.public_key()));
        // 同一台机器的另一个端口、别的主机：互不相干。
        assert!(saved_host_keys(&known, "web-1", 22).is_empty());
        assert!(saved_host_keys(&known, "web-3", 22).is_empty());
        // 恰好存的就是探测公钥本身也认得出来。
        assert_eq!(saved_host_keys(&known, "web-2", 22).len(), 1);
    }
}
