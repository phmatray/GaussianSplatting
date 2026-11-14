//! The self-signed certificate of the LAN listener.
//!
//! WebXR needs a secure context, so the headset reaches the server over
//! HTTPS. Nobody can issue a publicly trusted certificate for a private
//! address, hence a self-signed one that the user accepts once in the
//! headset browser. To keep that acceptance valid between runs the
//! certificate is stored in a directory chosen by the caller and reused as
//! long as it covers the machine's current LAN addresses and is not about
//! to expire.

use std::fs;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;

use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
const META_FILE: &str = "cert.json";

/// Lifetime of a new certificate. Kept under the 398 days some browsers
/// accept for a server certificate at all.
const VALIDITY: Duration = Duration::days(365);
/// A stored certificate this close to its end is replaced, so that it does
/// not expire in the middle of a session.
const RENEW_BEFORE: Duration = Duration::days(30);
/// Addresses kept in the certificate. Old ones stay (see [`load_or_create`]).
const MAX_ADDRESSES: usize = 16;
/// Names every certificate carries besides the LAN addresses.
const LOCAL_NAMES: [&str; 2] = ["localhost", "127.0.0.1"];

/// What is stored next to the certificate. Reading the validity and the
/// names back from here avoids an X.509 parser; the fingerprint ties the
/// file to the certificate it describes.
#[derive(Serialize, Deserialize)]
struct Meta {
    fingerprint: String,
    names: Vec<String>,
    /// Unix time of the end of validity.
    not_after: i64,
}

/// A certificate with its private key.
pub(crate) struct Identity {
    pub cert_der: Vec<u8>,
    /// PKCS#8.
    pub key_der: Vec<u8>,
    /// Subject alternative names: host names and IP addresses.
    pub names: Vec<String>,
    pub not_after: i64,
}

impl Identity {
    /// See [`fingerprint`].
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }

    /// The rustls configuration serving this identity over HTTP/2 and
    /// HTTP/1.1.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, rustls::Error> {
        let cert = CertificateDer::from(self.cert_der.clone());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        // An explicit provider: nothing depends on (or installs) the
        // process-wide default, which panics when another dependency of the
        // final binary enables a second rustls backend.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)?;
        // Not set by default; without it every connection is HTTP/1.1.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

/// SHA-256 of a DER certificate, as browsers display it: upper-case hex
/// bytes separated by colons.
pub fn fingerprint(cert_der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let hex: Vec<String> = digest.as_ref().iter().map(|byte| format!("{byte:02X}")).collect();
    hex.join(":")
}

