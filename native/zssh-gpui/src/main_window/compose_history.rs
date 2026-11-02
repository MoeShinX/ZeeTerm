//! 撰写栏的发送历史（0.2.14 `state/composeHistory.ts`）：最近发过的 50 条，↑ / ↓ 翻。
//!
//! 只存在内存里：不写磁盘、不进设置、不同步——里面可能有密码。列表按「旧 → 新」排；
//! 翻历史的状态是 `Nav { cursor, draft }`：cursor 为 `None` 表示没在翻，否则是正在看的那条
//! 的下标，draft 是开始翻之前输入框里的内容（翻回最新时还原）。

pub const HISTORY_MAX: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub cursor: Option<usize>,
    pub draft: String,
}

/// 记一条发送过的内容：和已有的一模一样就挪到最新，超过 50 条丢最旧的；空白不记。
pub fn push(list: &[String], text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return list.to_vec();
    }
    let mut next: Vec<String> = list.iter().filter(|item| *item != text).cloned().collect();
    next.push(text.to_owned());
    if next.len() > HISTORY_MAX {
        next.drain(..next.len() - HISTORY_MAX);
    }
    next
}

/// 光标是不是选区为空且在第一行（↑ 才翻历史）；输入框是空的也算。`start`/`end` 是字节下标。
pub fn caret_on_first_line(value: &str, start: usize, end: usize) -> bool {
    if value.is_empty() {
        return true;
    }
    start == end && !value[..start.min(value.len())].contains('\n')
}

/// 光标是不是选区为空且在最后一行（↓ 才翻历史）；输入框是空的也算。
pub fn caret_on_last_line(value: &str, start: usize, end: usize) -> bool {
    if value.is_empty() {
        return true;
    }
    start == end && !value[end.min(value.len())..].contains('\n')
}

pub struct Step {
    pub value: String,
    pub nav: Nav,
}

/// 往旧（`up`）或往新翻一步。翻不动（没有历史、已经是最旧 / 没在翻就按 ↓）返回 `None`，
/// 调用方就不拦这次按键，光标照常动。
pub fn step(list: &[String], nav: &Nav, up: bool, current: &str) -> Option<Step> {
    if up {
        if list.is_empty() {
            return None;
        }
        return match nav.cursor {
            None => {
                let cursor = list.len() - 1;
                Some(Step {
                    value: list[cursor].clone(),
                    nav: Nav {
                        cursor: Some(cursor),
                        draft: current.to_owned(),
                    },
                })
            }
            Some(0) => None,
            Some(at) => {
                let cursor = at.min(list.len()) - 1;
                Some(Step {
                    value: list[cursor].clone(),
                    nav: Nav {
                        cursor: Some(cursor),
                        draft: nav.draft.clone(),
                    },
                })
            }
        };
    }
    let at = nav.cursor?;
    let cursor = at + 1;
    if cursor >= list.len() {
        return Some(Step {
            value: nav.draft.clone(),
            nav: Nav::default(),
        });
    }
    Some(Step {
        value: list[cursor].clone(),
        nav: Nav {
            cursor: Some(cursor),
            draft: nav.draft.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn push_dedups_moves_to_newest_and_caps_at_fifty() {
        let base = list(&["a", "b", "c"]);
        assert_eq!(push(&base, "a"), list(&["b", "c", "a"]));
        assert_eq!(push(&base, "   "), base, "空白不记");
        let mut long = Vec::new();
        for n in 0..60 {
            long = push(&long, &format!("cmd{n}"));
        }
        assert_eq!(long.len(), HISTORY_MAX);
        assert_eq!(long[0], "cmd10");
        assert_eq!(long[49], "cmd59");
    }

    #[test]
    fn up_walks_back_and_down_restores_the_draft() {
        let history = list(&["one", "two"]);
        let first = step(&history, &Nav::default(), true, "draft").expect("有历史");
        assert_eq!(first.value, "two");
        let second = step(&history, &first.nav, true, "two").expect("再往前");
        assert_eq!(second.value, "one");
        assert!(
            step(&history, &second.nav, true, "one").is_none(),
            "最旧再按 ↑ 不拦"
        );
        let back = step(&history, &second.nav, false, "one").expect("往新");
        assert_eq!(back.value, "two");
        let end = step(&history, &back.nav, false, "two").expect("翻回最新");
        assert_eq!(end.value, "draft", "还原开始翻之前的内容");
        assert_eq!(end.nav, Nav::default());
        assert!(
            step(&history, &Nav::default(), false, "x").is_none(),
            "没在翻按 ↓ 不拦"
        );
        assert!(step(&[], &Nav::default(), true, "x").is_none());
    }

    #[test]
    fn arrows_only_act_on_the_first_or_last_line() {
        assert!(caret_on_first_line("", 0, 0));
        assert!(caret_on_first_line("abc\ndef", 2, 2));
        assert!(!caret_on_first_line("abc\ndef", 5, 5));
        assert!(!caret_on_first_line("abc", 0, 2), "有选区不翻");
        assert!(caret_on_last_line("abc\ndef", 5, 5));
        assert!(!caret_on_last_line("abc\ndef", 1, 1));
        assert!(caret_on_last_line("", 0, 0));
    }
}
