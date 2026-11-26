//! Key and mouse encoding matching `GhosttyKey` and xterm/Kitty terminal modes.

const MOD_SHIFT: u16 = 1;
const MOD_CTRL: u16 = 2;
const MOD_ALT: u16 = 4;
const MOD_SUPER: u16 = 8;

/// Converts Ghostty modifier bits into the ANSI/CSI-u modifier parameter (`1 + mask`).
fn csi_modifier(mods: u16) -> u16 {
    let shift = u16::from(mods & MOD_SHIFT != 0);
    let alt = if mods & MOD_ALT != 0 { 2 } else { 0 };
    let ctrl = if mods & MOD_CTRL != 0 { 4 } else { 0 };
    let sup = if mods & MOD_SUPER != 0 { 8 } else { 0 };
    1 + shift + alt + ctrl + sup
}

/// Maps `GhosttyKey` numeric constants used by `herdr-core/src/keys.rs` to their unshifted
/// Unicode codepoint and functional metadata.
enum KeyKind {
    Char(u32),
    FunctionalCsi { num: u16 },
    CsiFinal { final_byte: char, ss3: bool },
    Escape,
    Return,
    Backspace,
    Tab,
    Function1To4(char),
}

fn classify_key(key: i32, unshifted: u32) -> Option<KeyKind> {
    Some(match key {
        1 => KeyKind::Char(u32::from('`')),
        2 => KeyKind::Char(u32::from('\\')),
        3 => KeyKind::Char(u32::from('[')),
        4 => KeyKind::Char(u32::from(']')),
        5 => KeyKind::Char(u32::from(',')),
        6..=15 => KeyKind::Char(u32::from(b'0') + u32::try_from(key - 6).ok()?),
        16 => KeyKind::Char(u32::from('=')),
        20..=45 => KeyKind::Char(u32::from(b'a') + u32::try_from(key - 20).ok()?),
        46 => KeyKind::Char(u32::from('-')),
        47 => KeyKind::Char(u32::from('.')),
        48 => KeyKind::Char(u32::from('\'')),
        49 => KeyKind::Char(u32::from(';')),
        50 => KeyKind::Char(u32::from('/')),
        53 => KeyKind::Backspace,
        58 => KeyKind::Return,
        63 => KeyKind::Char(u32::from(' ')),
        64 => KeyKind::Tab,
        68 => KeyKind::FunctionalCsi { num: 3 },
        69 => KeyKind::CsiFinal {
            final_byte: 'F',
            ss3: true,
        },
        71 => KeyKind::CsiFinal {
            final_byte: 'H',
            ss3: true,
        },
        72 => KeyKind::FunctionalCsi { num: 2 },
        73 => KeyKind::FunctionalCsi { num: 6 },
        74 => KeyKind::FunctionalCsi { num: 5 },
        75 => KeyKind::CsiFinal {
            final_byte: 'B',
            ss3: true,
        },
        76 => KeyKind::CsiFinal {
            final_byte: 'D',
            ss3: true,
        },
        77 => KeyKind::CsiFinal {
            final_byte: 'C',
            ss3: true,
        },
        78 => KeyKind::CsiFinal {
            final_byte: 'A',
            ss3: true,
        },
        120 => KeyKind::Escape,
        121 => KeyKind::Function1To4('P'),
        122 => KeyKind::Function1To4('Q'),
        123 => KeyKind::Function1To4('R'),
        124 => KeyKind::Function1To4('S'),
        125 => KeyKind::FunctionalCsi { num: 15 },
        126 => KeyKind::FunctionalCsi { num: 17 },
        127 => KeyKind::FunctionalCsi { num: 18 },
        128 => KeyKind::FunctionalCsi { num: 19 },
        129 => KeyKind::FunctionalCsi { num: 20 },
        130 => KeyKind::FunctionalCsi { num: 21 },
        131 => KeyKind::FunctionalCsi { num: 23 },
        132 => KeyKind::FunctionalCsi { num: 24 },
        _ if unshifted >= 32 => KeyKind::Char(unshifted),
        _ => return None,
    })
}

fn csi_u(codepoint: u32, mods: u16) -> Vec<u8> {
    if mods == 0 {
        format!("\x1b[{codepoint}u").into_bytes()
    } else {
        format!("\x1b[{codepoint};{}u", csi_modifier(mods)).into_bytes()
    }
}

fn ctrl_byte(cp: u32) -> Option<u8> {
    match cp {
        0x20 => Some(0x00),
        0x40..=0x5f => u8::try_from(cp - 0x40).ok(),
        0x61..=0x7a => u8::try_from(cp - 0x60).ok(),
        _ => None,
    }
}