fn required_names(lan: &[Ipv4Addr]) -> Vec<String> {
    let mut names: Vec<String> = LOCAL_NAMES.iter().map(|n| n.to_string()).collect();
    for ip in lan.iter().take(MAX_ADDRESSES) {
        let name = ip.to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn generate(names: Vec<String>, now: OffsetDateTime) -> Result<Identity, rcgen::Error> {
    // rcgen turns the strings that parse as IP addresses into IP SANs.
    let mut params = CertificateParams::new(names.clone())?;
    params.distinguished_name.push(DnType::CommonName, "Splat Live (auto-signé)");
    // A day of slack for a headset whose clock is behind.
    params.not_before = now - Duration::days(1);
    params.not_after = now + VALIDITY;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.insert_extended_key_usage(ExtendedKeyUsagePurpose::ServerAuth);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(Identity {
        cert_der: cert.der().to_vec(),
        key_der: key.serialize_der(),
        names,
        not_after: params.not_after.unix_timestamp(),
    })
}

fn pem(label: &str, der: &[u8]) -> String {
    pem::encode(&pem::Pem::new(label, der))
}

/// Writes `contents` to `path` through a temporary file, so that a crash
/// never leaves half a certificate behind. `private` restricts the file to
/// its owner where the platform has such a notion.
fn write_atomic(path: &Path, contents: &[u8], private: bool) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn store(dir: &Path, identity: &Identity) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let meta =
        Meta { fingerprint: identity.fingerprint(), names: identity.names.clone(), not_after: identity.not_after };
    // The key first: a certificate without its key is useless, and the
    // metadata last because it is what makes the set valid.
    write_atomic(&dir.join(KEY_FILE), pem("PRIVATE KEY", &identity.key_der).as_bytes(), true)?;
    write_atomic(&dir.join(CERT_FILE), pem("CERTIFICATE", &identity.cert_der).as_bytes(), false)?;
    write_atomic(&dir.join(META_FILE), &serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?, false)
}

/// The stored identity, or why there is none to use.
fn load(dir: &Path) -> Result<Identity, String> {
    let read = |name: &str| fs::read(dir.join(name)).map_err(|e| format!("{name}: {e}"));
    let meta: Meta = serde_json::from_slice(&read(META_FILE)?).map_err(|e| format!("{META_FILE}: {e}"))?;
    let cert = CertificateDer::from_pem_slice(&read(CERT_FILE)?).map_err(|e| format!("{CERT_FILE}: {e}"))?;
    let key = PrivatePkcs8KeyDer::from_pem_slice(&read(KEY_FILE)?).map_err(|e| format!("{KEY_FILE}: {e}"))?;
    let identity = Identity {
        cert_der: cert.to_vec(),
        key_der: key.secret_pkcs8_der().to_vec(),
        names: meta.names,
        not_after: meta.not_after,
    };
    if identity.fingerprint() != meta.fingerprint {
        return Err(format!("{META_FILE} describes another certificate"));
    }
    // Also proves that the key parses and belongs to the certificate.
    identity.server_config().map_err(|e| format!("unusable certificate or key: {e}"))?;
    Ok(identity)
}

/// Returns the identity to serve on the LAN listener for the addresses
/// `lan`, and whether it was newly generated.
///
/// The identity stored in `dir` is reused when it is intact, names every
/// address in `lan` and is more than 30 days from its expiry. Otherwise a
/// new one is generated and stored. A new certificate also keeps the
/// addresses of the one it replaces (up to 16 in total), so that a laptop
/// moving between two networks settles on one certificate instead of
/// asking for a new acceptance at every move.
///
/// Without a directory, or when it cannot be written, the identity only
/// lives as long as the process.
pub(crate) fn load_or_create(
    dir: Option<&Path>,
    lan: &[Ipv4Addr],
    now: OffsetDateTime,
) -> Result<(Identity, bool), rcgen::Error> {
    let mut names = required_names(lan);
    if let Some(dir) = dir {
        match load(dir) {
            Ok(stored) => {
                let covers = names.iter().all(|name| stored.names.contains(name));
                let fresh_enough = stored.not_after - now.unix_timestamp() > RENEW_BEFORE.whole_seconds();
                if covers && fresh_enough {
                    return Ok((stored, false));
                }
                log::info!(
                    "replacing the stored certificate: {}",
                    if covers { "it is about to expire" } else { "it does not name the current LAN addresses" }
                );
                let limit = LOCAL_NAMES.len() + MAX_ADDRESSES;
                for name in stored.names {
                    if names.len() < limit && !names.contains(&name) && name.parse::<Ipv4Addr>().is_ok() {
                        names.push(name);
                    }
                }
            }
            Err(reason) => log::debug!("no stored certificate to reuse ({reason})"),
        }
    }
    let identity = generate(names, now)?;
    if let Some(dir) = dir {
        if let Err(err) = store(dir, &identity) {
            log::warn!(
                "impossible d'enregistrer le certificat dans {} ({err}) : il changera au prochain démarrage",
                dir.display()
            );
        }
    }
    Ok((identity, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(192, 168, 1, last)
    }

    #[test]
    fn fingerprint_format() {
        let print = fingerprint(b"abc");
        assert_eq!(
            print,
            "BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
        );
    }

    #[test]
    fn stored_identity_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let (first, fresh) = load_or_create(Some(dir.path()), &[ip(20)], now()).unwrap();
        assert!(fresh);
        assert_eq!(first.names, ["localhost", "127.0.0.1", "192.168.1.20"]);
        first.server_config().unwrap();
        for name in [CERT_FILE, KEY_FILE, META_FILE] {
            assert!(dir.path().join(name).is_file(), "{name} was not written");
        }
        assert!(!dir.path().join("key.tmp").exists());

        let (second, fresh) = load_or_create(Some(dir.path()), &[ip(20)], now()).unwrap();
        assert!(!fresh);
        assert_eq!(second.fingerprint(), first.fingerprint());
        assert_eq!(second.key_der, first.key_der);
        // Fewer addresses are still covered.
        let (third, fresh) = load_or_create(Some(dir.path()), &[], now()).unwrap();
        assert!(!fresh);
        assert_eq!(third.fingerprint(), first.fingerprint());
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_create(Some(dir.path()), &[], now()).unwrap();
        let mode = fs::metadata(dir.path().join(KEY_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "key.pem is readable by others: {mode:o}");
    }

    #[test]
    fn new_addresses_regenerate_and_keep_the_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = load_or_create(Some(dir.path()), &[ip(20)], now()).unwrap();
        let (second, fresh) = load_or_create(Some(dir.path()), &[Ipv4Addr::new(10, 0, 0, 5)], now()).unwrap();
        assert!(fresh);
        assert_ne!(second.fingerprint(), first.fingerprint());
        assert_eq!(second.names, ["localhost", "127.0.0.1", "10.0.0.5", "192.168.1.20"]);
        // Back on the first network: nothing to do.
        let (third, fresh) = load_or_create(Some(dir.path()), &[ip(20)], now()).unwrap();
        assert!(!fresh);
        assert_eq!(third.fingerprint(), second.fingerprint());
    }

    #[test]
    fn address_list_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let many: Vec<Ipv4Addr> = (1..=40).map(ip).collect();
        let (first, _) = load_or_create(Some(dir.path()), &many, now()).unwrap();
        assert_eq!(first.names.len(), LOCAL_NAMES.len() + MAX_ADDRESSES);
        let (second, fresh) = load_or_create(Some(dir.path()), &[ip(200)], now()).unwrap();
        assert!(fresh);
        assert_eq!(second.names.len(), LOCAL_NAMES.len() + MAX_ADDRESSES);
        assert_eq!(second.names[2], "192.168.1.200");
    }

    #[test]
    fn certificate_near_expiry_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = load_or_create(Some(dir.path()), &[ip(20)], now()).unwrap();
        let (kept, fresh) = load_or_create(Some(dir.path()), &[ip(20)], now() + Duration::days(300)).unwrap();
        assert!(!fresh);
        assert_eq!(kept.fingerprint(), first.fingerprint());
        let later = now() + Duration::days(340);
        let (renewed, fresh) = load_or_create(Some(dir.path()), &[ip(20)], later).unwrap();
        assert!(fresh);
        assert_ne!(renewed.fingerprint(), first.fingerprint());
        assert!(renewed.not_after > later.unix_timestamp() + Duration::days(360).whole_seconds());
    }

    #[test]
    fn damaged_files_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = load_or_create(Some(dir.path()), &[], now()).unwrap();

        fs::write(dir.path().join(CERT_FILE), "not a certificate").unwrap();
        let (second, fresh) = load_or_create(Some(dir.path()), &[], now()).unwrap();
        assert!(fresh);
        assert_ne!(second.fingerprint(), first.fingerprint());

        // A key that does not belong to the certificate.
        let other = generate(required_names(&[]), now()).unwrap();
        fs::write(dir.path().join(KEY_FILE), pem("PRIVATE KEY", &other.key_der)).unwrap();
        let (third, fresh) = load_or_create(Some(dir.path()), &[], now()).unwrap();
        assert!(fresh, "a mismatched key must not be served");
        third.server_config().unwrap();

        fs::write(dir.path().join(META_FILE), "{").unwrap();
        assert!(load_or_create(Some(dir.path()), &[], now()).unwrap().1);
        fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        assert!(load_or_create(Some(dir.path()), &[], now()).unwrap().1);
    }

    #[test]
    fn works_without_a_directory_or_with_an_unwritable_one() {
        let (a, fresh) = load_or_create(None, &[ip(20)], now()).unwrap();
        assert!(fresh);
        a.server_config().unwrap();
        // A file where the directory should be.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, "x").unwrap();
        let (b, fresh) = load_or_create(Some(&blocked), &[ip(20)], now()).unwrap();
        assert!(fresh);
        b.server_config().unwrap();
    }
}
