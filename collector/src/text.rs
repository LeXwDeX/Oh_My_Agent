//! 文本清洗：与 hub 端 clean_text() 一致，去掉控制字符（<0x20、0x7f）与 Unicode 格式字符（Cf 类）。

/// Unicode 格式控制字符（Cf 类，如 U+202E 方向覆盖、零宽字符）
pub fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// 需要剔除的字符：控制字符或 Cf 类
pub fn is_banned(c: char) -> bool {
    (c as u32) < 0x20 || c as u32 == 0x7f || is_format_char(c)
}

/// 去掉禁用字符后截断到 n 个 Unicode 字符
pub fn clean(s: &str, n: usize) -> String {
    s.chars().filter(|&c| !is_banned(c)).take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text() {
        assert_eq!(clean("a\u{202E}b\u{200B}c\n", 10), "abc");
        assert_eq!(clean("一二三四", 2), "一二");
    }
}
