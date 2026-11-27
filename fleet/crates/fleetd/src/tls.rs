//! The TLS listener's material and policy: the server config, the client's role from its
//! certificate, and the per-address connection limit.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use fleet_proto::{DeviceName, Role};
use rustls::RootCertStore;
use rustls::server::{ServerConfig, WebPkiClientVerifier};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

use crate::config::{Config, ConfigError, normalize_serial};
use crate::{own_uid, report_str};

const ROLE_URI_PREFIX: &str = "fleet:role:";

fn invalid(key: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.to_owned(),
        message: message.into(),
    }
}

/// The rustls config for `[tls]`, or `None` without it: TLS 1.3 only, and a client
/// certificate chaining to `[tls] ca` required. Checks the key file's mode and owner and
/// that the leaf certificate names the device.
pub fn load(cfg: &Config) -> Result<Option<Arc<ServerConfig>>, ConfigError> {
    let Some(tls) = &cfg.tls else {
        return Ok(None);
    };
    let read_certs =
        |key: &str, path: &Path| -> Result<Vec<CertificateDer<'static>>, ConfigError> {
            let certs = CertificateDer::pem_file_iter(path)
                .and_then(Iterator::collect::<Result<Vec<_>, _>>)
                .map_err(|e| invalid(key, format!("{}: {e}", path.display())))?;
            if certs.is_empty() {
                return Err(invalid(key, format!("{}: no certificate", path.display())));
            }
            Ok(certs)
        };

    let mut roots = RootCertStore::empty();
    for ca in read_certs("tls.ca", &tls.ca)? {
        roots
            .add(ca)
            .map_err(|e| invalid("tls.ca", e.to_string()))?;
    }
    let chain = read_certs("tls.cert", &tls.cert)?;
    if !names_device(&chain[0], &cfg.device) {
        return Err(invalid(
            "tls.cert",
            format!("the leaf certificate has no DNS SAN {}", cfg.device),
        ));
    }
    check_key_file(&tls.key)?;
    let key = PrivateKeyDer::from_pem_file(&tls.key)
        .map_err(|e| invalid("tls.key", format!("{}: {e}", tls.key.display())))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()
            .map_err(|e| invalid("tls.ca", e.to_string()))?;
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| invalid("tls", e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|e| invalid("tls.key", e.to_string()))?;
    Ok(Some(Arc::new(config)))
}

fn check_key_file(path: &Path) -> Result<(), ConfigError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| invalid("tls.key", format!("{}: {e}", path.display())))?;
    if meta.mode() & 0o077 != 0 {
        return Err(invalid("tls.key", "must have mode 0600 or 0400"));
    }
    if meta.uid() != own_uid() {
        return Err(invalid("tls.key", "must be owned by fleetd's user"));
    }
    Ok(())
}

fn names_device(cert: &CertificateDer<'_>, device: &DeviceName) -> bool {
    let Ok((_, cert)) = X509Certificate::from_der(cert) else {
        return false;
    };
    let Ok(Some(san)) = cert.subject_alternative_name() else {
        return false;
    };
    san.value
        .general_names
        .iter()
        .any(|n| matches!(n, GeneralName::DNSName(d) if d.eq_ignore_ascii_case(device.as_str())))
}

/// Who a verified client certificate says the peer is.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub role: Role,
    pub subject: String,
    pub serial: String,
}

/// The identity in a client certificate rustls has already verified, or why it grants
/// nothing: no role, more than one, or a revoked serial.
pub(crate) fn identify(
    leaf: &CertificateDer<'_>,
    revoked: &HashSet<String>,
) -> Result<Identity, &'static str> {
    let (_, cert) =
        X509Certificate::from_der(leaf).map_err(|_| "the certificate does not parse")?;
    let serial = normalize_serial(&cert.raw_serial_as_string()).unwrap_or_default();
    if revoked.contains(&serial) {
        return Err("the certificate is revoked");
    }
    let roles: Vec<&str> = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    GeneralName::URI(u) => u.strip_prefix(ROLE_URI_PREFIX),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let [role] = roles.as_slice() else {
        return Err("the certificate does not carry exactly one fleet role");
    };
    let role = role
        .parse()
        .map_err(|_| "the certificate's fleet role is not monitor or dispatch")?;
    Ok(Identity {
        role,
        subject: report_str(&cert.subject().to_string()),
        serial,
    })
}

/// Open tls connections per peer address.
#[derive(Clone)]
pub(crate) struct PerIp {
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
    max: usize,
}

/// One connection's share of its address's limit, given back on drop.
pub(crate) struct IpSlot {
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl PerIp {
    pub fn new(max: usize) -> Self {
        Self {
            counts: Arc::default(),
            max,
        }
    }

    pub fn acquire(&self, ip: IpAddr) -> Option<IpSlot> {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let n = counts.entry(ip).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(IpSlot {
            counts: Arc::clone(&self.counts),
            ip,
        })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = counts.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_ip_limit_counts_and_releases() {
        let limit = PerIp::new(2);
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let a = limit.acquire(ip).unwrap();
        let _b = limit.acquire(ip).unwrap();
        assert!(limit.acquire(ip).is_none());
        assert!(limit.acquire("192.0.2.2".parse().unwrap()).is_some());
        drop(a);
        assert!(limit.acquire(ip).is_some());
    }
}
