//! `zest cert`, `zest system-proxy` and `zest mode`: the commands behind
//! system-wide (HTTPS interception) mode.

use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use zest::certs::{self, Ca, INTERCEPT_HOSTS};
use zest::config::{self, Config};
use zest::onboarding::{self, Lang};
use zest::platform::SystemRunner;
use zest::sysproxy;
use zest::tui::theme::{self, ELECTRIC, FLAME, MUTED, SPARK};

const MACOS: bool = cfg!(target_os = "macos");

fn say(text: &str) {
    eprintln!("{} {text}", theme::tag());
}

fn ok(text: &str) {
    eprintln!("{} {} {text}", theme::tag(), theme::bold("✓", ELECTRIC));
}

fn warn(text: &str) {
    eprintln!("{} {}", theme::tag(), theme::paint(text, FLAME));
}

fn cmd(text: &str) {
    eprintln!("    {}", theme::bold(text, SPARK));
}

fn lang() -> Lang {
    config::load().and_then(|c| c.language).and_then(|l| Lang::from_code(&l)).unwrap_or_else(Lang::from_env)
}

fn update_config(f: impl FnOnce(&mut Config)) -> io::Result<()> {
    let path = config::path().ok_or_else(|| io::Error::other("no config directory (HOME is not set)"))?;
    let mut cfg = config::load().unwrap_or_default();
    f(&mut cfg);
    config::save_to(&cfg, &path)
}

fn certs_dir() -> io::Result<std::path::PathBuf> {
    certs::default_dir().ok_or_else(|| io::Error::other("no config directory (HOME is not set)"))
}

/// Ask on the terminal. Without one, the answer is "no": system changes need a
/// person at the keyboard or an explicit `--yes`.
fn ask(question: &str, default: bool) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        warn(&format!("{question} -> no (not running in a terminal; pass --yes to confirm)"));
        return Ok(false);
    }
    let color = theme::enabled();
    Ok(onboarding::confirm(&mut io::stdin().lock(), &mut io::stderr(), color, question, default)?.unwrap_or(false))
}

