//! The local "Zest Root CA" used to intercept HTTPS to the LLM APIs.
//!
//! The CA carries X.509 *name constraints*: it can only vouch for the
//! intercepted API hostnames. Even if its private key leaked, TLS clients that
//! enforce name constraints (macOS, Chromium, rustls/webpki, Go, ...) would
//! reject a certificate it signed for any other site.
//!
//! Files: `~/.config/zest/certs/rootCA.pem` and `rootCA-key.pem` (mode 0600).

use crate::platform::{self, Runner};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, NameConstraints, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io;
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

/// Hosts whose HTTPS traffic is decrypted and compressed. Everything else is tunneled untouched.
pub const INTERCEPT_HOSTS: [&str; 2] = ["api.anthropic.com", "api.openai.com"];
pub const CA_NAME: &str = "Zest Root CA";
pub const CERT_FILE: &str = "rootCA.pem";
pub const KEY_FILE: &str = "rootCA-key.pem";
const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

/// `~/.config/zest/certs` (next to `config.toml`).
pub fn default_dir() -> Option<PathBuf> {
    Some(crate::config::path()?.parent()?.join("certs"))
}

pub fn certs_dir_or_err() -> io::Result<PathBuf> {
    default_dir().ok_or_else(|| io::Error::other("no config directory (HOME is not set)"))
}

/// Is `host` (optionally with `:port`, any case, optional trailing dot) one we intercept?
pub fn is_intercepted(authority: &str) -> bool {
    let host = authority.rsplit_once(':').map_or(authority, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { authority });
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    INTERCEPT_HOSTS.contains(&host.as_str())
}

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

fn serial(seed: &str) -> SerialNumber {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let h = xxhash_rust::xxh3::xxh3_128(format!("{seed}{nanos}{}", std::process::id()).as_bytes());
    let mut bytes = h.to_be_bytes().to_vec();
    bytes[0] &= 0x7f; // keep it positive
    SerialNumber::from_slice(&bytes)
}

/// The CA's fixed identity. The issuer used to sign leaves is rebuilt from
/// these parameters plus the saved key, so they must never change.
fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_NAME);
    dn.push(DnType::OrganizationName, "zest (local, this computer only)");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: INTERCEPT_HOSTS.iter().map(|h| GeneralSubtree::DnsName(h.to_string())).collect(),
        excluded_subtrees: vec![],
    });
    p
}

pub struct Ca {
    pub dir: PathBuf,
    pub cert_pem: String,
    cert_der: CertificateDer<'static>,
    key: KeyPair,
}

impl Ca {
    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(CERT_FILE)
    }

    pub fn exists(dir: &Path) -> bool {
        dir.join(CERT_FILE).is_file() && dir.join(KEY_FILE).is_file()
    }

    /// Create a new CA in `dir`, replacing any existing one.
    pub fn generate(dir: &Path) -> io::Result<Ca> {
        let key = KeyPair::generate().map_err(err)?;
        let mut params = ca_params();
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(3650);
        params.serial_number = Some(serial("ca"));
        let cert = params.self_signed(&key).map_err(err)?;

        std::fs::create_dir_all(dir)?;
        restrict(dir, 0o700)?;
        let key_path = dir.join(KEY_FILE);
        std::fs::write(&key_path, key.serialize_pem())?;
        restrict(&key_path, 0o600)?;
        std::fs::write(dir.join(CERT_FILE), cert.pem())?;
        Ok(Ca { dir: dir.to_path_buf(), cert_pem: cert.pem(), cert_der: cert.der().clone(), key })
    }

    pub fn load(dir: &Path) -> io::Result<Ca> {
        let cert_pem = std::fs::read_to_string(dir.join(CERT_FILE))?;
        let key = KeyPair::from_pem(&std::fs::read_to_string(dir.join(KEY_FILE))?).map_err(err)?;
        let cert_der = pem_to_der(&cert_pem)?;
        Ok(Ca { dir: dir.to_path_buf(), cert_pem, cert_der, key })
    }

    /// Load the CA from `dir`, creating it first if needed. Returns `(ca, created)`.
    pub fn load_or_generate(dir: &Path) -> io::Result<(Ca, bool)> {
        if Ca::exists(dir) {
            Ok((Ca::load(dir)?, false))
        } else {
            Ok((Ca::generate(dir)?, true))
        }
    }

    pub fn der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    /// SHA-256 of the DER certificate, uppercase hex (what `security` prints).
    pub fn sha256_hex(&self) -> String {
        let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, self.cert_der.as_ref());
        d.as_ref().iter().map(|b| format!("{b:02X}")).collect()
    }

    /// A server certificate for `host`, signed by this CA. Returns the chain
    /// (leaf, CA) and the leaf's private key. Valid for 1 year: Apple rejects
    /// TLS server certificates valid for more than 825 days.
    pub fn leaf(&self, host: &str) -> io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let key = KeyPair::generate().map_err(err)?;
        let mut params = CertificateParams::new(vec![host.to_string()]).map_err(err)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        params.distinguished_name = dn;
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::days(1);
        params.not_after = now + Duration::days(365);
        params.serial_number = Some(serial(host));
        let issuer = Issuer::new(ca_params(), &self.key);
        let cert = params.signed_by(&key, &issuer).map_err(err)?;
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        Ok((vec![cert.der().clone(), self.cert_der.clone()], key_der))
    }
}

