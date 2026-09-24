//! macOS system-wide HTTP/HTTPS proxy via `networksetup`.
//!
//! Before changing anything, the current proxy settings of every enabled
//! network service are saved to `~/.config/zest/system-proxy.json`. Turning
//! the proxy off restores exactly those settings, even after a crash or from a
//! different process.

use crate::platform::{Output, Runner};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxySetting {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceState {
    pub service: String,
    pub web: ProxySetting,
    pub secure: ProxySetting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// Where zest pointed the system proxy.
    pub host: String,
    pub port: u16,
    /// Settings from before zest changed them.
    pub original: Vec<ServiceState>,
}

pub fn state_path() -> Option<PathBuf> {
    Some(crate::config::path()?.parent()?.join("system-proxy.json"))
}

/// Parse `networksetup -listallnetworkservices`: skip the header line and
/// services marked disabled with `*`.
pub fn parse_services(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("An asterisk") && !l.starts_with('*'))
        .map(|l| l.trim().to_string())
        .collect()
}

/// Parse `networksetup -getwebproxy <service>` (or `-getsecurewebproxy`).
pub fn parse_proxy(out: &str) -> ProxySetting {
    let mut s = ProxySetting::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "Enabled" => s.enabled = v.eq_ignore_ascii_case("yes"),
            "Server" => s.server = v.to_string(),
            "Port" => s.port = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    s
}

fn needs_admin(o: &Output) -> bool {
    let text = format!("{} {}", o.stdout, o.stderr).to_lowercase();
    !o.success || text.contains("requires admin") || text.contains("not authorized")
}

/// Run a `networksetup` change, retrying with sudo when macOS asks for admin rights.
fn setup(runner: &dyn Runner, args: &[&str]) -> io::Result<()> {
    let out = runner.run("networksetup", args, false)?;
    if !needs_admin(&out) {
        return Ok(());
    }
    let out = runner.run("networksetup", args, true)?;
    if needs_admin(&out) {
        return Err(io::Error::other(format!("networksetup {} failed: {}{}", args.join(" "), out.stdout.trim(), out.stderr.trim())));
    }
    Ok(())
}

pub fn services(runner: &dyn Runner) -> io::Result<Vec<String>> {
    let out = runner.run("networksetup", &["-listallnetworkservices"], false)?;
    if !out.success {
        return Err(io::Error::other(format!("networksetup -listallnetworkservices failed: {}", out.stderr.trim())));
    }
    Ok(parse_services(&out.stdout))
}

pub fn snapshot(runner: &dyn Runner, services: &[String]) -> io::Result<Vec<ServiceState>> {
    services
        .iter()
        .map(|svc| {
            let web = runner.run("networksetup", &["-getwebproxy", svc], false)?;
            let secure = runner.run("networksetup", &["-getsecurewebproxy", svc], false)?;
            Ok(ServiceState { service: svc.clone(), web: parse_proxy(&web.stdout), secure: parse_proxy(&secure.stdout) })
        })
        .collect()
}

fn load(path: &Path) -> Option<Saved> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Point every enabled network service's HTTP and HTTPS proxy at `host:port`.
/// Returns the services changed.
pub fn enable(runner: &dyn Runner, host: &str, port: u16, state: &Path) -> io::Result<Vec<String>> {
    let svcs = services(runner)?;
    // After an unclean shutdown the saved file holds the *real* originals;
    // snapshotting again would record zest's own settings as "original".
    if load(state).is_none() {
        let saved = Saved { host: host.to_string(), port, original: snapshot(runner, &svcs)? };
        if let Some(dir) = state.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(state, serde_json::to_string_pretty(&saved)?)?;
    }
    let p = port.to_string();
    for svc in &svcs {
        setup(runner, &["-setwebproxy", svc, host, &p])?;
        setup(runner, &["-setsecurewebproxy", svc, host, &p])?;
    }
    Ok(svcs)
}

fn restore_one(runner: &dyn Runner, svc: &str, set: &str, state_flag: &str, orig: &ProxySetting) -> io::Result<()> {
    if orig.enabled && !orig.server.is_empty() {
        setup(runner, &[set, svc, &orig.server, &orig.port.to_string()])
    } else {
        // Put the old server back (so the UI looks as before), then switch it off.
        if !orig.server.is_empty() {
            setup(runner, &[set, svc, &orig.server, &orig.port.to_string()])?;
        }
        setup(runner, &[state_flag, svc, "off"])
    }
}