/// Is zest answering on 127.0.0.1:`port`?
pub fn zest_running(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
    if s.write_all(b"GET /zest/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.starts_with("HTTP/1.1 200") && buf.ends_with("ok")
}

// ---------------------------------------------------------------- zest cert

/// Create the CA if needed and trust it. Returns true when it ends up trusted.
pub fn cert_install(yes: bool) -> io::Result<bool> {
    let dir = certs_dir()?;
    let (ca, created) = Ca::load_or_generate(&dir)?;
    let path = ca.cert_path();
    if created {
        ok(&format!("created {} in {}", certs::CA_NAME, dir.display()));
    } else {
        say(&format!("using the existing {} in {}", certs::CA_NAME, dir.display()));
    }
    say(&theme::paint(
        &format!("it can only vouch for {} (X.509 name constraints), and its key never leaves this computer", INTERCEPT_HOSTS.join(" and ")),
        MUTED,
    ));

    if !MACOS {
        say("trust it with your system's tools, for example:");
        for c in certs::manual_trust_commands(std::env::consts::OS, &path) {
            cmd(&c);
        }
        update_config(|c| c.mitm = Some(true))?;
        return Ok(false);
    }

    if certs::is_trusted(&SystemRunner, &path) {
        ok("macOS already trusts it");
    } else {
        say("macOS needs to trust it. zest will run:");
        cmd(&certs::manual_trust_commands("macos", &path)[0]);
        if !yes && !ask("Trust it now? You'll be asked for your password. [Y/n]", true)? {
            warn("not trusted yet. Run `zest cert install` when you're ready.");
            return Ok(false);
        }
        certs::trust(&SystemRunner, &path)?;
        ok("trusted by macOS (System keychain)");
    }
    update_config(|c| c.mitm = Some(true))?;
    Ok(true)
}

pub fn cert_remove(yes: bool) -> io::Result<()> {
    let dir = certs_dir()?;
    if !Ca::exists(&dir) {
        say("no Zest Root CA to remove");
        update_config(|c| {
            c.mitm = Some(false);
            c.system_proxy = Some(false);
        })?;
        return Ok(());
    }
    if !yes && !ask("Remove Zest Root CA from this Mac and delete its key? [Y/n]", true)? {
        return Ok(());
    }
    // Never leave the system proxy pointing at interception we're about to disable.
    if MACOS {
        if let Some(state) = sysproxy::state_path().filter(|p| p.exists()) {
            let port = config::load().and_then(|c| c.port).unwrap_or(8080);
            sysproxy::disable(&SystemRunner, "127.0.0.1", port, &state)?;
            ok("restored your previous system proxy settings");
        }
        let ca = Ca::load(&dir)?;
        certs::untrust(&SystemRunner, &ca)?;
        ok("removed from the macOS System keychain");
    } else {
        say("also remove it from your system's certificate store if you added it there");
    }
    std::fs::remove_dir_all(&dir)?;
    ok(&format!("deleted {}", dir.display()));
    update_config(|c| {
        c.mitm = Some(false);
        c.system_proxy = Some(false);
    })
}

pub fn cert_status() -> io::Result<()> {
    let dir = certs_dir()?;
    let cfg = config::load().unwrap_or_default();
    if !Ca::exists(&dir) {
        say("no Zest Root CA yet (create and trust it with `zest cert install`)");
        return Ok(());
    }
    let ca = Ca::load(&dir)?;
    say(&format!("certificate: {}", ca.cert_path().display()));
    say(&format!("SHA-256:     {}", ca.sha256_hex()));
    say(&format!("valid for:   {}", INTERCEPT_HOSTS.join(", ")));
    if MACOS {
        let trusted = certs::is_trusted(&SystemRunner, &ca.cert_path());
        say(&format!("trusted:     {}", if trusted { theme::bold("yes", ELECTRIC) } else { theme::paint("no (run `zest cert install`)", FLAME) }));
    }
    say(&format!("interception in `zest serve`: {}", if cfg.mitm == Some(true) { "on" } else { "off" }));
    Ok(())
}

// ---------------------------------------------------------------- zest system-proxy

fn require_macos() -> io::Result<()> {
    if MACOS {
        Ok(())
    } else {
        Err(io::Error::other(
            "the system proxy switch is macOS-only. Elsewhere, set HTTPS_PROXY=http://127.0.0.1:8080 for the apps you want to route through zest",
        ))
    }
}

fn state_path() -> io::Result<std::path::PathBuf> {
    sysproxy::state_path().ok_or_else(|| io::Error::other("no config directory (HOME is not set)"))
}

pub fn proxy_on(port: u16, force: bool) -> io::Result<()> {
    require_macos()?;
    if !force && !zest_running(port) {
        return Err(io::Error::other(format!(
            "zest isn't running on port {port}. Turning the system proxy on now would cut every app off from the internet. \
             Run `zest serve --system-proxy` instead (it switches the proxy on and back off by itself), or start zest first"
        )));
    }
    let services = sysproxy::enable(&SystemRunner, "127.0.0.1", port, &state_path()?)?;
    ok(&format!("system proxy for {} now points at 127.0.0.1:{port}", services.join(", ")));
    warn("keep zest running while it's on. Turn it off with `zest system-proxy off`.");
    Ok(())
}

pub fn proxy_off(port: u16) -> io::Result<()> {
    require_macos()?;
    let changed = sysproxy::disable(&SystemRunner, "127.0.0.1", port, &state_path()?)?;
    if changed.is_empty() {
        say("the system proxy wasn't pointing at zest; nothing to change");
    } else {
        ok(&format!("restored the previous proxy settings for {}", changed.join(", ")));
    }
    Ok(())
}

pub fn proxy_status(port: u16) -> io::Result<()> {
    require_macos()?;
    let active = sysproxy::active_services(&SystemRunner, "127.0.0.1", port)?;
    if active.is_empty() {
        say(&format!("System Proxy: {}", theme::paint("off", MUTED)));
    } else {
        say(&format!(
            "System Proxy: {} {} via {}",
            theme::bold("ACTIVE", ELECTRIC),
            theme::paint(&format!("({})", INTERCEPT_HOSTS.join(", ")), SPARK),
            active.join(", ")
        ));
        if !zest_running(port) {
            warn(&format!("but zest isn't answering on port {port}. Start it (`zest serve`) or run `zest system-proxy off`."));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- zest mode

/// Switch to system-wide mode: trust the CA and let `zest serve` manage the system proxy.
pub fn mode_system(already_confirmed: bool) -> io::Result<bool> {
    require_macos()?;
    let l = lang();
    if !already_confirmed {
        onboarding::say(&mut io::stderr(), theme::enabled(), false, onboarding::system_mode_text(l))?;
        if !ask(onboarding::system_mode_question(l), false)? {
            say("staying in base-URL (export) mode");
            return Ok(false);
        }
    }
    if !cert_install(true)? {
        return Ok(false);
    }
    update_config(|c| {
        c.mitm = Some(true);
        c.system_proxy = Some(true);
    })?;
    let happy = match l {
        Lang::En => "System-wide mode is on. Start me with zest serve: I switch your Mac's proxy on while I run and put your old settings back when I stop (Ctrl+C). The export mode still works too.",
        Lang::Ru => "Системный режим включён. Запустите меня командой zest serve: пока я работаю, прокси Mac включён, а когда я останавливаюсь (Ctrl+C), я возвращаю прежние настройки. Режим с export тоже продолжает работать.",
    };
    onboarding::say(&mut io::stderr(), theme::enabled(), true, happy)?;
    Ok(true)
}

/// Back to base-URL (export) mode.
pub fn mode_cli() -> io::Result<()> {
    update_config(|c| {
        c.mitm = Some(false);
        c.system_proxy = Some(false);
    })?;
    if MACOS {
        if let Some(state) = sysproxy::state_path().filter(|p| p.exists()) {
            let port = config::load().and_then(|c| c.port).unwrap_or(8080);
            sysproxy::disable(&SystemRunner, "127.0.0.1", port, &state)?;
            ok("restored your previous system proxy settings");
        }
    }
    ok("base-URL mode: point tools at zest with ANTHROPIC_BASE_URL / OPENAI_BASE_URL");
    if Ca::exists(&certs_dir()?) {
        say(&theme::paint("Zest Root CA is still installed; remove it with `zest cert remove`", MUTED));
    }
    Ok(())
}