fn pem_to_der(pem: &str) -> io::Result<CertificateDer<'static>> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::from_pem_slice(pem.as_bytes()).map_err(err)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- trust store

/// Add the CA to the macOS System keychain as a trusted root (asks for sudo).
pub fn trust(runner: &dyn Runner, cert: &Path) -> io::Result<()> {
    let cert = cert.to_string_lossy();
    let out = runner.run(
        "security",
        &["add-trusted-cert", "-d", "-r", "trustRoot", "-k", SYSTEM_KEYCHAIN, &cert],
        true,
    )?;
    if !out.success {
        return Err(err(format!("security add-trusted-cert failed: {}", out.stderr.trim())));
    }
    Ok(())
}

/// Remove the CA's trust setting and the certificate from the System keychain.
pub fn untrust(runner: &dyn Runner, ca: &Ca) -> io::Result<()> {
    let cert = ca.cert_path();
    let cert = cert.to_string_lossy();
    // Removing trust fails harmlessly if it was never trusted; deleting by
    // SHA-256 only ever touches this exact certificate.
    let _ = runner.run("security", &["remove-trusted-cert", "-d", &cert], true)?;
    let hash = ca.sha256_hex();
    let out = runner.run("security", &["delete-certificate", "-Z", &hash, SYSTEM_KEYCHAIN], true)?;
    if !out.success && !out.stderr.contains("could not be found") && !out.stderr.contains("Unable to delete") {
        return Err(err(format!("security delete-certificate failed: {}", out.stderr.trim())));
    }
    Ok(())
}

/// Whether macOS currently trusts the CA for TLS (no sudo needed).
pub fn is_trusted(runner: &dyn Runner, cert: &Path) -> bool {
    let cert = cert.to_string_lossy();
    runner.run("security", &["verify-cert", "-c", &cert, "-p", "ssl"], false).is_ok_and(|o| o.success)
}

/// Manual instructions for platforms where zest doesn't change the trust store itself.
pub fn manual_trust_commands(os: &str, cert: &Path) -> Vec<String> {
    let c = cert.display();
    match os {
        "linux" => vec![
            format!("sudo cp {c} /usr/local/share/ca-certificates/zest-root-ca.crt"),
            "sudo update-ca-certificates".into(),
        ],
        "windows" => vec![format!("certutil -user -addstore Root \"{c}\"")],
        _ => vec![platform::display(
            "security",
            &["add-trusted-cert", "-d", "-r", "trustRoot", "-k", SYSTEM_KEYCHAIN, &c.to_string()],
            true,
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fake::FakeRunner;

    #[test]
    fn matches_only_the_api_hosts() {
        for yes in ["api.anthropic.com", "api.anthropic.com:443", "API.OpenAI.com:443", "api.openai.com."] {
            assert!(is_intercepted(yes), "{yes}");
        }
        for no in ["anthropic.com", "claude.ai:443", "api.anthropic.com.evil.io:443", "xapi.openai.com", "chatgpt.com", ""] {
            assert!(!is_intercepted(no), "{no}");
        }
    }

    #[test]
    fn trust_and_untrust_use_the_system_keychain_with_sudo() {
        let dir = std::env::temp_dir().join(format!("zest-trust-{}", std::process::id()));
        let ca = Ca::generate(&dir).unwrap();
        let r = FakeRunner::default();
        trust(&r, &ca.cert_path()).unwrap();
        untrust(&r, &ca).unwrap();
        let calls = r.calls();
        assert_eq!(
            calls[0],
            format!("sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain {}", ca.cert_path().display())
        );
        assert!(calls[1].starts_with("sudo security remove-trusted-cert -d "));
        assert_eq!(calls[2], format!("sudo security delete-certificate -Z {} /Library/Keychains/System.keychain", ca.sha256_hex()));
        assert_eq!(ca.sha256_hex().len(), 64);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
