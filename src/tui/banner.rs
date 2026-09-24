//! The `zest serve` startup banner: a small blue-fire spirit drawn in 24-bit
//! color with half-block characters (two pixels per terminal cell).

use crate::tui::theme::{self, Rgb, CORE, DEEP_BLUE, DEEP_CYAN, ELECTRIC, FLAME, MUTED, SPARK};

const EYE: Rgb = (0x04, 0x10, 0x30);

/// 16x20 sprite. `.` is transparent.
const SPRITE: [&str; 20] = [
    ".......s........",
    "........B.....s.",
    ".......BB.......",
    "..s...BCB...B...",
    "......BCCB.BC...",
    ".....BCLCBBCB...",
    "....BCCLLCCCB...",
    "...BCCLWWLCCCB..",
    "..BCLWWWWWWLCB..",
    "..BCWWWWWWWWLCB.",
    ".BCLWsEWWWsEWCB.",
    ".BCLWEEWWWEEWCB.",
    ".BcCLWWWWWWWLCB.",
    ".BcCLWWWEEWWLCB.",
    "..BcCLLWWWLLCB..",
    "..bBcCCCCCCCBb..",
    "...bBBcCCCBBb...",
    "....bbBBBBbb....",
    "......bbbb......",
    "................",
];

fn color(px: u8) -> Option<Rgb> {
    match px {
        b'W' => Some(CORE),
        b'L' | b's' => Some(SPARK),
        b'C' => Some(ELECTRIC),
        b'c' => Some(DEEP_CYAN),
        b'B' => Some(FLAME),
        b'b' => Some(DEEP_BLUE),
        b'E' => Some(EYE),
        _ => None,
    }
}

/// Zesty's expression. `Happy` swaps the round eyes for `^ ^`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Normal,
    Happy,
}

const HAPPY_EYES: [&str; 2] = [".BCLWWEWWWEWWCB.", ".BCLWEWEWEWEWCB."];

fn sprite(mood: Mood) -> [&'static str; 20] {
    let mut s = SPRITE;
    if mood == Mood::Happy {
        s[10] = HAPPY_EYES[0];
        s[11] = HAPPY_EYES[1];
    }
    s
}

/// The sprite as 10 lines of ANSI text, each 16 cells wide.
pub fn mascot() -> Vec<String> {
    mascot_with(Mood::Normal)
}

pub fn mascot_with(mood: Mood) -> Vec<String> {
    sprite(mood)
        .chunks(2)
        .map(|pair| {
            let (top, bottom) = (pair[0].as_bytes(), pair[1].as_bytes());
            let mut line = String::new();
            for x in 0..top.len() {
                match (color(top[x]), color(bottom[x])) {
                    (None, None) => line.push(' '),
                    (Some(t), None) => line.push_str(&format!("{}▀{}", theme::fg(t), theme::RESET)),
                    (None, Some(b)) => line.push_str(&format!("{}▄{}", theme::fg(b), theme::RESET)),
                    (Some(t), Some(b)) => {
                        line.push_str(&format!("{}{}▀{}", theme::fg(t), theme::bg(b), theme::RESET))
                    }
                }
            }
            line
        })
        .collect()
}

pub struct BannerInfo<'a> {
    pub addr: String,
    pub mode: &'a str,
    pub anthropic: &'a str,
    pub openai: &'a str,
}

/// The full banner: mascot on the left, details on the right. Plain text when
/// color is off (pipes, `NO_COLOR`).
pub fn render(info: &BannerInfo) -> String {
    let label = |s: &str| theme::paint(&format!("{s:<10}"), MUTED);
    let url = |s: &str| theme::paint(s, SPARK);
    let details = [
        String::new(),
        format!("{}  {}", theme::gradient("zest", ELECTRIC, FLAME), theme::paint(concat!("v", env!("CARGO_PKG_VERSION")), MUTED)),
        theme::paint("blue-fire prompt compression", DEEP_CYAN),
        String::new(),
        format!("{}{}   {}", label("listening"), theme::bold(&format!("http://{}", info.addr), ELECTRIC), theme::paint(&format!("mode: {}", info.mode), FLAME)),
        format!("{}{}", label("anthropic"), url(info.anthropic)),
        format!("{}{}", label("openai"), url(info.openai)),
        String::new(),
        format!("{} ANTHROPIC_BASE_URL=http://{}", theme::paint("export", FLAME), info.addr),
        format!("{} OPENAI_BASE_URL=http://{}/v1", theme::paint("export", FLAME), info.addr),
    ];
    if !theme::enabled() {
        return details.iter().filter(|l| !l.is_empty()).cloned().collect::<Vec<_>>().join("\n");
    }
    let mut out = String::from("\n");
    for (art, text) in mascot().iter().zip(details.iter()) {
        out.push_str(&format!("  {art}   {text}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_is_rectangular_and_fully_mapped() {
        for row in sprite(Mood::Normal).iter().chain(sprite(Mood::Happy).iter()) {
            assert_eq!(row.len(), 16);
            assert!(row.bytes().all(|p| p == b'.' || color(p).is_some()), "unmapped pixel in {row}");
        }
        assert_eq!(mascot().len(), 10);
    }
}
