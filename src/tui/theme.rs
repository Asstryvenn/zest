//! The electric-blue palette shared by the banner, the per-request readout and
//! `--help`. Colors are 24-bit and only emitted when stderr is a terminal and
//! `NO_COLOR` is unset.

use std::io::IsTerminal;
use std::sync::OnceLock;

pub type Rgb = (u8, u8, u8);

/// `#00F0FF`: the signature color.
pub const ELECTRIC: Rgb = (0x00, 0xF0, 0xFF);
pub const SPARK: Rgb = (0x7D, 0xF9, 0xFF);
pub const CORE: Rgb = (0xE8, 0xFF, 0xFF);
pub const DEEP_CYAN: Rgb = (0x00, 0xA8, 0xC8);
pub const FLAME: Rgb = (0x1E, 0x6B, 0xFF);
pub const DEEP_BLUE: Rgb = (0x0A, 0x2A, 0x8C);
/// Secondary text: a desaturated steel blue that stays readable on dark and light backgrounds.
pub const MUTED: Rgb = (0x6B, 0x8B, 0xA4);

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map_or(true, |t| t != "dumb")
    })
}

pub fn fg((r, g, b): Rgb) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

pub fn bg((r, g, b): Rgb) -> String {
    format!("\x1b[48;2;{r};{g};{b}m")
}

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";

/// `text` in `color`, or unchanged when color is off.
pub fn paint(text: &str, color: Rgb) -> String {
    if enabled() {
        format!("{}{text}{RESET}", fg(color))
    } else {
        text.to_string()
    }
}

pub fn bold(text: &str, color: Rgb) -> String {
    if enabled() {
        format!("{BOLD}{}{text}{RESET}", fg(color))
    } else {
        text.to_string()
    }
}

/// Bold text with a per-character gradient from `from` to `to`.
pub fn gradient(text: &str, from: Rgb, to: Rgb) -> String {
    if !enabled() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(2) - 1;
    let lerp = |a: u8, b: u8, t: f32| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    let mut out = String::from(BOLD);
    for (i, ch) in chars.into_iter().enumerate() {
        let t = i as f32 / n as f32;
        out.push_str(&fg((lerp(from.0, to.0, t), lerp(from.1, to.1, t), lerp(from.2, to.2, t))));
        out.push(ch);
    }
    out.push_str(RESET);
    out
}

/// The `[zest]` prefix used on every status line.
pub fn tag() -> String {
    format!("{}{}{}", paint("[", MUTED), bold("zest", ELECTRIC), paint("]", MUTED))
}