fn encode_legacy_char(cp: u32, mods: u16, text: &str) -> Vec<u8> {
    if mods & MOD_SUPER != 0 {
        return csi_u(cp, mods);
    }
    let ctrl = mods & MOD_CTRL != 0;
    let alt = mods & MOD_ALT != 0;
    let mut out = Vec::new();
    if ctrl {
        let Some(byte) = ctrl_byte(cp) else {
            return csi_u(cp, mods);
        };
        if alt {
            out.push(0x1b);
        }
        out.push(byte);
        return out;
    }
    if alt {
        out.push(0x1b);
        if !text.is_empty() {
            out.extend_from_slice(text.as_bytes());
        } else if let Some(ch) = char::from_u32(cp) {
            let ch = if mods & MOD_SHIFT != 0 {
                ch.to_ascii_uppercase()
            } else {
                ch
            };
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        return out;
    }
    if !text.is_empty() {
        out.extend_from_slice(text.as_bytes());
    } else if let Some(ch) = char::from_u32(cp) {
        let ch = if mods & MOD_SHIFT != 0 {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// Encodes one key event using the terminal's current DECCKM and Kitty keyboard flags.
pub(crate) fn encode_key(
    key: i32,
    mods: u16,
    text: &str,
    unshifted: u32,
    app_cursor: bool,
    kitty_flags: u8,
) -> Vec<u8> {
    let Some(kind) = classify_key(key, unshifted) else {
        return Vec::new();
    };
    let disambiguate = kitty_flags & 1 != 0;
    let report_all = kitty_flags & 8 != 0;
    let effective_cp = if unshifted != 0 {
        unshifted
    } else if let KeyKind::Char(cp) = kind {
        cp
    } else {
        0
    };

    match kind {
        KeyKind::FunctionalCsi { num } => {
            if mods == 0 {
                format!("\x1b[{num}~").into_bytes()
            } else {
                format!("\x1b[{num};{}~", csi_modifier(mods)).into_bytes()
            }
        }
        KeyKind::CsiFinal { final_byte, ss3 } => {
            if mods != 0 {
                format!("\x1b[1;{}{final_byte}", csi_modifier(mods)).into_bytes()
            } else if ss3 && app_cursor {
                format!("\x1bO{final_byte}").into_bytes()
            } else {
                format!("\x1b[{final_byte}").into_bytes()
            }
        }
        KeyKind::Function1To4(final_byte) => {
            if mods == 0 {
                format!("\x1bO{final_byte}").into_bytes()
            } else {
                format!("\x1b[1;{}{final_byte}", csi_modifier(mods)).into_bytes()
            }
        }
        KeyKind::Escape => {
            if disambiguate || report_all || mods != 0 {
                csi_u(27, mods)
            } else {
                vec![0x1b]
            }
        }
        KeyKind::Return => {
            if report_all || mods != 0 {
                if !disambiguate && mods == MOD_ALT {
                    vec![0x1b, b'\r']
                } else {
                    csi_u(13, mods)
                }
            } else {
                vec![b'\r']
            }
        }
        KeyKind::Backspace => {
            if report_all || (disambiguate && mods & !MOD_SHIFT != 0) {
                csi_u(127, mods)
            } else if mods & MOD_CTRL != 0 {
                if mods & MOD_ALT != 0 {
                    vec![0x1b, 0x08]
                } else {
                    vec![0x08]
                }
            } else if mods & MOD_ALT != 0 {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        KeyKind::Tab => {
            if report_all || (disambiguate && mods & !MOD_SHIFT != 0) {
                csi_u(9, mods)
            } else if mods & MOD_SHIFT != 0 {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        KeyKind::Char(cp) => {
            let code = if effective_cp != 0 { effective_cp } else { cp };
            let use_csi_u =
                report_all || (disambiguate && (mods & (MOD_CTRL | MOD_ALT | MOD_SUPER)) != 0);
            if use_csi_u {
                csi_u(code, mods)
            } else {
                encode_legacy_char(code, mods, text)
            }
        }
    }
}

/// Encodes one mouse-wheel notch at the center of `(cols, rows)` (1-based) if mouse tracking is active.
pub(crate) fn encode_wheel(
    up: bool,
    cols: u16,
    rows: u16,
    mouse_active: bool,
    sgr_1006: bool,
) -> Vec<u8> {
    if !mouse_active {
        return Vec::new();
    }
    let col = (cols / 2).saturating_add(1);
    let row = (rows / 2).saturating_add(1);
    let button = if up { 64u16 } else { 65u16 };
    if sgr_1006 {
        return format!("\x1b[<{button};{col};{row}M").into_bytes();
    }
    let cb = u8::try_from(button + 32).unwrap_or(b' ');
    let cx = u8::try_from(col.min(223) + 32).unwrap_or(255);
    let cy = u8::try_from(row.min(223) + 32).unwrap_or(255);
    vec![0x1b, b'[', b'M', cb, cx, cy]
}
