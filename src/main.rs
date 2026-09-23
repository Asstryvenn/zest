use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Instant;
use toksqueeze::compressor::{BlockKind, CompressorEngine, Mode};
use toksqueeze::proxy::server::{self, ProxyConfig};
use toksqueeze::tui::stats;

/// Zero-latency local token & context compression for LLM APIs.
///
/// With no subcommand, compresses stdin (or FILE) to stdout:
///   cat build.log | toksqueeze
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    compress: CompressArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Run the local proxy.
    Serve(ServeArgs),
    /// Compress stdin or a file to stdout.
    Compress(CompressArgs),
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, short, default_value_t = 8080)]
    port: u16,
    #[arg(long, value_enum, default_value_t = Mode::Balanced)]
    mode: Mode,
    #[arg(long, default_value = "https://api.anthropic.com", env = "TOKSQUEEZE_ANTHROPIC_UPSTREAM")]
    anthropic_upstream: String,
    #[arg(long, default_value = "https://api.openai.com", env = "TOKSQUEEZE_OPENAI_UPSTREAM")]
    openai_upstream: String,
    /// Input price (USD per 1M tokens) for models not in the built-in table.
    #[arg(long, default_value_t = 3.0)]
    price_per_mtok: f64,
    /// Never drop INFO/DEBUG log lines.
    #[arg(long)]
    keep_verbose_logs: bool,
    /// Don't print a line per request.
    #[arg(long, short)]
    quiet: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum InputKind {
    /// Terminal output, logs, files (no filler removal).
    Output,
    /// A human-written prompt (filler removal on).
    Prompt,
}

#[derive(Args)]
struct CompressArgs {
    /// Input file; reads stdin when omitted.
    file: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = Mode::Balanced)]
    mode: Mode,
    #[arg(long, value_enum, default_value_t = InputKind::Output)]
    kind: InputKind,
    /// Text naming what matters (functions/files kept intact, e.g. "fix parse_config").
    #[arg(long)]
    focus: Option<String>,
    #[arg(long)]
    keep_verbose_logs: bool,
    /// Don't print the stats line to stderr.
    #[arg(long, short)]
    quiet: bool,
}

fn run_compress(a: CompressArgs) -> std::io::Result<()> {
    let input = match &a.file {
        Some(p) => String::from_utf8_lossy(&std::fs::read(p)?).into_owned(),
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!("toksqueeze: reading from stdin (pipe input in, or see --help)");
            }
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            String::from_utf8_lossy(&buf).into_owned()
        }
    };
    let engine = CompressorEngine::new(a.mode).with_verbose_logs(a.keep_verbose_logs);
    let kind = match a.kind {
        InputKind::Output => BlockKind::Tool,
        InputKind::Prompt => BlockKind::Human,
    };
    let start = Instant::now();
    let focus = engine.focus_for(a.focus.as_deref().into_iter().chain([input.as_str()]));
    let out = engine.compress(&input, kind, &focus);
    let elapsed = start.elapsed();
    std::io::stdout().write_all(out.as_bytes())?;
    if !a.quiet {
        let (before, after) = (stats::count_tokens(&input), stats::count_tokens(&out));
        let usd = before.saturating_sub(after) as f64 * 3.0 / 1e6;
        eprintln!("{}", stats::format_line(before, after, usd, elapsed.as_secs_f64() * 1e3, ""));
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Serve(s)) => {
            let addr: SocketAddr = match format!("{}:{}", s.host, s.port).parse() {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("toksqueeze: invalid address {}:{}: {e}", s.host, s.port);
                    std::process::exit(2);
                }
            };
            let cfg = ProxyConfig {
                mode: s.mode,
                openai_upstream: s.openai_upstream,
                anthropic_upstream: s.anthropic_upstream,
                default_price_per_mtok: s.price_per_mtok,
                keep_verbose_logs: s.keep_verbose_logs,
                quiet: s.quiet,
            };
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(server::serve(addr, cfg))
        }
        Some(Command::Compress(a)) => run_compress(a),
        None => run_compress(cli.compress),
    };
    if let Err(e) = result {
        eprintln!("toksqueeze: {e}");
        std::process::exit(1);
    }
}
