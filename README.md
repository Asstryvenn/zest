# TokSqueeze

A local proxy and CLI that compresses prompts, logs and source code **before** they
are sent to the OpenAI or Anthropic APIs. It is written in Rust and adds a few
milliseconds per request.

```
[TokSqueeze] 15,939 -> 722 tokens (-95.5%) | Saved: ~$0.076 | Latency: 2.8ms claude-opus-5 /v1/messages
```

## Quick start

```bash
cargo build --release
./target/release/toksqueeze serve --mode balanced        # listens on 127.0.0.1:8080
```

Point your tools at it:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080          # Claude Code, Anthropic SDKs
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1          # OpenAI SDKs, most IDE agents
```

Use it as a pipe:

```bash
cat build.log | toksqueeze                               # logs -> stdout, stats -> stderr
toksqueeze --mode max src/service.py                     # collapse function bodies
toksqueeze --kind prompt --focus "fix parse_config" prompt.md
```

## What it does

Every text block in `system`, `messages[].content` and tool results goes through four tiers.

| Tier | What | Modes |
|---|---|---|
| 1. Noise | Strips ANSI and control characters, keeps only the last frame of `\r` progress bars, removes trailing whitespace and extra blank lines, and collapses runs of identical lines (`[TOKSQUEEZE: previous line repeated 499 more times]`) | all |
| 1. Filler | Removes greetings, "could you please…", "read this carefully and…" and "thanks in advance" from human-written prose (never inside backticks or quotes) | balanced, max |
| 2. AST | Tree-sitter (Python, TypeScript/JavaScript, TSX) collapses function bodies to `...  # toksqueeze: N lines hidden` or `/* toksqueeze: N lines hidden */`. It keeps imports, classes, types, interfaces, signatures, docstrings, constructors and every function the message names | balanced*, max |
| 3. Logs | Drops TRACE/DEBUG/INFO lines when the text looks like a log. Keeps them if they mention error/fail/timeout, or if the user asks for "debug logs". Collapses runs of passing tests. Prunes Python and JS/Java stack traces to the innermost frames plus all user-code frames, dropping `site-packages` and `node_modules` internals | balanced, max |
| 4. Safety | Code is only ever changed by whole rows *inside* function bodies, so string literals, SQL and JSON are never cut. Shell, SQL, JSON, YAML and other fenced languages are never touched. Every collapse is re-parsed and dropped if it doesn't parse. If upstream returns 400 for a rewritten body, the original is retried unchanged | all |

\* **Balanced** only collapses code in a user or system message whose prose names specific code
(`` `reserve_stock` ``, `parse_config()`, `camelCase`, a traceback frame, a file name). Only the parts the
user didn't name are collapsed. If a file is named but no function in it is, that file is treated as the
edit target and kept whole. With no target, code is left alone. **Max** also collapses unfenced code in
tool results (including `cat -n`-style listings, line numbers preserved) and folds lines that differ only
in numbers.

| `--mode` | Use when |
|---|---|
| `safe` | You want zero semantic risk: only whitespace, ANSI and exact duplicates change |
| `balanced` (default) | Coding agents and chat: large wins on logs, code only where a target is clear |
| `max` | Read-heavy agents where the model mostly needs structure, not bodies |

### Measured on the fixtures in `tests/fixtures/`

| Input | safe | balanced | max |
|---|---|---|---|
| `ci_run.log` (pytest + app logs + Django traceback, 16k tokens) | -15.8% | -95.6% | -96.2% |
| `inventory_service.py`, unfenced (1.1k tokens) | 0% | 0% | -58.7% |
| `orders.ts`, unfenced (0.9k tokens) | 0% | 0% | -60.3% |

## Prompt caching and thinking blocks

Compression is a **pure function** of (mode, block kind, the message's own prose, text). It never depends
on later messages, so the rewritten history is byte-identical on every request. That keeps Anthropic/OpenAI
prompt caches warm. It also keeps signed thinking blocks valid: newer Claude models reject edited history
for some accounts. Anthropic assistant turns are never rewritten.

Two things to know:

- **Start the proxy before the session and keep the mode fixed.** If you switch mode mid-conversation, or
  route an existing conversation through the proxy, the history the model sees changes. That costs one
  cache miss and can trigger a history-mismatch 400. The proxy then retries the request uncompressed.
- The `$` figure uses list input prices and ignores cache-read discounts, so it is an upper bound.

## Proxy details

| Route | Behaviour |
|---|---|
| `POST /v1/messages`, `/v1/messages/count_tokens` | Anthropic format, compressed, sent to `--anthropic-upstream` |
| `POST */chat/completions` | OpenAI format, compressed, sent to `--openai-upstream` |
| anything else | Passed through unchanged. Requests with `anthropic-version`/`x-api-key` go to Anthropic, everything else to OpenAI |
| `GET /toksqueeze/stats` | Session totals as JSON |
| `GET /toksqueeze/health` | `ok` |

Headers (including auth) are forwarded as-is, and `Content-Length` is recomputed. Responses, including SSE
streams, are streamed back without buffering. Tool calls, tool inputs, images, documents, thinking blocks
and `cache_control` markers are never modified. Rewritten text blocks are cached in memory by content hash,
so conversations that resend their history are cheap.

This is a base-URL proxy, not an HTTPS man-in-the-middle, so no certificates need to be installed.

Token counts use the o200k BPE (`tiktoken-rs`). Claude's tokenizer counts somewhat differently, so treat
absolute numbers as estimates. The before/after ratio is consistent.

## Layout

```
src/
├── main.rs                 CLI (clap): `serve`, `compress`, or bare pipe mode
├── proxy/server.rs         axum server, routing, forwarding, 400 fallback
├── proxy/handlers.rs       OpenAI / Anthropic JSON rewriting
├── compressor/engine.rs    pipeline manager, fence segmentation, code detection, cache
├── compressor/ast.rs       tree-sitter body collapsing + re-parse validation
├── compressor/logs.rs      level filtering, dedup, pass-run folding, traceback pruning
├── compressor/text.rs      ANSI/control stripping, whitespace, filler removal
└── tui/stats.rs            token counting, pricing table, the one-line readout
tests/
├── compression.rs          >=50% reduction on fixtures, syntax validity, safety rules
└── proxy.rs                end-to-end through a mock upstream (streaming, 400 retry)
```

```bash
cargo test
```

## Not yet done

- Tree-sitter grammars beyond Python/TS/JS (Rust, Go and Java code is detected and protected, not reduced)
- OpenAI Responses API (`/v1/responses`) bodies pass through uncompressed
- A full-screen TUI dashboard (currently one line per request plus a session summary on Ctrl+C)
