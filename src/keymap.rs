//! US 键盘布局「字符 → (基础字符, 是否需要 Shift, macOS 虚拟键码)」映射。
//!
//! macOS 下使用 `Keyboard::raw(mac_keycode, Click)` 发送 raw keycode，绕过 enigo 的
//! `get_layoutdependent_keycode`（后者遍历不 break，小键盘 `.`keycode=65 覆盖主键盘 47，
//! 导致 `>` 输出为 `.`）。非 macOS 仍用 `Key::Unicode(base)`。
//! macOS keycode 移植自原 Python `typepaste/keymap.py` 的 `_KEY_MAP`（已验证可用）。

/// 单个字符的按键动作。
pub enum KeyAction {
    /// 按基础字符的物理键；shift=true 时需额外按住 Shift。
    /// mac_keycode 为 macOS 虚拟键码（US 布局），macOS 下直接用 raw() 发送。
    Char {
        base: char,
        shift: bool,
        mac_keycode: u16,
    },
    /// 回车。
    Return,
    /// 制表符。
    Tab,
}

/// 符号映射表：(输入字符, 基础字符, 是否需 Shift, macOS keycode)。
const KEY_MAP: &[(char, char, bool, u16)] = &[
    // 空格
    (' ', ' ', false, 49),
    // 数字行
    ('1', '1', false, 18),
    ('!', '1', true, 18),
    ('2', '2', false, 19),
    ('@', '2', true, 19),
    ('3', '3', false, 20),
    ('#', '3', true, 20),
    ('4', '4', false, 21),
    ('$', '4', true, 21),
    ('5', '5', false, 23),
    ('%', '5', true, 23),
    ('6', '6', false, 22),
    ('^', '6', true, 22),
    ('7', '7', false, 26),
    ('&', '7', true, 26),
    ('8', '8', false, 28),
    ('*', '8', true, 28),
    ('9', '9', false, 25),
    ('(', '9', true, 25),
    ('0', '0', false, 29),
    (')', '0', true, 29),
    ('-', '-', false, 27),
    ('_', '-', true, 27),
    ('=', '=', false, 24),
    ('+', '=', true, 24),
    // 字母行符号
    ('[', '[', false, 33),
    ('{', '[', true, 33),
    (']', ']', false, 30),
    ('}', ']', true, 30),
    ('\\', '\\', false, 42),
    ('|', '\\', true, 42),
    (';', ';', false, 41),
    (':', ';', true, 41),
    ('\'', '\'', false, 39),
    ('"', '\'', true, 39),
    (',', ',', false, 43),
    ('<', ',', true, 43),
    ('.', '.', false, 47),
    ('>', '.', true, 47),
    ('/', '/', false, 44),
    ('?', '/', true, 44),
    ('`', '`', false, 50),
    ('~', '`', true, 50),
];

/// 字母 → macOS keycode 映射（小写大写同 keycode）。
/// 按 a-z 顺序，keycode 取自原 Python keymap.py。
const LETTER_KEYCODES: [u16; 26] = [
    0,  // a
    11, // b
    8,  // c
    2,  // d
    14, // e
    3,  // f
    5,  // g
    4,  // h
    34, // i
    38, // j
    40, // k
    37, // l
    46, // m
    45, // n
    31, // o
    35, // p
    12, // q
    15, // r
    1,  // s
    17, // t
    32, // u
    9,  // v
    13, // w
    7,  // x
    16, // y
    6,  // z
];

/// 查找字符的按键动作。字母大小写动态推导；查表得符号；`\n\r`→Return, `\t`→Tab。
pub fn get_key_info(ch: char) -> Option<KeyAction> {
    match ch {
        '\n' | '\r' => Some(KeyAction::Return),
        '\t' => Some(KeyAction::Tab),
        'a'..='z' => Some(KeyAction::Char {
            base: ch,
            shift: false,
            mac_keycode: LETTER_KEYCODES[(ch as u8 - b'a') as usize],
        }),
        'A'..='Z' => Some(KeyAction::Char {
            base: ch.to_ascii_lowercase(),
            shift: true,
            mac_keycode: LETTER_KEYCODES[(ch as u8 - b'A') as usize],
        }),
        _ => KEY_MAP
            .iter()
            .find(|(c, _, _, _)| *c == ch)
            .map(|(_, base, shift, kc)| KeyAction::Char {
                base: *base,
                shift: *shift,
                mac_keycode: *kc,
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_letter() {
        match get_key_info('a').unwrap() {
            KeyAction::Char {
                base,
                shift,
                mac_keycode,
            } => {
                assert_eq!(base, 'a');
                assert!(!shift);
                assert_eq!(mac_keycode, 0);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn uppercase_letter() {
        match get_key_info('Z').unwrap() {
            KeyAction::Char {
                base,
                shift,
                mac_keycode,
            } => {
                assert_eq!(base, 'z');
                assert!(shift);
                assert_eq!(mac_keycode, 6);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn special_chars_need_shift() {
        for (ch, base, kc) in [
            ('!', '1', 18),
            ('@', '2', 19),
            ('{', '[', 33),
            ('"', '\'', 39),
            ('~', '`', 50),
        ] {
            match get_key_info(ch).unwrap() {
                KeyAction::Char {
                    base: b,
                    shift,
                    mac_keycode,
                } => {
                    assert_eq!(b, base);
                    assert!(shift, "{ch} should need shift");
                    assert_eq!(mac_keycode, kc, "{ch} keycode mismatch");
                }
                _ => panic!(),
            }
        }
    }

    #[test]
    fn dot_and_greater_than_same_keycode() {
        // `.` 和 `>` 共用 keycode 47（主键盘），不应被小键盘 65 覆盖
        match get_key_info('.').unwrap() {
            KeyAction::Char { mac_keycode, .. } => assert_eq!(mac_keycode, 47),
            _ => panic!(),
        }
        match get_key_info('>').unwrap() {
            KeyAction::Char {
                mac_keycode, shift, ..
            } => {
                assert_eq!(mac_keycode, 47);
                assert!(shift);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn digit_no_shift() {
        match get_key_info('5').unwrap() {
            KeyAction::Char {
                base,
                shift,
                mac_keycode,
            } => {
                assert_eq!(base, '5');
                assert!(!shift);
                assert_eq!(mac_keycode, 23);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn newline_and_tab() {
        assert!(matches!(get_key_info('\n').unwrap(), KeyAction::Return));
        assert!(matches!(get_key_info('\r').unwrap(), KeyAction::Return));
        assert!(matches!(get_key_info('\t').unwrap(), KeyAction::Tab));
    }

    #[test]
    fn unknown_char_is_none() {
        assert!(get_key_info('中').is_none());
    }
}
