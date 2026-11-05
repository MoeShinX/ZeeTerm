//! 主密码强度条的评分（0.2.14 `state/syncModel.ts` 的 `passwordStrength`）：
//! 只看长度和字符种类，是提醒不是门槛（门槛是核心的最短长度）。

use crate::i18n::{Msg, t};

/// 强度条上的文字（四个档位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncStrengthMsg {
    TooWeak,
    Weak,
    Fair,
    Strong,
}

impl Msg for SyncStrengthMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::TooWeak => "太弱",
            Self::Weak => "弱",
            Self::Fair => "一般",
            Self::Strong => "强",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::TooWeak => "Too weak",
            Self::Weak => "Weak",
            Self::Fair => "Fair",
            Self::Strong => "Strong",
        }
    }
}

/// 评分和文字。`score`：0 空 · 1 太弱 · 2 弱 · 3 一般 · 4 强。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Strength {
    pub score: u8,
    pub label: &'static str,
}

pub fn password_strength(password: &str) -> Strength {
    let length = password.chars().count();
    if length == 0 {
        return Strength {
            score: 0,
            label: "",
        };
    }
    let kinds = [
        password.chars().any(|c| c.is_ascii_lowercase()),
        password.chars().any(|c| c.is_ascii_uppercase()),
        password.chars().any(|c| c.is_ascii_digit()),
        // 0.2.14 的 `/[^A-Za-z0-9]/`：中文、空格、符号都算「其他」。
        password.chars().any(|c| !c.is_ascii_alphanumeric()),
    ]
    .iter()
    .filter(|present| **present)
    .count();
    if length < 8 {
        return Strength {
            score: 1,
            label: t(SyncStrengthMsg::TooWeak),
        };
    }
    if length >= 16 || (length >= 12 && kinds >= 3) {
        return Strength {
            score: 4,
            label: t(SyncStrengthMsg::Strong),
        };
    }
    if length >= 12 || kinds >= 3 {
        return Strength {
            score: 3,
            label: t(SyncStrengthMsg::Fair),
        };
    }
    Strength {
        score: 2,
        label: t(SyncStrengthMsg::Weak),
    }
}

#[cfg(test)]
mod tests {
    use super::password_strength;

    #[test]
    fn strength_follows_the_old_front_end() {
        assert_eq!(password_strength("").score, 0);
        assert_eq!(password_strength("abc12").label, "太弱");
        assert_eq!(password_strength("abcdefgh").label, "弱");
        // 8 位但有三种字符：一般。
        assert_eq!(password_strength("Abcdef12").label, "一般");
        // 12 位只有一种字符：一般；12 位三种字符：强；16 位一律强。
        assert_eq!(password_strength("abcdefghijkl").label, "一般");
        assert_eq!(password_strength("Abcdefghij12").label, "强");
        assert_eq!(password_strength("abcdefghijklmnop").label, "强");
    }

    #[test]
    fn length_counts_characters_not_bytes() {
        // 6 个汉字 = 18 字节，但只有 6 个字符：太弱。
        assert_eq!(password_strength("密码密码密码").score, 1);
        // 12 个汉字：长度够 12，其他字符算一种，所以是「一般」。
        assert_eq!(password_strength("密码密码密码密码密码密码").label, "一般");
    }
}
