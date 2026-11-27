//! `fleet pki`: the private CA and the server and client certificates it issues.

use std::fmt::Write as _;
use std::fs::{self, DirBuilder};
use std::io::{self, Write as _};
use std::net::IpAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fleet_proto::{DeviceName, ROLE_URI_PREFIX, Role};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::pki_types::DnsName;
use time::OffsetDateTime;

use crate::files;

pub const CA_DAYS: u32 = 3650;
/// Browsers' ceiling for a leaf, and short enough that a lost key ages out.
pub const LEAF_DAYS: u32 = 397;
pub const MAX_DAYS: u32 = 36_500;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PkiError(String);

fn fail(msg: impl Into<String>) -> PkiError {
    PkiError(msg.into())
}

/// What a `pki` command wrote.
#[derive(Debug)]
pub struct Issued {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// The certificate's serial in lowercase hex, as `revoked_serials` takes it.
    pub serial: String,
}

/// `fleet pki init`: a new CA, `ca.pem` and `ca.key`.
pub fn init(dir: &Path, days: u32) -> Result<Issued, PkiError> {
    let mut params = CertificateParams::default();
    let serial = validity_and_serial(&mut params, days)?;
    // Unique per CA: verifiers find an issuer by name, so two CAs sharing one would make a
    // certificate from the wrong CA fail as a bad signature instead of an unknown issuer.
    params.distinguished_name = name(&format!("fleet CA {}", &serial[..16]));
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().map_err(|e| fail(format!("key generation: {e}")))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| fail(format!("signing: {e}")))?;
    write_pair(dir, "ca", &cert.pem(), &key.serialize_pem(), serial)
}

/// `fleet pki issue-server`: `<device>.pem` and `<device>.key`, with DNS SANs for the
/// device name and each of `dns`, IP SANs for each of `ips`, and serverAuth.
pub fn issue_server(
    dir: &Path,
    device: &DeviceName,
    dns: &[String],
    ips: &[IpAddr],
    days: u32,
) -> Result<Issued, PkiError> {
    let mut sans = vec![dns_san(device.as_str())?];
    for d in dns {
        sans.push(dns_san(d)?);
    }
    sans.extend(ips.iter().copied().map(SanType::IpAddress));
    issue(
        dir,
        device.as_str(),
        sans,
        ExtendedKeyUsagePurpose::ServerAuth,
        days,
    )
}

/// `fleet pki issue-client`: `<name>.pem` and `<name>.key`, with the role as the SAN URI
/// `fleet:role:<role>`, and clientAuth.
pub fn issue_client(
    dir: &Path,
    name: &DeviceName,
    role: Role,
    days: u32,
) -> Result<Issued, PkiError> {
    let uri = format!("{ROLE_URI_PREFIX}{role}");
    let san = SanType::URI(uri.try_into().map_err(|e| fail(format!("role URI: {e}")))?);
    issue(
        dir,
        name.as_str(),
        vec![san],
        ExtendedKeyUsagePurpose::ClientAuth,
        days,
    )
}

fn issue(
    dir: &Path,
    stem: &str,
    sans: Vec<SanType>,
    usage: ExtendedKeyUsagePurpose,
    days: u32,
) -> Result<Issued, PkiError> {
    let issuer = load_ca(dir)?;
    let mut params = CertificateParams::default();
    params.distinguished_name = name(stem);
    params.subject_alt_names = sans;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![usage];
    params.use_authority_key_identifier_extension = true;
    let serial = validity_and_serial(&mut params, days)?;
    let key = KeyPair::generate().map_err(|e| fail(format!("key generation: {e}")))?;
    let cert = params
        .signed_by(&key, &issuer)
        .map_err(|e| fail(format!("signing: {e}")))?;
    write_pair(dir, stem, &cert.pem(), &key.serialize_pem(), serial)
}

fn load_ca(dir: &Path) -> Result<Issuer<'static, KeyPair>, PkiError> {
    let cert_path = dir.join("ca.pem");
    let key_path = dir.join("ca.key");
    let read_err = |p: &Path, e: io::Error| fail(format!("{}: {e}", p.display()));
    let cert = files::read_capped(&cert_path, files::MAX_CONFIG_LEN)
        .map_err(|e| read_err(&cert_path, e))?;
    let key = files::read_private_key(&key_path).map_err(|e| read_err(&key_path, e))?;
    let utf8 = |p: &Path, b: Vec<u8>| {
        String::from_utf8(b).map_err(|_| fail(format!("{}: not PEM", p.display())))
    };
    let key = KeyPair::from_pem(&utf8(&key_path, key)?)
        .map_err(|e| fail(format!("{}: {e}", key_path.display())))?;
    Issuer::from_ca_cert_pem(&utf8(&cert_path, cert)?, key)
        .map_err(|e| fail(format!("{}: {e}", cert_path.display())))
}

fn name(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn
}

fn dns_san(s: &str) -> Result<SanType, PkiError> {
    DnsName::try_from(s).map_err(|_| fail(format!("{s:?} is not a DNS name")))?;
    Ok(SanType::DnsName(
        s.try_into().map_err(|e| fail(format!("{s:?}: {e}")))?,
    ))
}

/// Sets the validity, from a few minutes ago (for clock skew) to `days` from now, and a
/// random serial, which it returns in hex.
fn validity_and_serial(params: &mut CertificateParams, days: u32) -> Result<String, PkiError> {
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(fail(format!("--days must be between 1 and {MAX_DAYS}")));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| fail("the clock is before 1970"))?;
    let now = OffsetDateTime::from_unix_timestamp(i64::try_from(now.as_secs()).unwrap_or(i64::MAX))
        .map_err(|e| fail(format!("clock: {e}")))?;
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(i64::from(days));

    let mut serial = [0u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut serial)
        .map_err(|_| fail("no system randomness"))?;
    // Positive with no leading zero byte, so the DER integer is exactly these 16 bytes and
    // the hex printed here is the hex any tool reads back.
    serial[0] = (serial[0] & 0x7f) | 0x40;
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    Ok(serial.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// Writes `<stem>.pem` (0644) and `<stem>.key` (0600) into `dir`, creating it 0700. Nothing
/// existing is overwritten: either file already present fails the whole command.
fn write_pair(
    dir: &Path,
    stem: &str,
    cert_pem: &str,
    key_pem: &str,
    serial: String,
) -> Result<Issued, PkiError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| fail(format!("{}: {e}", dir.display())))?;
    let cert = dir.join(format!("{stem}.pem"));
    let key = dir.join(format!("{stem}.key"));
    for p in [&cert, &key] {
        if fs::symlink_metadata(p).is_ok() {
            return Err(fail(format!(
                "{} exists; refusing to overwrite it",
                p.display()
            )));
        }
    }
    let create = |p: &Path, mode| {
        files::create_new(p, mode).map_err(|e| fail(format!("{}: {e}", p.display())))
    };
    let mut key_file = create(&key, 0o600)?;
    let mut cert_file = match create(&cert, 0o644) {
        Ok(f) => f,
        Err(e) => {
            let _ = fs::remove_file(&key);
            return Err(e);
        }
    };
    let written = key_file
        .write_all(key_pem.as_bytes())
        .and_then(|()| key_file.sync_all())
        .and_then(|()| cert_file.write_all(cert_pem.as_bytes()))
        .and_then(|()| cert_file.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&key);
        let _ = fs::remove_file(&cert);
        return Err(fail(format!("writing {}: {e}", dir.display())));
    }
    Ok(Issued { cert, key, serial })
}
