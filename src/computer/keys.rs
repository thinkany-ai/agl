//! Key combos such as "cmd+shift+t" or "return", as macOS virtual key codes
//! (US layout positions) and modifier flags.

use anyhow::{bail, Result};

pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALT: u64 = 0x0008_0000;
pub const FLAG_CMD: u64 = 0x0010_0000;
pub const FLAG_FN: u64 = 0x0080_0000;

#[derive(Debug, PartialEq)]
pub struct Combo {
    pub code: u16,
    pub flags: u64,
    /// The character a menu shows for it ("A" for cmd+a), for keys that
    /// are one character.
    pub ch: Option<char>,
}

fn code(key: &str) -> Option<u16> {
    let k = key.to_ascii_lowercase();
    let c = match k.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" | "equal" => 24,
        "9" => 25,
        "7" => 26,
        "-" | "minus" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" | "quote" => 39,
        "k" => 40,
        ";" | "semicolon" => 41,
        "\\" | "backslash" => 42,
        "," | "comma" => 43,
        "/" | "slash" => 44,
        "n" => 45,
        "m" => 46,
        "." | "period" => 47,
        "tab" => 48,
        "space" => 49,
        "`" | "grave" => 50,
        "delete" | "backspace" => 51,
        "escape" | "esc" => 53,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "home" => 115,
        "pageup" | "page_up" => 116,
        "forwarddelete" | "forward_delete" | "del" => 117,
        "end" => 119,
        "pagedown" | "page_down" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    };
    Some(c)
}

fn modifier(m: &str) -> Option<u64> {
    Some(match m.to_ascii_lowercase().as_str() {
        "cmd" | "command" | "meta" | "super" => FLAG_CMD,
        "shift" => FLAG_SHIFT,
        "ctrl" | "control" => FLAG_CONTROL,
        "alt" | "option" | "opt" => FLAG_ALT,
        "fn" => FLAG_FN,
        _ => return None,
    })
}

/// Parses "cmd+l", "shift+tab return" (space-separated combos, pressed in order).
pub fn parse(spec: &str) -> Result<Vec<Combo>> {
    let mut out = Vec::new();
    for combo in spec.split_whitespace() {
        let parts: Vec<&str> = combo.split('+').collect();
        // "cmd++" means cmd and the + key; plain "+" is not on the US layout.
        let (mods, key) = parts.split_at(parts.len() - 1);
        let key = key[0];
        if key.is_empty() {
            bail!("missing key in `{combo}`");
        }
        let mut flags = 0;
        for m in mods {
            flags |= modifier(m).ok_or_else(|| anyhow::anyhow!("unknown modifier `{m}` in `{combo}` (cmd, shift, ctrl, alt, fn)"))?;
        }
        let code = code(key).ok_or_else(|| anyhow::anyhow!("unknown key `{key}`; use computer_type for text"))?;
        // Navigation keys carry the fn flag, as the hardware keys do.
        if matches!(code, 115 | 116 | 117 | 119 | 121 | 123..=126) {
            flags |= FLAG_FN;
        }
        let mut chars = key.chars();
        let ch = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c.to_ascii_uppercase()),
            _ => None,
        };
        out.push(Combo { code, flags, ch });
    }
    if out.is_empty() {
        bail!("`keys` is empty");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(parse("cmd+l").unwrap(), vec![Combo { code: 37, flags: FLAG_CMD, ch: Some('L') }]);
        assert_eq!(parse("Return").unwrap(), vec![Combo { code: 36, flags: 0, ch: None }]);
        let v = parse("cmd+shift+t tab").unwrap();
        assert_eq!(v[0], Combo { code: 17, flags: FLAG_CMD | FLAG_SHIFT, ch: Some('T') });
        assert_eq!(v[1], Combo { code: 48, flags: 0, ch: None });
        assert_eq!(parse("cmd+,").unwrap()[0].ch, Some(','));
        assert_eq!(parse("left").unwrap()[0].flags, FLAG_FN);
        assert!(parse("hyper+x").is_err());
        assert!(parse("cmd+ü").is_err());
        assert!(parse("  ").is_err());
    }
}
