//! Tier 1 (dedup) and Tier 3 (log level + traceback pruning) for terminal output.

use crate::compressor::engine::Mode;
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

static PY_FRAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*File "([^"]+)", line \d+"#).unwrap());
static STACK_AT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+at\s+\S").unwrap());
static STACK_MORE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+\.\.\. \d+ more\s*$").unwrap());
static CARET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[\^~]+\s*$").unwrap());
static DIGITS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

static LEVEL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        \[\s*(?i:(TRACE|DEBUG|INFO|NOTICE|VERBOSE|WARN|WARNING|ERROR|FATAL|CRITICAL))\s*\]
        | \b(TRACE|DEBUG|INFO|NOTICE|VERBOSE|WARN|WARNING|ERROR|FATAL|CRITICAL)\b[:\s|]
        | \blevel=(?i:(trace|debug|info|notice|warn|warning|error|fatal|critical))\b
        | "level"\s*:\s*"(?i:(trace|debug|info|notice|warn|warning|error|fatal|critical))"
        | ^(trace|debug|info|warn|error):\s
        "#,
    )
    .unwrap()
});

/// Verbose lines mentioning these are kept even when their level would drop them.
static ALARM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error|exception|fail|fatal|panic|traceback|denied|refused|timeout").unwrap());

static PASS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:✓|✔|√)\s|^\s*(?:PASS|PASSED|ok)\b|\sPASSED(?:\s+\[\s*\d+%\])?\s*$|\s\.\.\.\s+ok\s*$|^\s*\[\s*OK\s*\]",
    )
    .unwrap()
});

const LEVEL_SCAN_BYTES: usize = 96;

fn marker(msg: &str) -> String {
    format!("[ZEST: {msg}]")
}

