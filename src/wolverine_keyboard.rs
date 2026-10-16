//! USB HID keyboard usages and modifier bits used by Synapse's onboard codec.
//! mw-main.js module 4442 export CS (`U`) encodes function 2, size 2 as
//! [modifier mask, keyboard usage]. It is not a macro or text-typing engine.
use anyhow::{Result, bail, ensure};
use std::fmt::Write as _;

const MODIFIERS: [(&str, u8); 8] = [
    ("Ctrl", 1),
    ("Shift", 2),
    ("Alt", 4),
    ("Super", 8),
    ("RightCtrl", 16),
    ("RightShift", 32),
    ("RightAlt", 64),
    ("RightSuper", 128),
];
const KEYS: [(&str, u8); 40] = [
    ("Enter", 40),
    ("Escape", 41),
    ("Backspace", 42),
    ("Tab", 43),
    ("Space", 44),
    ("Minus", 45),
    ("Equal", 46),
    ("LeftBracket", 47),
    ("RightBracket", 48),
    ("Backslash", 49),
    ("Semicolon", 51),
    ("Quote", 52),
    ("Grave", 53),
    ("Comma", 54),
    ("Period", 55),
    ("Slash", 56),
    ("CapsLock", 57),
    ("PrintScreen", 70),
    ("ScrollLock", 71),
    ("Pause", 72),
    ("Insert", 73),
    ("Home", 74),
    ("PageUp", 75),
    ("Delete", 76),
    ("End", 77),
    ("PageDown", 78),
    ("Right", 79),
    ("Left", 80),
    ("Down", 81),
    ("Up", 82),
    ("NumLock", 83),
    ("KeypadDivide", 84),
    ("KeypadMultiply", 85),
    ("KeypadMinus", 86),
    ("KeypadPlus", 87),
    ("KeypadEnter", 88),
    ("KeypadPeriod", 99),
    ("Menu", 101),
    ("KeypadEqual", 103),
    ("NonUSBackslash", 100),
];
fn key_usage(name: &str) -> Option<u8> {
    if name.len() == 1 {
        let key = name.as_bytes()[0].to_ascii_lowercase();
        return match key {
            b'a'..=b'z' => Some(key - b'a' + 4),
            b'1'..=b'9' => Some(key - b'1' + 30),
            b'0' => Some(39),
            _ => None,
        };
    }
    if let Some(number) = name
        .strip_prefix('F')
        .or_else(|| name.strip_prefix('f'))
        .and_then(|number| number.parse::<u8>().ok())
    {
        return match number {
            1..=12 => Some(57 + number),
            13..=24 => Some(91 + number),
            _ => None,
        };
    }
    if name.len() == 7
        && name
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Keypad"))
    {
        return match name.as_bytes()[6] {
            b'1'..=b'9' => Some(name.as_bytes()[6] - b'1' + 89),
            b'0' => Some(98),
            _ => None,
        };
    }
    KEYS.iter()
        .find(|(label, _)| label.eq_ignore_ascii_case(name))
        .map(|(_, usage)| *usage)
}
pub fn parse(shortcut: &str) -> Result<(u8, u8)> {
    ensure!(
        !shortcut.trim().is_empty(),
        "Enter a shortcut, for example Ctrl+Shift+F13"
    );
    ensure!(shortcut.len() <= 128, "Keyboard shortcut is too long");
    let mut modifiers = 0;
    let mut key = None;
    for part in shortcut.split('+') {
        let part = part.trim();
        ensure!(
            !part.is_empty(),
            "Empty shortcut component; use names such as Equal or KeypadPlus"
        );
        if let Some((_, mask)) = MODIFIERS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(part))
        {
            ensure!(modifiers & mask == 0, "Duplicate modifier {part}");
            modifiers |= mask;
        } else {
            ensure!(
                key.is_none(),
                "A shortcut contains one keyboard key plus optional modifiers, not a macro sequence"
            );
            key = Some(key_usage(part).ok_or_else(|| anyhow::anyhow!("Unknown USB keyboard key {part:?}; use A–Z, 0–9, F1–F24 or a named key such as Enter"))?);
        }
    }
    match key {
        Some(key) => Ok((modifiers, key)),
        None if modifiers.count_ones() == 1 => Ok((0, 224 + modifiers.trailing_zeros() as u8)),
        None => {
            bail!("Add a keyboard key to this modifier combination, for example Ctrl+Shift+F13")
        }
    }
}
fn write_key(text: &mut String, key: u8) {
    match key {
        4..=29 => text.push(char::from(b'A' + key - 4)),
        30..=38 => text.push(char::from(b'1' + key - 30)),
        39 => text.push('0'),
        58..=69 => {
            let _ = write!(text, "F{}", key - 57);
        }
        104..=115 => {
            let _ = write!(text, "F{}", key - 91);
        }
        89..=97 => {
            let _ = write!(text, "Keypad{}", key - 88);
        }
        98 => text.push_str("Keypad0"),
        224..=231 => text.push_str(MODIFIERS[usize::from(key - 224)].0),
        _ => match KEYS.iter().find(|(_, usage)| *usage == key) {
            Some((name, _)) => text.push_str(name),
            None => {
                let _ = write!(text, "USB key {key:#04x}");
            }
        },
    }
}
pub fn describe(modifiers: u8, key: u8) -> String {
    let mut text = String::new();
    for (label, mask) in MODIFIERS {
        if modifiers & mask != 0 {
            text.push_str(label);
            text.push('+');
        }
    }
    if key != 0 {
        write_key(&mut text, key);
    } else {
        text.pop();
    }
    text
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_shortcuts_at_hid_boundaries_without_implied_shift() {
        assert_eq!(parse("Ctrl+Shift+F13").unwrap(), (3, 104));
        assert_eq!(parse("RightAlt+F24").unwrap(), (64, 115));
        assert_eq!(parse("a").unwrap(), (0, 4));
        assert_eq!(parse("0").unwrap(), (0, 39));
        assert_eq!(parse("Ctrl").unwrap(), (0, 224));
    }
    #[test]
    fn rejects_sequences_ambiguous_modifiers_and_out_of_range_keys() {
        for text in [
            "",
            "Ctrl++A",
            "Ctrl+Ctrl+A",
            "A+B",
            "F25",
            "Ctrl+Shift",
            "é",
            "abcdeé",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }
}
