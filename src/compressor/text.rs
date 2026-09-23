//! Tier 1: deterministic noise cleanup for prose and terminal output.
//!
//! Nothing in here is ever applied to text that the engine has classified as
//! source code — see `engine.rs` for the routing.

use regex::Regex;
use std::sync::LazyLock;

/// CSI / OSC / two-byte escape sequences emitted by terminals.
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap()
});

/// C0 control characters other than `\t`, `\n`, `\r`, plus DEL.
static CTRL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").unwrap());

/// Inline code spans and double-quoted strings are never touched by filler removal.
static PROTECTED_SPAN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`[^`\n]*`|"[^"\n]*""#).unwrap());

static INLINE_SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]{2,}").unwrap());

/// Filler patterns: (regex, replacement). Replacements may reference capture group 1
/// to keep the sentence boundary that preceded the removed phrase.
static FILLER: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        // Greeting on its own, terminated by punctuation or end of line: "Hello Claude," / "Hi there!"
        (
            r"(?im)^([ \t]*)(?:hi|hello|hey|greetings|good (?:morning|afternoon|evening))(?:[ \t]+(?:there|claude|chatgpt|gpt|assistant|team|all|everyone|folks))?(?:[ \t]*[,!.]+[ \t]*|[ \t]*$)",
            "$1",
        ),
        // "I would really appreciate it if you could ..."
        (r"(?i)\bI(?: would|'d) (?:really |greatly |very much )?appreciate (?:it )?if you (?:could|can|would)\s+", ""),
        // "I want you to ..." / "I'd like you to ..."
        (r"(?i)\bI(?: want| need| would like|'d like) you to\s+", ""),
        // Sentence-initial "Could you (please) ..."
        (r"(?im)(^[ \t]*|[.!?][ \t]+)(?:could|can|would|will) you (?:please |kindly )?", "$1"),
        // Mid-sentence "... could you please ..."
        (r"(?i)\b(?:could|can|would|will) you (?:please|kindly)\s+", ""),
        // "read this carefully and ..."
        (
            r"(?i)\b(?:(?:carefully|thoroughly) )?(?:read|look (?:at|over|through)|go (?:over|through)|review) (?:this|the following|the code below|everything below)(?: (?:carefully|thoroughly))? and\s+",
            "",
        ),
        (r"(?i)\b(?:please|kindly)\b,?\s+", ""),
        // Trailing gratitude: "Thanks in advance!" at end of line.
        (r"(?im)(^|[.!?])[ \t]*(?:thanks|thank you)(?: (?:so|very) much| in advance| a lot)?[.!]*[ \t]*$", "$1"),
    ]
    .into_iter()
    .map(|(p, r)| (Regex::new(p).unwrap(), r))
    .collect()
});

/// Strip ANSI escapes and control characters, and resolve carriage-return
/// overwrites the way a terminal would (progress bars keep only their final frame).
pub fn strip_terminal_noise(s: &str) -> String {
    let s = ANSI_RE.replace_all(s, "");
    let s = CTRL_RE.replace_all(&s, "");
    if !s.contains('\r') {
        return s.into_owned();
    }
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        // After a bare `\r` the terminal redraws the line: keep the last non-empty frame.
        let frame = line.rsplit('\r').find(|f| !f.trim().is_empty()).unwrap_or("");
        out.push_str(frame);
    }
    out
}

/// Trim trailing whitespace and collapse runs of blank lines to a single blank line.
/// Leading indentation is preserved (it carries structure in Markdown and YAML).
pub fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.pop(); // `split` yields one more piece than there are newlines
    out
}

/// Collapse runs of inner spaces/tabs to one space, keeping leading indentation.
pub fn collapse_inline_spaces(s: &str) -> String {
    s.split('\n')
        .map(|line| {
            let body_start = line.len() - line.trim_start().len();
            let (indent, body) = line.split_at(body_start);
            format!("{indent}{}", INLINE_SPACES_RE.replace_all(body, " "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove conversational filler from human-written prose. Inline code, quoted
/// strings and shell-looking lines are left untouched.
pub fn remove_filler(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let t = line.trim_start();
        if t.starts_with('$') || t.starts_with('>') || t.starts_with("#!") {
            out.push_str(line);
            continue;
        }
        let mut last = 0;
        for m in PROTECTED_SPAN_RE.find_iter(line) {
            out.push_str(&strip_filler(&line[last..m.start()]));
            out.push_str(m.as_str());
            last = m.end();
        }
        out.push_str(&strip_filler(&line[last..]));
    }
    out
}

fn strip_filler(s: &str) -> String {
    let mut cur = s.to_string();
    for (re, rep) in FILLER.iter() {
        if re.is_match(&cur) {
            cur = re.replace_all(&cur, *rep).into_owned();
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_and_progress_frames() {
        let s = "\x1b[32mPASS\x1b[0m ok\nDownloading 10%\rDownloading 55%\rDownloading 100%\n";
        assert_eq!(strip_terminal_noise(s), "PASS ok\nDownloading 100%\n");
    }

    #[test]
    fn collapses_blank_lines_and_trailing_space() {
        assert_eq!(normalize_whitespace("a  \n\n\n\n  b\t\n"), "a\n\n  b\n");
    }

    #[test]
    fn removes_filler_but_not_quotes_or_code() {
        let s = "Hello Claude,\nCould you please read this carefully and fix the bug in `please_do()`?\nPrint \"please wait\" too. Thanks in advance!";
        let out = remove_filler(s);
        assert_eq!(out, "\nfix the bug in `please_do()`?\nPrint \"please wait\" too.");
    }

    #[test]
    fn greeting_needs_punctuation() {
        assert_eq!(remove_filler("Hello world program in Rust"), "Hello world program in Rust");
    }
}