/// Undo [`enable`]. With a saved state, restore it exactly; without one, turn
/// off any proxy that points at `host:port`. Returns the services changed.
pub fn disable(runner: &dyn Runner, host: &str, port: u16, state: &Path) -> io::Result<Vec<String>> {
    let mut changed = Vec::new();
    if let Some(saved) = load(state) {
        for s in &saved.original {
            restore_one(runner, &s.service, "-setwebproxy", "-setwebproxystate", &s.web)?;
            restore_one(runner, &s.service, "-setsecurewebproxy", "-setsecurewebproxystate", &s.secure)?;
            changed.push(s.service.clone());
        }
        std::fs::remove_file(state)?;
        return Ok(changed);
    }
    let svcs = services(runner)?;
    for s in snapshot(runner, &svcs)? {
        let ours = |p: &ProxySetting| p.enabled && p.server == host && p.port == port;
        if ours(&s.web) {
            setup(runner, &["-setwebproxystate", &s.service, "off"])?;
        }
        if ours(&s.secure) {
            setup(runner, &["-setsecurewebproxystate", &s.service, "off"])?;
        }
        if ours(&s.web) || ours(&s.secure) {
            changed.push(s.service);
        }
    }
    Ok(changed)
}

/// Services whose HTTPS proxy currently points at `host:port`.
pub fn active_services(runner: &dyn Runner, host: &str, port: u16) -> io::Result<Vec<String>> {
    let svcs = services(runner)?;
    Ok(snapshot(runner, &svcs)?
        .into_iter()
        .filter(|s| s.secure.enabled && s.secure.server == host && s.secure.port == port)
        .map(|s| s.service)
        .collect())
}

/// Turns the system proxy back off when dropped (Ctrl+C, SIGTERM, panics).
pub struct Guard<R: Runner> {
    pub runner: R,
    pub host: String,
    pub port: u16,
    pub state: PathBuf,
    armed: bool,
}

impl<R: Runner> Guard<R> {
    pub fn enable(runner: R, host: &str, port: u16, state: PathBuf) -> io::Result<(Self, Vec<String>)> {
        let svcs = enable(&runner, host, port, &state)?;
        Ok((Guard { runner, host: host.to_string(), port, state, armed: true }, svcs))
    }

    /// Restore now. Idempotent.
    pub fn restore(&mut self) -> io::Result<Vec<String>> {
        if !self.armed {
            return Ok(vec![]);
        }
        self.armed = false;
        disable(&self.runner, &self.host, self.port, &self.state)
    }
}

impl<R: Runner> Drop for Guard<R> {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            eprintln!("[zest] could not restore the system proxy: {e}. Run `zest system-proxy off`.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fake::FakeRunner;

    const SERVICES: &str = "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\nThunderbolt Bridge\n*iPhone USB\nUSB 10/100/1000 LAN\n";
    const OFF: &str = "Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0\n";
    const CORP: &str = "Enabled: Yes\nServer: proxy.corp.example\nPort: 3128\nAuthenticated Proxy Enabled: 0\n";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zest-sysproxy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("system-proxy.json")
    }

    #[test]
    fn parses_networksetup_output() {
        assert_eq!(parse_services(SERVICES), vec!["Wi-Fi", "Thunderbolt Bridge", "USB 10/100/1000 LAN"]);
        assert_eq!(parse_proxy(CORP), ProxySetting { enabled: true, server: "proxy.corp.example".into(), port: 3128 });
        assert_eq!(parse_proxy(OFF), ProxySetting::default());
    }

