//! Step 5 verification: realistic log dumps and source files shrink by at least
//! 50% while the code that remains still parses and nothing important is lost.

use toksqueeze::compressor::ast::{parses_cleanly, Lang};
use toksqueeze::compressor::{BlockKind, CompressorEngine, Mode};
use toksqueeze::tui::stats::count_tokens;

const CI_LOG: &str = include_str!("fixtures/ci_run.log");
const PY: &str = include_str!("fixtures/inventory_service.py");
const TS: &str = include_str!("fixtures/orders.ts");

fn squeeze(mode: Mode, kind: BlockKind, text: &str) -> String {
    let e = CompressorEngine::new(mode);
    let focus = e.focus_for([text]);
    e.compress(text, kind, &focus).to_string()
}

fn reduction(before: &str, after: &str) -> f64 {
    1.0 - count_tokens(after) as f64 / count_tokens(before) as f64
}

/// The body of the first fenced block in `s`.
fn first_fence(s: &str) -> &str {
    let start = s.find("```").unwrap();
    let body = &s[start..];
    let body = &body[body.find('\n').unwrap() + 1..];
    &body[..body.find("\n```").unwrap() + 1]
}

#[test]
fn ci_log_shrinks_over_50_percent_and_keeps_the_failure() {
    for mode in [Mode::Balanced, Mode::Max] {
        let out = squeeze(mode, BlockKind::Tool, CI_LOG);
        let r = reduction(CI_LOG, &out);
        assert!(r >= 0.5, "{mode:?}: only {:.1}% saved", r * 100.0);
        for must in [
            "ValueError: available cannot go negative for 'A-1'",
            "shop/inventory/service.py\", line 57, in reserve_stock",
            "shop/inventory/views.py\", line 41, in post",
            "tests/test_inventory.py\", line 88",
            "test_reserve_stock_concurrent FAILED",
            "WARNING shop.cache: redis latency",
            "1 failed, 264 passed",
        ] {
            assert!(out.contains(must), "{mode:?} lost {must:?}");
        }
        assert!(!out.contains('\x1b'), "ANSI escapes survive");
        assert!(!out.contains("DEBUG shop.db"));
    }
}

#[test]
fn safe_mode_only_dedupes_and_strips_noise() {
    let out = squeeze(Mode::Safe, BlockKind::Tool, CI_LOG);
    assert!(out.contains("[TOKSQUEEZE: previous line repeated 119 more times]"));
    assert!(out.contains("INFO  shop."), "safe mode keeps INFO lines");
    assert!(out.contains("site-packages/django/test/client.py"), "safe mode keeps every frame");
    assert!(!out.contains('\x1b'));
}

#[test]
fn verbose_logs_are_kept_when_asked_for() {
    let e = CompressorEngine::new(Mode::Balanced);
    let focus = e.focus_for(["Show me the debug logs around the failure", CI_LOG]);
    let out = e.compress(CI_LOG, BlockKind::Tool, &focus);
    assert!(out.contains("DEBUG shop.db"));
}

#[test]
fn unfenced_python_file_in_max_mode_halves_and_still_parses() {
    let out = squeeze(Mode::Max, BlockKind::Tool, PY);
    let r = reduction(PY, &out);
    assert!(r >= 0.5, "only {:.1}% saved", r * 100.0);
    assert!(parses_cleanly(&out, Lang::Python), "collapsed Python must parse:\n{out}");
    assert!(out.contains("def reserve_stock(self, req: ReservationRequest) -> Reservation:"));
    assert!(out.contains("class ReservationRequest:\n    sku: str\n    qty: int"));
    assert!(out.contains("from shop.inventory.models import AuditEntry, Item, Reservation"));
}