fn leading_ws(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// The log level of a line, if it looks like a structured log line.
pub fn line_level(line: &str) -> Option<String> {
    let mut end = line.len().min(LEVEL_SCAN_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let caps = LEVEL_RE.captures(&line[..end])?;
    let lvl = caps.iter().skip(1).flatten().next()?.as_str().to_ascii_uppercase();
    Some(lvl)
}

/// True when at least 5 lines and 30% of the non-empty lines carry a log level.
pub fn is_log_like(text: &str) -> bool {
    let mut total = 0usize;
    let mut tagged = 0usize;
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        total += 1;
        if line_level(l).is_some() {
            tagged += 1;
        }
    }
    tagged >= 5 && tagged * 10 >= total * 3
}

fn is_verbose_level(lvl: &str) -> bool {
    matches!(lvl, "TRACE" | "DEBUG" | "INFO" | "NOTICE" | "VERBOSE")
}

/// Drop TRACE/DEBUG/INFO lines, replacing each dropped run with one marker.
pub fn drop_verbose_levels(lines: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    for line in lines {
        let drop = line_level(&line).is_some_and(|l| is_verbose_level(&l)) && !ALARM_RE.is_match(&line);
        if drop {
            dropped += 1;
            continue;
        }
        if dropped > 0 {
            out.push(marker(&format!("{dropped} INFO/DEBUG lines hidden")));
            dropped = 0;
        }
        out.push(line);
    }
    if dropped > 0 {
        out.push(marker(&format!("{dropped} INFO/DEBUG lines hidden")));
    }
    out
}

/// Replace runs of 3+ passing-test lines with a single count.
pub fn collapse_passing(lines: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut run: Vec<String> = Vec::new();
    let flush = |run: &mut Vec<String>, out: &mut Vec<String>| {
        if run.len() >= 3 {
            out.push(marker(&format!("{} passing test lines hidden", run.len())));
            run.clear();
        } else {
            out.append(run);
        }
    };
    for line in lines {
        if PASS_RE.is_match(&line) {
            run.push(line);
        } else {
            flush(&mut run, &mut out);
            out.push(line);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Collapse runs of identical lines (all modes); in `similar` mode also collapse
/// runs of 4+ lines that differ only in their digits (timestamps, counters, ids).
pub fn dedupe(lines: Vec<String>, similar: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let cur = &lines[i];
        if cur.trim().is_empty() {
            out.push(cur.clone());
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < lines.len() && lines[j] == *cur {
            j += 1;
        }
        if j - i >= 3 {
            out.push(cur.clone());
            out.push(marker(&format!("previous line repeated {} more times", j - i - 1)));
            i = j;
            continue;
        }
        if similar {
            let key = DIGITS_RE.replace_all(cur, "0");
            let mut k = i + 1;
            while k < lines.len() && DIGITS_RE.replace_all(&lines[k], "0") == key {
                k += 1;
            }
            if k - i >= 4 {
                out.push(cur.clone());
                out.push(marker(&format!("{} similar lines hidden", k - i - 2)));
                out.push(lines[k - 1].clone());
                i = k;
                continue;
            }
        }
        out.push(cur.clone());
        i += 1;
    }
    out
}

/// Drop long lines already seen earlier in the text (max mode, logs only).
pub fn drop_global_repeats(lines: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(lines.len());
    let mut dropped = 0usize;
    for line in lines {
        if line.trim().len() >= 30 && !line.starts_with("[ZEST") && !seen.insert(line.clone()) {
            dropped += 1;
            continue;
        }
        if dropped > 0 {
            out.push(marker(&format!("{dropped} repeated lines hidden")));
            dropped = 0;
        }
        out.push(line);
    }
    if dropped > 0 {
        out.push(marker(&format!("{dropped} repeated lines hidden")));
    }
    out
}

fn is_library_path(p: &str) -> bool {
    const LIB: &[&str] = &[
        "site-packages", "dist-packages", "/lib/python", "\\lib\\", "<frozen", "importlib", "runpy.py",
        "node_modules", "node:internal", "internal/", "(native)", "<anonymous>",
        " java.", " javax.", " sun.", " jdk.", " kotlin.", " kotlinx.", " scala.", " org.junit.",
        " org.springframework.", " org.apache.", " io.netty.", " reactor.", "java.base/",
    ];
    LIB.iter().any(|m| p.contains(m))
}

/// Decide which frames to keep. `from_end`: Python prints the innermost frame
/// last; JS/Java print it first.
fn select_frames(libs: &[bool], mode: Mode, from_end: bool) -> Vec<bool> {
    let n = libs.len();
    let k = if mode == Mode::Max { 3 } else { 5 };
    let mut keep = vec![false; n];
    let mut user_budget = if mode == Mode::Max { 3 } else { usize::MAX };
    let order: Vec<usize> = if from_end { (0..n).rev().collect() } else { (0..n).collect() };
    for (rank, &idx) in order.iter().enumerate() {
        if rank < k {
            keep[idx] = true;
        } else if !libs[idx] && user_budget > 0 {
            keep[idx] = true;
            user_budget -= 1;
        }
    }
    keep
}

fn emit_frames(frames: Vec<Vec<String>>, keep: &[bool], indent: &str, mode: Mode, out: &mut Vec<String>) {
    let mut hidden = 0usize;
    for (frame, &k) in frames.into_iter().zip(keep) {
        if !k {
            hidden += 1;
            continue;
        }
        if hidden > 0 {
            out.push(format!("{indent}{}", marker(&format!("{hidden} framework frames hidden"))));
            hidden = 0;
        }
        for l in frame {
            if mode == Mode::Max && CARET_RE.is_match(&l) {
                continue;
            }
            out.push(l);
        }
    }
    if hidden > 0 {
        out.push(format!("{indent}{}", marker(&format!("{hidden} framework frames hidden"))));
    }
}

/// Prune Python tracebacks and JS/Java `at ...` stack traces. The exception type,
/// the message, the innermost frames and all user-code frames are kept.
pub fn prune_tracebacks(lines: Vec<String>, mode: Mode) -> Vec<String> {
    let k = if mode == Mode::Max { 3 } else { 5 };
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        // Python: frames are `File "...", line N, in f` + indented source/caret lines.
        if lines[i].trim_start().starts_with("Traceback (most recent call last)") {
            out.push(lines[i].clone());
            i += 1;
            let mut frames: Vec<Vec<String>> = Vec::new();
            let mut libs = Vec::new();
            let mut indent = String::from("  ");
            while i < lines.len() {
                let line = &lines[i];
                if let Some(c) = PY_FRAME_RE.captures(line) {
                    indent = line[..leading_ws(line)].to_string();
                    libs.push(is_library_path(&c[1]));
                    frames.push(vec![line.clone()]);
                } else if !frames.is_empty()
                    && !line.trim().is_empty()
                    && (leading_ws(line) > indent.len() || line.trim_start().starts_with("[Previous line repeated"))
                {
                    frames.last_mut().unwrap().push(line.clone());
                } else {
                    break;
                }
                i += 1;
            }
            if frames.len() > k + 1 {
                let keep = select_frames(&libs, mode, true);
                emit_frames(frames, &keep, &indent, mode, &mut out);
            } else {
                frames.into_iter().flatten().for_each(|l| out.push(l));
            }
            continue;
        }
        // JS / Java / Kotlin: a contiguous run of `    at ...` lines.
        if STACK_AT_RE.is_match(&lines[i]) {
            let indent = lines[i][..leading_ws(&lines[i])].to_string();
            let mut frames = Vec::new();
            let mut libs = Vec::new();
            while i < lines.len() && (STACK_AT_RE.is_match(&lines[i]) || STACK_MORE_RE.is_match(&lines[i])) {
                libs.push(STACK_AT_RE.is_match(&lines[i]) && is_library_path(&format!(" {}", lines[i].trim())));
                frames.push(vec![lines[i].clone()]);
                i += 1;
            }
            if frames.len() > k + 1 {
                let keep = select_frames(&libs, mode, false);
                emit_frames(frames, &keep, &indent, mode, &mut out);
            } else {
                frames.into_iter().flatten().for_each(|l| out.push(l));
            }
            continue;
        }
        out.push(lines[i].clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn dedupes_identical_runs() {
        let lines = vec!["PASS test_auth.py".to_string(); 500];
        let out = dedupe(lines, false);
        assert_eq!(out, vec!["PASS test_auth.py", "[ZEST: previous line repeated 499 more times]"]);
    }

    #[test]
    fn detects_levels() {
        assert_eq!(line_level("2024-01-01 12:00:00 INFO app: started").as_deref(), Some("INFO"));
        assert_eq!(line_level("[debug] cache warm").as_deref(), Some("DEBUG"));
        assert_eq!(line_level(r#"{"level":"info","msg":"x"}"#).as_deref(), Some("INFO"));
        assert_eq!(line_level("For more info see the docs"), None);
    }

    #[test]
    fn keeps_alarming_info_lines() {
        let out = drop_verbose_levels(v("INFO a\nINFO b\nINFO request failed: timeout\nERROR boom"));
        assert_eq!(out, v("[ZEST: 2 INFO/DEBUG lines hidden]\nINFO request failed: timeout\nERROR boom"));
    }

    #[test]
    fn prunes_python_framework_frames() {
        let mut s = String::from("Traceback (most recent call last):\n  File \"/app/main.py\", line 3, in <module>\n    run()\n");
        for n in 0..10 {
            s.push_str(&format!("  File \"/venv/lib/python3.12/site-packages/django/x{n}.py\", line 1, in f{n}\n    call()\n"));
        }
        s.push_str("  File \"/app/views.py\", line 9, in handler\n    1/0\nZeroDivisionError: division by zero");
        let out = prune_tracebacks(v(&s), Mode::Balanced).join("\n");
        assert!(out.contains("/app/main.py"));
        assert!(out.contains("/app/views.py"));
        assert!(out.contains("ZeroDivisionError: division by zero"));
        assert!(out.contains("framework frames hidden"));
        assert!(!out.contains("x0.py"));
    }
}
