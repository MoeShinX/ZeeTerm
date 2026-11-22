//! 输入文本里"位置"的三种单位。它们数值上经常不一样（一个汉字：3 个 UTF-8 字节、1 个 UTF-16 单元、
//! 终端里占 2 列），混着用会把光标算错，所以各自一个类型，互转必须走下面的函数。

/// 输入文本里的 UTF-8 字节下标（必须落在字符边界上）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteIdx(pub usize);

/// 输入法用的 UTF-16 单元下标。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utf16Idx(pub usize);

/// 终端的列号（单元格），宽字符占两列。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermCol(pub usize);

/// 字节下标换成 UTF-16 下标；不在字符边界或越界返回 `None`。
pub fn byte_to_utf16(text: &str, byte: ByteIdx) -> Option<Utf16Idx> {
    if byte.0 > text.len() || !text.is_char_boundary(byte.0) {
        return None;
    }
    Some(Utf16Idx(text[..byte.0].encode_utf16().count()))
}

/// UTF-16 下标换成字节下标；越界或落在代理对中间返回 `None`。
pub fn utf16_to_byte(text: &str, index: Utf16Idx) -> Option<ByteIdx> {
    let mut units = 0usize;
    for (byte, ch) in text.char_indices() {
        if units == index.0 {
            return Some(ByteIdx(byte));
        }
        units += ch.len_utf16();
        if units > index.0 {
            return None;
        }
    }
    (units == index.0).then_some(ByteIdx(text.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_units_disagree_on_cjk_and_astral_characters() {
        // "a你😀b"：字节 1+3+4+1，UTF-16 1+1+2+1。
        let text = "a你😀b";
        assert_eq!(text.len(), 9);
        assert_eq!(byte_to_utf16(text, ByteIdx(0)), Some(Utf16Idx(0)));
        assert_eq!(byte_to_utf16(text, ByteIdx(1)), Some(Utf16Idx(1)));
        assert_eq!(byte_to_utf16(text, ByteIdx(4)), Some(Utf16Idx(2)));
        assert_eq!(byte_to_utf16(text, ByteIdx(8)), Some(Utf16Idx(4)));
        assert_eq!(byte_to_utf16(text, ByteIdx(9)), Some(Utf16Idx(5)));
    }

    #[test]
    fn conversions_reject_positions_inside_a_character() {
        let text = "a你😀b";
        assert_eq!(byte_to_utf16(text, ByteIdx(2)), None, "你的中间");
        assert_eq!(byte_to_utf16(text, ByteIdx(10)), None, "越界");
        assert_eq!(utf16_to_byte(text, Utf16Idx(3)), None, "代理对中间");
        assert_eq!(utf16_to_byte(text, Utf16Idx(6)), None, "越界");
    }

    #[test]
    fn round_trips_on_every_boundary() {
        let text = "ls 你好 😀 e\u{301}x";
        for (byte, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            let units = byte_to_utf16(text, ByteIdx(byte)).expect("边界");
            assert_eq!(utf16_to_byte(text, units), Some(ByteIdx(byte)));
        }
    }
}
