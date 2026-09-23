//! Tier 2: Tree-sitter based code reduction.
//!
//! Function bodies are collapsed by whole source *rows*, so a collapse can never
//! cut through a string literal: every removed row lies strictly inside a
//! function body node. Signatures, classes, imports, types and interfaces are
//! kept verbatim. The result is re-parsed; if it no longer parses cleanly the
//! collapse is abandoned (Tier 4 fallback).

use regex::Regex;
use std::cell::RefCell;
use std::sync::LazyLock;
use tree_sitter::{Language, Node, Parser, Tree};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Python,
    TypeScript,
    Tsx,
}

impl Lang {
    pub fn from_tag(tag: &str) -> Option<Lang> {
        match tag.to_ascii_lowercase().as_str() {
            "py" | "python" | "python3" | "py3" | "pyi" => Some(Lang::Python),
            "ts" | "typescript" | "js" | "javascript" | "mjs" | "cjs" | "node" => Some(Lang::TypeScript),
            "tsx" | "jsx" => Some(Lang::Tsx),
            _ => None,
        }
    }

    pub fn from_filename(name: &str) -> Option<Lang> {
        Lang::from_tag(name.rsplit('.').next()?)
    }

    fn language(self) -> Language {
        match self {
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

thread_local! {
    static PARSERS: RefCell<[Option<Parser>; 3]> = const { RefCell::new([None, None, None]) };
}

pub fn parse(code: &str, lang: Lang) -> Option<Tree> {
    PARSERS.with(|cell| {
        let mut parsers = cell.borrow_mut();
        let slot = &mut parsers[lang as usize];
        if slot.is_none() {
            let mut p = Parser::new();
            p.set_language(&lang.language()).ok()?;
            *slot = Some(p);
        }
        slot.as_mut().unwrap().parse(code, None)
    })
}

/// True when `code` parses without any ERROR or MISSING nodes.
pub fn parses_cleanly(code: &str, lang: Lang) -> bool {
    parse(code, lang).is_some_and(|t| !t.root_node().has_error())
}

static PY_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^(?:\s*(?:async\s+)?def \w+\(|class \w+[(:]|import \w|from [\w.]+ import |\s*self\.\w+|if __name__ ==)")
        .unwrap()
});
static TS_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^(?:\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\b|\s*(?:export\s+)?(?:const|let|var)\s+\w+\s*[:=]|import .* from ['\x22]|\s*(?:export\s+)?(?:interface|type|enum|class)\s+\w+|\s*module\.exports)",
    )
    .unwrap()
});

/// Guess whether unfenced text is Python or TS/JS source. Requires at least two
/// language hints *and* a clean parse.
pub fn detect_lang(code: &str) -> Option<Lang> {
    let py = PY_HINT.find_iter(code).count();
    let ts = TS_HINT.find_iter(code).count();
    let order = if py >= ts { [Lang::Python, Lang::TypeScript] } else { [Lang::TypeScript, Lang::Python] };
    for (lang, score) in order.into_iter().map(|l| (l, if l == Lang::Python { py } else { ts })) {
        if score >= 2 && parses_cleanly(code, lang) {
            return Some(lang);
        }
    }
    None
}

/// One contiguous range of rows to replace with a placeholder line.
#[derive(Debug, Clone)]
pub struct Collapse {
    pub start_row: usize,
    pub end_row: usize, // inclusive
    pub placeholder: String,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub collapses: Vec<Collapse>,
    /// Every function/method name defined in the snippet.
    pub defined: Vec<String>,
}

impl Plan {
    pub fn hidden_rows(&self) -> usize {
        self.collapses.iter().map(|c| c.end_row - c.start_row + 1).sum()
    }
}

/// Apply a plan to the given lines (which may carry line-number prefixes).
/// `placeholder_prefix` receives the first hidden line and returns the prefix
/// to put in front of the placeholder (used for `cat -n` style output).
pub fn apply(lines: &[&str], plan: &Plan, placeholder_prefix: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut row = 0;
    let mut cs = plan.collapses.iter().peekable();
    while row < lines.len() {
        if let Some(c) = cs.peek() {
            if c.start_row == row {
                out.push_str(&placeholder_prefix(lines[row]));
                out.push_str(&c.placeholder);
                out.push('\n');
                row = c.end_row + 1;
                cs.next();
                continue;
            }
        }
        out.push_str(lines[row]);
        out.push('\n');
        row += 1;
    }
    out
}

const ALWAYS_KEEP: &[&str] = &["__init__", "__post_init__", "constructor"];

/// Plan which function bodies to collapse. `keep(name)` protects a function
/// (and everything that encloses it) from collapsing. Returns `None` when the
/// snippet doesn't parse cleanly or the collapsed output would not.
pub fn plan(code: &str, lang: Lang, keep: &dyn Fn(&str) -> bool, min_rows: usize) -> Option<Plan> {
    let tree = parse(code, lang)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let src = code.as_bytes();
    let lines: Vec<&str> = code.split('\n').collect();

    let mut candidates: Vec<Collapse> = Vec::new();
    let mut kept_rows: Vec<(usize, usize)> = Vec::new();
    let mut defined = Vec::new();

    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
        if !is_function(node.kind(), lang) {
            continue;
        }
        let Some(body) = node.child_by_field_name("body") else { continue };
        let name = function_name(node, src);
        if let Some(n) = &name {
            defined.push(n.clone());
        }
        let protected = name.as_deref().is_some_and(|n| keep(n) || ALWAYS_KEEP.contains(&n));
        if protected {
            kept_rows.push((node.start_position().row, node.end_position().row));
            continue;
        }
        let range = match lang {
            Lang::Python => python_body_range(node, body),
            _ => brace_body_range(body, &lines),
        };
        let Some((start, end, indent)) = range else { continue };
        if end + 1 - start < min_rows {
            continue;
        }
        let n = end + 1 - start;
        let placeholder = match lang {
            Lang::Python => format!("{indent}...  # toksqueeze: {n} lines hidden"),
            _ => format!("{indent}/* toksqueeze: {n} lines hidden */"),
        };
        candidates.push(Collapse { start_row: start, end_row: end, placeholder });
    }

    // Never hide a protected function inside a collapsed parent.
    candidates.retain(|c| !kept_rows.iter().any(|&(s, e)| s >= c.start_row && e <= c.end_row));
    // Outermost first; drop anything nested in an already-chosen range.
    candidates.sort_by_key(|c| (c.start_row, std::cmp::Reverse(c.end_row)));
    let mut chosen: Vec<Collapse> = Vec::new();
    for c in candidates {
        if chosen.last().is_some_and(|p| c.start_row <= p.end_row) {
            continue;
        }
        chosen.push(c);
    }

    let plan = Plan { collapses: chosen, defined };
    if !plan.collapses.is_empty() {
        let collapsed = apply(&lines, &plan, |_| String::new());
        if !parses_cleanly(&collapsed, lang) {
            return None;
        }
    }
    Some(plan)
}

fn is_function(kind: &str, lang: Lang) -> bool {
    match lang {
        Lang::Python => kind == "function_definition",
        _ => matches!(
            kind,
            "function_declaration"
                | "function_expression"
                | "function"
                | "generator_function_declaration"
                | "generator_function"
                | "arrow_function"
                | "method_definition"
        ),
    }
}

fn text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn function_name(node: Node, src: &[u8]) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(text(n, src).to_string());
    }
    // `const foo = () => {}`, `obj.foo = function() {}`, `{ foo: () => {} }`, class fields.
    let parent = node.parent()?;
    let field = match parent.kind() {
        "variable_declarator" | "public_field_definition" | "field_definition" => "name",
        "assignment_expression" => "left",
        "pair" => "key",
        _ => return None,
    };
    let n = parent.child_by_field_name(field)?;
    let t = text(n, src);
    Some(t.rsplit('.').next().unwrap_or(t).to_string())
}

