//! The compression pipeline manager.
//!
//! A text block is split into fenced code blocks and plain text. Each piece is
//! routed through the tiers allowed by the mode:
//!
//! | piece                               | safe        | balanced                     | max                    |
//! |-------------------------------------|-------------|------------------------------|------------------------|
//! | prose / terminal output             | Tier 1      | Tier 1 + filler + Tier 3     | + similar-line folding |
//! | fenced py/ts/js code                | untouched   | Tier 2 if the message targets specific code | Tier 2 |
//! | unfenced code (tool output)         | untouched   | untouched                    | Tier 2                 |
//! | any other fenced language (sh, sql, json, ...) | untouched | untouched          | untouched              |
//!
//! Compression is a pure function of (mode, block kind, message focus, text).
//! It never depends on *later* messages, so the rewritten conversation prefix is
//! byte-identical from one request to the next. That keeps provider prompt
//! caches warm and keeps signed thinking blocks valid.

use crate::compressor::{ast, logs, text};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use xxhash_rust::xxh3::Xxh3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Tier 1 only: ANSI/control stripping, whitespace, identical-line dedup.
    Safe,
    /// Tier 1 + filler removal, log/traceback pruning, AST reduction of non-targeted code.
    Balanced,
    /// Everything, including AST reduction of unfenced tool output and similar-line folding.
    Max,
}

/// Where a text block came from. Filler removal only applies to human prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// User or system prompt text.
    Human,
    /// Tool results / command output / piped input.
    Tool,
    /// Prior model output (only touched in max mode, and never for Anthropic).
    Assistant,
}

/// What the surrounding message is about. Functions and files named here are
/// never collapsed.
#[derive(Clone, Debug, Default)]
pub struct Focus {
    pub identifiers: HashSet<String>,
    pub files: HashSet<String>,
    /// The prose names specific code (backticks, snake_case, calls, file names).
    pub has_target: bool,
    pub keep_verbose_logs: bool,
    fingerprint: u64,
}

impl Focus {
    fn finish(mut self) -> Self {
        let mut ids: Vec<&String> = self.identifiers.iter().chain(self.files.iter()).collect();
        ids.sort();
        let mut h = Xxh3::new();
        for id in ids {
            h.update(id.as_bytes());
            h.update(&[0]);
        }
        h.update(&[self.has_target as u8, self.keep_verbose_logs as u8]);
        self.fingerprint = h.digest();
        self
    }
}

static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_$][A-Za-z0-9_$]{2,}").unwrap());
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[\w./\\-]*?([\w-]+\.(?:py|pyi|ts|tsx|js|jsx|mjs|cjs))\b").unwrap()
});
static TARGET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"`[^`\n]+`|\b[a-z][a-z0-9]*_[a-z0-9_]+\b|\b[a-z]+[A-Z]\w*\b|\b[A-Z][a-z0-9]+[A-Z]\w*\b|\b\w+\(").unwrap()
});
static VERBOSE_REQUEST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:debug|info|verbose|trace)[- ](?:logs?|lines?|output|messages?|level)\b|\bkeep (?:the )?(?:debug|info)\b").unwrap()
});
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^( {0,3})(`{3,}|~{3,})\s*([^`\s]*)(.*)$").unwrap());
static NUMBERED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\s*\d+)(\t|→|: |\| )").unwrap());
static CODEISH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)[;{}]\s*$ | ^\s*[}\])]
        | ^\s*(?:fn|func|def|class|import|package|use|pub|let|const|var|return|struct|impl|enum|interface|type|export|\#include|package|public|private|protected|static|async|await|if\s*\(|for\s*\(|while\s*\()\b
        | ^\s{4,}\S.*[=(]",
    )
    .unwrap()
});

/// Fenced-block languages whose contents are terminal output, not code.
fn is_output_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "" | "text" | "txt" | "log" | "logs" | "console" | "output" | "stdout" | "stderr" | "plaintext" | "traceback" | "pytb"
    )
}

