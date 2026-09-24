mod cli_system;

use clap::builder::styling::{Color, RgbColor, Style, Styles};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Instant;
use zest::compressor::{BlockKind, CompressorEngine, Mode};
use zest::config::{self, Config};
use zest::onboarding::{self, Lang, Setup};
use zest::proxy::server::{self, ProxyConfig};
use zest::tui::stats;
use zest::tui::theme::{self, FLAME, SPARK};

const fn rgb((r, g, b): (u8, u8, u8)) -> Option<Color> {
    Some(Color::Rgb(RgbColor(r, g, b)))
}

/// `--help` in the electric-blue theme.
const STYLES: Styles = Styles::styled()
    .header(Style::new().bold().fg_color(rgb(theme::ELECTRIC)))
    .usage(Style::new().bold().fg_color(rgb(theme::ELECTRIC)))
    .literal(Style::new().bold().fg_color(rgb(theme::SPARK)))
    .placeholder(Style::new().fg_color(rgb(theme::DEEP_CYAN)))
    .valid(Style::new().fg_color(rgb(theme::SPARK)))
    .invalid(Style::new().bold().fg_color(rgb(theme::FLAME)))
    .error(Style::new().bold().fg_color(rgb(theme::FLAME)));

/// zest: blue-fire prompt compression for LLM APIs.
///
/// With no subcommand, compresses stdin (or FILE) to stdout:
///   cat build.log | zest
///
/// New here? Run `zest init` for a guided setup (English / Русский).
#[derive(Parser)]
#[command(version, styles = STYLES, args_conflicts_with_subcommands = true)]
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
    /// Guided first-run setup with Zesty (English / Русский).
    #[command(visible_alias = "onboarding", alias = "setup")]
    Init(InitArgs),
    /// Compress stdin or a file to stdout.
    Compress(CompressArgs),
    /// Manage the local Zest Root CA used for HTTPS interception.
    #[command(subcommand)]
    Cert(CertCmd),
    /// Point the macOS system proxy at zest, or restore the previous settings.
    #[command(subcommand, name = "system-proxy")]
    SystemProxy(SysProxyCmd),
    /// Switch between system-wide mode and base-URL (export) mode.
    #[command(subcommand)]
    Mode(ModeCmd),
}