#[test]
fn unfenced_typescript_file_in_max_mode_halves_and_still_parses() {
    let out = squeeze(Mode::Max, BlockKind::Tool, TS);
    let r = reduction(TS, &out);
    assert!(r >= 0.5, "only {:.1}% saved", r * 100.0);
    assert!(parses_cleanly(&out, Lang::TypeScript), "collapsed TS must parse:\n{out}");
    assert!(out.contains("export interface LineItem {\n  sku: string;"));
    assert!(out.contains("async transition(id: string, next: OrderStatus): Promise<Order> {"));
}

#[test]
fn balanced_keeps_the_targeted_function_byte_for_byte() {
    let prompt = format!(
        "Hello! Could you please look at this and fix the race in `reserve_stock`? It raises when two requests land together.\n\n```python\n{PY}```\n\nThanks in advance!"
    );
    let out = squeeze(Mode::Balanced, BlockKind::Human, &prompt);
    let code = first_fence(&out);
    assert!(parses_cleanly(code, Lang::Python), "collapsed Python must parse:\n{code}");

    // The targeted function is untouched, string literals included.
    let start = PY.find("    def reserve_stock").unwrap();
    let end = PY.find("    def release").unwrap();
    assert!(code.contains(&PY[start..end]));
    assert!(code.contains("...  # toksqueeze:"), "other bodies collapse");
    assert!(!out.contains("Could you please"));
    assert!(!out.contains("Thanks in advance"));
    assert!(reduction(&prompt, &out) >= 0.4);
}

#[test]
fn balanced_without_a_target_leaves_code_alone() {
    let prompt = format!("Why is this slow?\n\n```python\n{PY}```\n");
    let out = squeeze(Mode::Balanced, BlockKind::Human, &prompt);
    assert!(out.contains(PY), "no named target, so the whole file is kept");
}

#[test]
fn a_named_file_with_no_named_function_is_kept_whole() {
    let prompt = format!("Refactor inventory_service.py to use async views.\n\n```python\n{PY}```\n");
    let out = squeeze(Mode::Balanced, BlockKind::Human, &prompt);
    assert!(out.contains(PY));
}

#[test]
fn safe_mode_never_touches_code() {
    let prompt = format!("fix `reserve_stock`\n\n```python\n{PY}```\n```ts\n{TS}```\n");
    let out = squeeze(Mode::Safe, BlockKind::Human, &prompt);
    assert!(out.contains(PY) && out.contains(TS));
}

#[test]
fn string_literals_with_blank_lines_survive_every_mode() {
    let src = "def banner() -> str:\n    return \"\"\"\n\n\n    Welcome   to   the   shop\n\n\n\"\"\"\n\n\n\ndef other():\n    pass\n";
    for mode in [Mode::Safe, Mode::Balanced, Mode::Max] {
        let prompt = format!("fix `banner`\n```python\n{src}```\n");
        let out = squeeze(mode, BlockKind::Human, &prompt);
        assert!(out.contains(src), "{mode:?} changed a literal");
    }
}

#[test]
fn compression_is_deterministic_across_engines() {
    // A fresh engine (e.g. after a proxy restart) must produce identical bytes,
    // or provider prompt caches and signed thinking blocks would be invalidated.
    let prompt = format!("fix `total`\n```ts\n{TS}```\n{CI_LOG}");
    let a = squeeze(Mode::Max, BlockKind::Human, &prompt);
    let b = squeeze(Mode::Max, BlockKind::Human, &prompt);
    assert_eq!(a, b);
}

#[test]
fn stays_fast() {
    let e = CompressorEngine::new(Mode::Balanced);
    let big = CI_LOG.repeat(4);
    let focus = e.focus_for([big.as_str()]);
    let _ = e.compress(CI_LOG, BlockKind::Tool, &focus); // warm regexes and parsers
    let t = std::time::Instant::now();
    let _ = e.compress(&big, BlockKind::Tool, &focus);
    let ms = t.elapsed().as_secs_f64() * 1e3;
    // Generous bound so debug builds on slow CI pass; release builds run in a few ms.
    assert!(ms < 500.0, "took {ms:.1}ms");
}