    #[test]
    fn enable_then_disable_restores_the_originals_exactly() {
        let state = tmp("restore");
        let r = FakeRunner::with(&[
            ("networksetup -listallnetworkservices", "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\nEthernet\n", true),
            ("networksetup -getwebproxy Wi-Fi", OFF, true),
            ("networksetup -getsecurewebproxy Wi-Fi", OFF, true),
            ("networksetup -getwebproxy Ethernet", CORP, true),
            ("networksetup -getsecurewebproxy Ethernet", CORP, true),
        ]);
        let svcs = enable(&r, "127.0.0.1", 8080, &state).unwrap();
        assert_eq!(svcs, vec!["Wi-Fi", "Ethernet"]);
        let calls = r.calls();
        assert!(calls.contains(&"networksetup -setwebproxy Wi-Fi 127.0.0.1 8080".to_string()));
        assert!(calls.contains(&"networksetup -setsecurewebproxy Ethernet 127.0.0.1 8080".to_string()));
        assert!(state.exists());

        let r2 = FakeRunner::default();
        disable(&r2, "127.0.0.1", 8080, &state).unwrap();
        assert_eq!(
            r2.calls(),
            vec![
                "networksetup -setwebproxystate Wi-Fi off",
                "networksetup -setsecurewebproxystate Wi-Fi off",
                "networksetup -setwebproxy Ethernet proxy.corp.example 3128",
                "networksetup -setsecurewebproxy Ethernet proxy.corp.example 3128",
            ]
        );
        assert!(!state.exists());
    }

    #[test]
    fn a_second_enable_after_a_crash_keeps_the_real_originals() {
        let state = tmp("crash");
        let first = FakeRunner::with(&[
            ("networksetup -listallnetworkservices", "Wi-Fi\n", true),
            ("networksetup -getwebproxy", CORP, true),
            ("networksetup -getsecurewebproxy", CORP, true),
        ]);
        enable(&first, "127.0.0.1", 8080, &state).unwrap();
        // zest died without restoring; the system now points at zest.
        let ours = "Enabled: Yes\nServer: 127.0.0.1\nPort: 8080\n";
        let second = FakeRunner::with(&[
            ("networksetup -listallnetworkservices", "Wi-Fi\n", true),
            ("networksetup -getwebproxy", ours, true),
            ("networksetup -getsecurewebproxy", ours, true),
        ]);
        enable(&second, "127.0.0.1", 8080, &state).unwrap();
        let saved = load(&state).unwrap();
        assert_eq!(saved.original[0].web.server, "proxy.corp.example");
        std::fs::remove_dir_all(state.parent().unwrap()).unwrap();
    }

    #[test]
    fn retries_with_sudo_when_admin_rights_are_needed() {
        let r = FakeRunner::with(&[
            ("networksetup -setwebproxy", "** Error: Command requires admin privileges.", false),
            ("sudo networksetup -setwebproxy", "", true),
        ]);
        setup(&r, &["-setwebproxy", "Wi-Fi", "127.0.0.1", "8080"]).unwrap();
        assert_eq!(
            r.calls(),
            vec!["networksetup -setwebproxy Wi-Fi 127.0.0.1 8080", "sudo networksetup -setwebproxy Wi-Fi 127.0.0.1 8080"]
        );
    }

    #[test]
    fn disable_without_saved_state_only_touches_proxies_pointing_at_zest() {
        let state = tmp("nostate");
        let ours = "Enabled: Yes\nServer: 127.0.0.1\nPort: 8080\n";
        let r = FakeRunner::with(&[
            ("networksetup -listallnetworkservices", "Wi-Fi\nEthernet\n", true),
            ("networksetup -getwebproxy Wi-Fi", ours, true),
            ("networksetup -getsecurewebproxy Wi-Fi", ours, true),
            ("networksetup -getwebproxy Ethernet", CORP, true),
            ("networksetup -getsecurewebproxy Ethernet", CORP, true),
        ]);
        assert_eq!(disable(&r, "127.0.0.1", 8080, &state).unwrap(), vec!["Wi-Fi"]);
        let sets: Vec<String> = r.calls().into_iter().filter(|c| c.contains("state")).collect();
        assert_eq!(sets, vec!["networksetup -setwebproxystate Wi-Fi off", "networksetup -setsecurewebproxystate Wi-Fi off"]);
    }

    #[test]
    fn guard_restores_on_drop() {
        let state = tmp("guard");
        let r = FakeRunner::with(&[
            ("networksetup -listallnetworkservices", "Wi-Fi\n", true),
            ("networksetup -get", OFF, true),
        ]);
        {
            let (_g, svcs) = Guard::enable(r, "127.0.0.1", 9000, state.clone()).unwrap();
            assert_eq!(svcs, vec!["Wi-Fi"]);
            assert!(state.exists());
        }
        assert!(!state.exists(), "dropping the guard restores and clears the saved state");
    }
}