enum Segment<'a> {
    Text(&'a str),
    Fence { open: &'a str, tag: &'a str, info: &'a str, body: &'a str, close: &'a str },
}

/// Split text into plain runs and fenced code blocks (``` or ~~~).
fn segment(s: &str) -> Vec<Segment<'_>> {
    let mut segs = Vec::new();
    let mut text_start = 0;
    let mut pos = 0;
    let lines: Vec<&str> = s.split_inclusive('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let bare = line.trim_end_matches(['\n', '\r']);
        if let Some(c) = FENCE_RE.captures(bare) {
            let fence = c.get(2).unwrap().as_str();
            let (fch, flen) = (fence.as_bytes()[0] as char, fence.len());
            let open_start = pos;
            let body_start = pos + line.len();
            let mut j = i + 1;
            let mut p = body_start;
            let mut close_at = None;
            while j < lines.len() {
                let t = lines[j].trim_end_matches(['\n', '\r']);
                let tt = t.trim_start_matches(' ');
                if t.len() - tt.len() <= 3 && tt.len() >= flen && tt.trim_end().chars().all(|ch| ch == fch) && !tt.trim_end().is_empty() {
                    close_at = Some((p, j));
                    break;
                }
                p += lines[j].len();
                j += 1;
            }
            if text_start < open_start {
                segs.push(Segment::Text(&s[text_start..open_start]));
            }
            let (body_end, close, next_i) = match close_at {
                Some((p, j)) => (p, &s[p..p + lines[j].len()], j + 1),
                None => (s.len(), "", lines.len()),
            };
            segs.push(Segment::Fence {
                open: &s[open_start..body_start],
                tag: c.get(3).unwrap().as_str(),
                info: c.get(4).unwrap().as_str(),
                body: &s[body_start..body_end],
                close,
            });
            pos = body_end + close.len();
            text_start = pos;
            i = next_i;
            continue;
        }
        pos += line.len();
        i += 1;
    }
    if text_start < s.len() {
        segs.push(Segment::Text(&s[text_start..]));
    }
    segs
}

/// `cat -n` / Read-tool style numbered listing.
fn is_numbered_listing(s: &str) -> bool {
    let non_empty: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    non_empty.len() >= 3 && non_empty.iter().filter(|l| NUMBERED_RE.is_match(l)).count() * 10 >= non_empty.len() * 8
}

/// Conservative "this is source code of some language" check for unfenced text.
fn looks_like_code(s: &str) -> bool {
    if is_numbered_listing(s) {
        return true;
    }
    let non_empty: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    if non_empty.len() < 3 || logs::is_log_like(s) || s.contains("Traceback (most recent call last)") {
        return false;
    }
    let t = s.trim_start();
    if (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return true;
    }
    let codeish = non_empty.iter().filter(|l| CODEISH_RE.is_match(l)).count();
    codeish * 10 >= non_empty.len() * 4 || ast::detect_lang(s).is_some()
}

/// The compression pipeline. Cheap to share: results are memoised by content hash.
pub struct CompressorEngine {
    pub mode: Mode,
    pub keep_verbose_logs: bool,
    cache: Mutex<HashMap<u128, Arc<str>>>,
}

const CACHE_CAPACITY: usize = 8192;

impl CompressorEngine {
    pub fn new(mode: Mode) -> Self {
        Self { mode, keep_verbose_logs: false, cache: Mutex::new(HashMap::new()) }
    }

    pub fn with_verbose_logs(mut self, keep: bool) -> Self {
        self.keep_verbose_logs = keep;
        self
    }