/// Rows of a Python function body to hide, keeping a leading docstring.
fn python_body_range(func: Node, body: Node) -> Option<(usize, usize, String)> {
    let header_row = body.prev_sibling().map(|n| n.end_position().row).unwrap_or(func.start_position().row);
    let mut start = body.start_position().row;
    let end = body.end_position().row;
    if start <= header_row {
        return None; // one-liner: `def f(): return 1`
    }
    let indent = " ".repeat(body.start_position().column);
    if let Some(first) = body.named_child(0) {
        let is_doc = first.kind() == "expression_statement"
            && first.named_child(0).is_some_and(|s| s.kind() == "string");
        if is_doc {
            start = first.end_position().row + 1;
        }
    }
    (start <= end).then_some((start, end, indent))
}

/// Rows strictly between `{` and `}` of a statement block, when the braces sit
/// at the end / start of their own lines.
fn brace_body_range(body: Node, lines: &[&str]) -> Option<(usize, usize, String)> {
    if body.kind() != "statement_block" {
        return None; // concise arrow body: `x => x + 1`
    }
    let open_row = body.start_position().row;
    let close_row = body.end_position().row;
    if close_row < open_row + 2 {
        return None;
    }
    let after_open = &lines[open_row][body.start_position().column + 1..];
    let before_close = &lines[close_row][..body.end_position().column.saturating_sub(1)];
    if !after_open.trim().is_empty() || !before_close.trim().is_empty() {
        return None;
    }
    let first = lines[open_row + 1];
    let indent = first[..first.len() - first.trim_start().len()].to_string();
    Some((open_row + 1, close_row - 1, indent))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PY: &str = r#"import os
from typing import Optional

class Config:
    """App config."""
    def __init__(self, path: str):
        self.path = path
        self.data = {}

    def load(self) -> dict:
        """Load the file."""
        with open(self.path) as f:
            raw = f.read()
        text = "keep\n\nthis literal"
        return parse(raw)

def parse(raw: str) -> dict:
    out = {}
    for line in raw.splitlines():
        k, v = line.split("=", 1)
        out[k] = v
    return out
"#;

    #[test]
    fn collapses_python_bodies_but_keeps_targets() {
        let p = plan(PY, Lang::Python, &|n| n == "parse", 2).unwrap();
        let lines: Vec<&str> = PY.split('\n').collect();
        let out = apply(&lines, &p, |_| String::new());
        assert!(out.contains("def load(self) -> dict:\n        \"\"\"Load the file.\"\"\"\n        ...  # toksqueeze: 4 lines hidden"));
        assert!(out.contains("k, v = line.split(\"=\", 1)"), "kept function stays intact");
        assert!(out.contains("self.data = {}"), "constructors are kept");
        assert!(parses_cleanly(&out, Lang::Python));
        assert!(p.defined.contains(&"load".to_string()));
    }

    #[test]
    fn collapses_ts_methods_and_arrows() {
        let ts = "export interface User { id: string }\n\nexport class Repo {\n  constructor(private db: Db) {}\n\n  async find(id: string): Promise<User> {\n    const row = await this.db.get(id);\n    if (!row) throw new Error(\"missing\");\n    return row as User;\n  }\n}\n\nexport const handler = async (req: Req) => {\n  const u = await repo.find(req.id);\n  log(u);\n  return u;\n};\n";
        let p = plan(ts, Lang::TypeScript, &|_| false, 2).unwrap();
        let lines: Vec<&str> = ts.split('\n').collect();
        let out = apply(&lines, &p, |_| String::new());
        assert!(out.contains("async find(id: string): Promise<User> {\n    /* toksqueeze: 3 lines hidden */\n  }"));
        assert!(out.contains("export const handler = async (req: Req) => {\n  /* toksqueeze: 3 lines hidden */\n};"));
        assert!(out.contains("export interface User { id: string }"));
        assert!(parses_cleanly(&out, Lang::TypeScript));
    }

    #[test]
    fn detects_unfenced_python() {
        assert_eq!(detect_lang(PY), Some(Lang::Python));
        assert_eq!(detect_lang("just some words\nand more words"), None);
    }
}