#[derive(Subcommand)]
enum CertCmd {
    /// Create Zest Root CA (if needed) and trust it (macOS: System keychain, asks for your password).
    Install {
        /// Don't ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Untrust and delete Zest Root CA, and turn interception off.
    #[command(alias = "uninstall")]
    Remove {
        #[arg(long, short)]
        yes: bool,
    },
    /// Show where the CA is and whether it's trusted.
    Status,
}

#[derive(Subcommand)]
enum SysProxyCmd {
    /// Route HTTP/HTTPS for every network service through zest (zest must be running).
    On {
        #[arg(long, short)]
        port: Option<u16>,
        /// Skip the check that zest is running.
        #[arg(long)]
        force: bool,
    },
    /// Restore the proxy settings from before `on`.
    Off {
        #[arg(long, short)]
        port: Option<u16>,
    },
    /// Is the system proxy pointing at zest?
    Status {
        #[arg(long, short)]
        port: Option<u16>,
    },
}

#[derive(Subcommand)]
enum ModeCmd {
    /// Desktop apps and tools using api.anthropic.com / api.openai.com go through zest
    /// automatically (macOS; trusts the local CA, needs your password once).
    System {
        #[arg(long, short)]
        yes: bool,
    },
    /// Base-URL mode: tools opt in with ANTHROPIC_BASE_URL / OPENAI_BASE_URL.
    Cli,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on [default: your `zest init` choice, else 8080]
    #[arg(long, short)]
    port: Option<u16>,
    /// Compression mode [default: your `zest init` choice, else balanced]
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[arg(long, default_value = "https://api.anthropic.com", env = "ZEST_ANTHROPIC_UPSTREAM")]
    anthropic_upstream: String,
    #[arg(long, default_value = "https://api.openai.com", env = "ZEST_OPENAI_UPSTREAM")]
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
    /// Decrypt and compress HTTPS to api.anthropic.com / api.openai.com (needs `zest cert install`)
    /// [default: your `zest mode` choice]
    #[arg(long, overrides_with = "no_mitm")]
    mitm: bool,
    #[arg(long, hide = true)]
    no_mitm: bool,
    /// Point the macOS system proxy at zest while it runs and restore it on exit
    /// [default: your `zest mode` choice]
    #[arg(long, overrides_with = "no_system_proxy")]
    system_proxy: bool,
    #[arg(long, hide = true)]
    no_system_proxy: bool,
}

fn flag(on: bool, off: bool, saved: Option<bool>) -> bool {
    if on {
        true
    } else if off {
        false
    } else {
        saved.unwrap_or(false)
    }
}

/// Used to get `ServeArgs` with all their defaults (including env vars) after onboarding.
#[derive(Parser)]
struct ServeDefaults {
    #[command(flatten)]
    args: ServeArgs,
}

#[derive(Args)]
struct InitArgs {
    /// Skip the language question.
    #[arg(long, value_parser = ["en", "ru"])]
    lang: Option<String>,
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

fn run_serve(s: ServeArgs, saved: Option<Config>) -> std::io::Result<()> {
    let saved = saved.unwrap_or_default();
    let port = s.port.or(saved.port).unwrap_or(8080);
    let mode = s.mode.or(saved.mode).unwrap_or(Mode::Balanced);
    let system_proxy = flag(s.system_proxy, s.no_system_proxy, saved.system_proxy);
    let mitm = flag(s.mitm, s.no_mitm, saved.mitm) || system_proxy;
    let mitm_ca_dir = if mitm {
        let dir = zest::certs::certs_dir_or_err()?;
        if !zest::certs::Ca::exists(&dir) {
            return Err(std::io::Error::other(
                "HTTPS interception needs the local CA: run `zest cert install` (or `zest mode system`), or start with --no-mitm --no-system-proxy",
            ));
        }
        Some(dir)
    } else {
        None
    };
    let addr: SocketAddr = match format!("{}:{port}", s.host).parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{} {}", theme::tag(), theme::paint(&format!("invalid address {}:{port}: {e}", s.host), FLAME));
            std::process::exit(2);
        }
    };
    let cfg = ProxyConfig {
        mode,
        openai_upstream: s.openai_upstream,
        anthropic_upstream: s.anthropic_upstream,
        default_price_per_mtok: s.price_per_mtok,
        keep_verbose_logs: s.keep_verbose_logs,
        quiet: s.quiet,
        mitm_ca_dir,
        mitm_upstream_override: None,
        system_proxy,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(server::serve(addr, cfg))
}

fn run_init(lang: Option<Lang>) -> std::io::Result<()> {
    let clear = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let Some(setup) = Setup::detect(lang, clear) else {
        return Err(std::io::Error::other("couldn't find your home folder (HOME / USERPROFILE is not set)"));
    };
    let outcome = onboarding::run(&mut std::io::stdin().lock(), &mut std::io::stdout(), &setup)?;
    match outcome {
        Some(o) => {
            let mut cfg = o.config;
            if o.system_mode && cli_system::mode_system(true)? {
                cfg = config::load().unwrap_or(cfg);
            }
            if o.start_now {
                run_serve(ServeDefaults::parse_from(["zest"]).args, Some(cfg))
            } else {
                Ok(())
            }
        }
        None => {
            eprintln!(
                "\n{} {}",
                theme::tag(),
                theme::paint("Setup stopped. Run `zest init` to start again. / Настройка прервана. Запустите `zest init`, чтобы начать заново.", SPARK)
            );
            Ok(())
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Serve(s)) => run_serve(s, config::load()),
        Some(Command::Init(a)) => run_init(a.lang.as_deref().and_then(Lang::from_code)),
        Some(Command::Compress(a)) => run_compress(a),
        Some(Command::Cert(c)) => match c {
            CertCmd::Install { yes } => cli_system::cert_install(yes).map(|_| ()),
            CertCmd::Remove { yes } => cli_system::cert_remove(yes),
            CertCmd::Status => cli_system::cert_status(),
        },
        Some(Command::SystemProxy(c)) => {
            let port = |p: Option<u16>| p.or(config::load().and_then(|c| c.port)).unwrap_or(8080);
            match c {
                SysProxyCmd::On { port: p, force } => cli_system::proxy_on(port(p), force),
                SysProxyCmd::Off { port: p } => cli_system::proxy_off(port(p)),
                SysProxyCmd::Status { port: p } => cli_system::proxy_status(port(p)),
            }
        }
        Some(Command::Mode(ModeCmd::System { yes })) => cli_system::mode_system(yes).map(|_| ()),
        Some(Command::Mode(ModeCmd::Cli)) => cli_system::mode_cli(),
        // A bare `zest` typed in a terminal: first run gets the guided setup.
        None if cli.compress.file.is_none() && std::io::stdin().is_terminal() => {
            if config::load().is_none() {
                run_init(None)
            } else {
                eprintln!(
                    "{} {}",
                    theme::tag(),
                    theme::paint("nothing to compress. Start the proxy with `zest serve`, pipe text in (`cat build.log | zest`), or rerun setup with `zest init`.", SPARK)
                );
                Ok(())
            }
        }
        None => run_compress(cli.compress),
    };
    if let Err(e) = result {
        eprintln!("{} {}", theme::tag(), theme::paint(&e.to_string(), FLAME));
        std::process::exit(1);
    }
}