    /// Build the focus for one message from all of its text blocks. Only prose and
    /// terminal output contribute; code does not name itself as a target.
    pub fn focus_for<'a>(&self, texts: impl IntoIterator<Item = &'a str>) -> Focus {
        let mut f = Focus { keep_verbose_logs: self.keep_verbose_logs, ..Default::default() };
        for t in texts {
            for seg in segment(t) {
                let s = match seg {
                    Segment::Text(s) if !looks_like_code(s) => s,
                    Segment::Fence { tag, body, .. } if is_output_tag(tag) && !looks_like_code(body) => body,
                    _ => continue,
                };
                for m in WORD_RE.find_iter(s) {
                    f.identifiers.insert(m.as_str().to_string());
                }
                for c in FILE_RE.captures_iter(s) {
                    f.files.insert(c[1].to_ascii_lowercase());
                }
                f.has_target |= TARGET_RE.is_match(s) || !f.files.is_empty();
                f.keep_verbose_logs |= VERBOSE_REQUEST_RE.is_match(s);
            }
        }
        f.finish()
    }

    /// Compress one text block. Deterministic for identical inputs.
    pub fn compress(&self, input: &str, kind: BlockKind, focus: &Focus) -> Arc<str> {
        let mut h = Xxh3::new();
        h.update(&[self.mode as u8, kind as u8]);
        h.update(&focus.fingerprint.to_le_bytes());
        h.update(input.as_bytes());
        let key = h.digest128();
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let mut out = self.compress_uncached(input, kind, focus);
        // Never grow a block, and never empty one out: a message that was only
        // "Hi!" must still say something, or the API rejects the empty block.
        if out.len() >= input.len() || (out.trim().is_empty() && !input.trim().is_empty()) {
            out = input.to_string();
        }
        let out: Arc<str> = out.into();
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(key, out.clone());
        out
    }

    fn compress_uncached(&self, input: &str, kind: BlockKind, focus: &Focus) -> String {
        let mut out = String::with_capacity(input.len());
        let mut prev_line = "";
        for seg in segment(input) {
            match seg {
                Segment::Text(s) => {
                    if looks_like_code(s) {
                        out.push_str(&self.code(s, None, None, kind, focus, false));
                    } else {
                        out.push_str(&self.prose(s, kind, focus));
                    }
                    prev_line = s.trim_end().rsplit('\n').next().unwrap_or("");
                }
                Segment::Fence { open, tag, info, body, close } => {
                    out.push_str(open);
                    if is_output_tag(tag) && !looks_like_code(body) {
                        let terminal = self.prose(body, BlockKind::Tool, focus);
                        out.push_str(&terminal);
                        if !terminal.is_empty() && !terminal.ends_with('\n') && !close.is_empty() {
                            out.push('\n');
                        }
                    } else {
                        let file = FILE_RE
                            .captures(info)
                            .or_else(|| FILE_RE.captures(prev_line))
                            .map(|c| c[1].to_ascii_lowercase());
                        let lang = ast::Lang::from_tag(tag).or_else(|| file.as_deref().and_then(ast::Lang::from_filename));
                        out.push_str(&self.code(body, lang, file.as_deref(), kind, focus, true));
                    }
                    out.push_str(close);
                    prev_line = "";
                }
            }
        }
        out
    }

    /// Tier 1 + Tier 3 for prose and terminal output.
    fn prose(&self, s: &str, kind: BlockKind, focus: &Focus) -> String {
        let trailing_nl = s.ends_with('\n');
        let cleaned = text::strip_terminal_noise(s);
        let mut lines: Vec<String> = cleaned.split('\n').map(str::to_string).collect();
        if trailing_nl {
            lines.pop();
        }
        let log_like = logs::is_log_like(&cleaned);
        if self.mode >= Mode::Balanced {
            lines = logs::prune_tracebacks(lines, self.mode);
            if log_like && !focus.keep_verbose_logs {
                lines = logs::drop_verbose_levels(lines);
            }
            lines = logs::collapse_passing(lines);
        }
        let similar = self.mode == Mode::Max || (self.mode == Mode::Balanced && log_like);
        lines = logs::dedupe(lines, similar);
        if self.mode == Mode::Max && log_like {
            lines = logs::drop_global_repeats(lines);
        }
        let mut joined = lines.join("\n");
        if self.mode >= Mode::Balanced && kind == BlockKind::Human {
            joined = text::remove_filler(&joined);
        }
        let mut joined = text::normalize_whitespace(&joined);
        if self.mode == Mode::Max {
            joined = text::collapse_inline_spaces(&joined);
        }
        if trailing_nl {
            joined.push('\n');
        }
        joined
    }

    /// Tier 2 for code. Returns the input unchanged whenever policy forbids a
    /// collapse or the collapse would not re-parse.
    fn code(&self, s: &str, lang: Option<ast::Lang>, file: Option<&str>, kind: BlockKind, focus: &Focus, fenced: bool) -> String {
        let allowed = match self.mode {
            Mode::Safe => false,
            Mode::Balanced => fenced && kind == BlockKind::Human && focus.has_target,
            Mode::Max => true,
        };
        if !allowed {
            return s.to_string();
        }
        let numbered = is_numbered_listing(s);
        let lines: Vec<&str> = s.split('\n').collect();
        let stripped: Vec<&str> = if numbered {
            lines.iter().map(|l| NUMBERED_RE.find(l).map_or(*l, |m| &l[m.end()..])).collect()
        } else {
            lines.clone()
        };
        let code = stripped.join("\n");
        let Some(lang) = lang.or_else(|| ast::detect_lang(&code)) else {
            return s.to_string();
        };
        let keep = |name: &str| focus.identifiers.contains(name);
        let min_rows = if self.mode == Mode::Max { 2 } else { 3 };
        let Some(plan) = ast::plan(&code, lang, &keep, min_rows) else {
            return s.to_string();
        };
        // A file the user named, with no specific function named in it, is the edit target.
        let file_targeted = file.is_some_and(|f| focus.files.contains(f));
        let fn_targeted = plan.defined.iter().any(|d| focus.identifiers.contains(d));
        if plan.collapses.is_empty() || (file_targeted && !fn_targeted) {
            return s.to_string();
        }
        let mut out = ast::apply(&lines, &plan, |first_hidden| {
            if numbered {
                NUMBERED_RE
                    .captures(first_hidden)
                    .map(|c| format!("{}{}", " ".repeat(c[1].len()), &c[2]))
                    .unwrap_or_default()
            } else {
                String::new()
            }
        });
        out.pop(); // `apply` terminates every line; `lines` came from split('\n')
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_fences() {
        let s = "intro\n```py\nx = 1\n```\noutro\n";
        let segs = segment(s);
        assert_eq!(segs.len(), 3);
        let rebuilt: String = segs
            .iter()
            .map(|s| match s {
                Segment::Text(t) => t.to_string(),
                Segment::Fence { open, body, close, .. } => format!("{open}{body}{close}"),
            })
            .collect();
        assert_eq!(rebuilt, s);
    }

    #[test]
    fn shell_and_sql_fences_are_untouched() {
        let e = CompressorEngine::new(Mode::Max);
        let s = "Please run this:\n```bash\necho   \"hello\"\n\n\n\necho   \"hello\"\n```\n```sql\nSELECT  'a    b' FROM t;\n```\n";
        let out = e.compress(s, BlockKind::Human, &e.focus_for([s]));
        assert!(out.contains("```bash\necho   \"hello\"\n\n\n\necho   \"hello\"\n```"));
        assert!(out.contains("SELECT  'a    b' FROM t;"));
    }

    #[test]
    fn numbered_listing_keeps_line_numbers() {
        let src = "     1\tdef a():\n     2\t    x = 1\n     3\t    y = 2\n     4\t    return x + y\n     5\t\n     6\tdef b():\n     7\t    return 3\n";
        let e = CompressorEngine::new(Mode::Max);
        let out = e.compress(src, BlockKind::Tool, &Focus::default());
        assert_eq!(out.as_ref(), "     1\tdef a():\n      \t    ...  # zest: 3 lines hidden\n     5\t\n     6\tdef b():\n     7\t    return 3\n");
    }

    #[test]
    fn never_empties_a_message() {
        let e = CompressorEngine::new(Mode::Max);
        for greeting in ["hi", "Hello!", "Thanks in advance!", "  hey there,  "] {
            let out = e.compress(greeting, BlockKind::Human, &e.focus_for([greeting]));
            assert_eq!(out.as_ref(), greeting);
        }
    }

    #[test]
    fn is_deterministic() {
        let e = CompressorEngine::new(Mode::Balanced);
        let s = "INFO a\nINFO b\nINFO c\nWARN d\nERROR e\nINFO f\n";
        let f = e.focus_for([s]);
        assert_eq!(e.compress(s, BlockKind::Tool, &f), e.compress(s, BlockKind::Tool, &f));
    }
}
