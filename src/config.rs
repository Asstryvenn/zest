//! User settings written by `zest init` and read by `zest serve`.
//!
//! Location: `$ZEST_CONFIG`, else `%APPDATA%\zest\config.toml` on Windows,
//! else `$XDG_CONFIG_HOME/zest/config.toml`, else `~/.config/zest/config.toml`.

use crate::compressor::Mode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// `en` or `ru`: the language chosen during onboarding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Decrypt HTTPS to the API hosts with the local Zest Root CA (`zest cert install`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mitm: Option<bool>,
    /// Point the macOS system proxy at zest while `zest serve` runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_proxy: Option<bool>,
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ZEST_CONFIG").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return Some(PathBuf::from(appdata).join("zest").join("config.toml"));
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(xdg).join("zest").join("config.toml"));
    }
    home_dir().map(|h| h.join(".config").join("zest").join("config.toml"))
}

/// The saved config, if there is one and it parses.
pub fn load() -> Option<Config> {
    let text = std::fs::read_to_string(path()?).ok()?;
    toml::from_str(&text).ok()
}

pub fn save_to(cfg: &Config, path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    std::fs::write(path, format!("# Written by `zest init`. Command-line flags override these.\n{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let dir = std::env::temp_dir().join(format!("zest-cfg-{}", std::process::id()));
        let p = dir.join("config.toml");
        let cfg = Config { language: Some("ru".into()), mode: Some(Mode::Max), port: Some(9090), mitm: Some(true), system_proxy: None };
        save_to(&cfg, &p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("mode = \"max\""));
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), cfg);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
