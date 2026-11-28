mod common;

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use common::{TempDir, make_pki};
use fleet::pki;
use fleet::transport::read_certs;
use fleet_proto::Role;
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

fn roots(ca: &Path) -> Arc<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for c in read_certs(ca).unwrap() {
        roots.add(c).unwrap();
    }
    Arc::new(roots)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn leaf(path: &Path) -> CertificateDer<'static> {
    read_certs(path).unwrap().remove(0)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o777
}

fn sans(der: &CertificateDer<'_>) -> Vec<String> {
    let (_, cert) = X509Certificate::from_der(der).unwrap();
    cert.subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names
        .iter()
        .map(|n| match n {
            GeneralName::DNSName(d) => format!("dns:{d}"),
            GeneralName::URI(u) => format!("uri:{u}"),
            GeneralName::IPAddress(ip) => format!("ip:{ip:?}"),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn server_certificate_chains_to_the_ca_for_its_names_only() {
    let dir = TempDir::new();
    pki::init(dir.path(), 30).unwrap();
    let server = pki::issue_server(
        dir.path(),
        &"dev-a".parse().unwrap(),
        &["dev-a.example.com".into()],
        &[IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))],
        pki::LEAF_DAYS,
    )
    .unwrap();
    let verifier =
        WebPkiServerVerifier::builder_with_provider(roots(&dir.join("ca.pem")), provider())
            .build()
            .unwrap();
    let cert = leaf(&server.cert);
    let now = UnixTime::now();
    for name in ["dev-a", "dev-a.example.com", "192.0.2.10"] {
        let name = ServerName::try_from(name).unwrap();
        verifier
            .verify_server_cert(&cert, &[], &name, &[], now)
            .unwrap_or_else(|e| panic!("{name:?}: {e}"));
    }
    let other = ServerName::try_from("dev-b").unwrap();
    assert!(
        verifier
            .verify_server_cert(&cert, &[], &other, &[], now)
            .is_err()
    );

    assert_eq!(
        sans(&cert),
        ["dns:dev-a", "dns:dev-a.example.com", "ip:[192, 0, 2, 10]"]
    );
    let (_, parsed) = X509Certificate::from_der(&cert).unwrap();
    let eku = parsed.extended_key_usage().unwrap().unwrap().value;
    assert!(eku.server_auth && !eku.client_auth);
    let days = (parsed.validity().not_after.timestamp() - parsed.validity().not_before.timestamp())
        / 86_400;
    assert_eq!(days, i64::from(pki::LEAF_DAYS));
    assert_eq!(hex(parsed.raw_serial()), server.serial);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[test]
fn client_certificate_carries_its_role_and_chains_to_the_ca() {
    let dir = TempDir::new();
    pki::init(dir.path(), 30).unwrap();
    let verifier =
        WebPkiClientVerifier::builder_with_provider(roots(&dir.join("ca.pem")), provider())
            .build()
            .unwrap();
    for (name, role) in [("hub", Role::Dispatch), ("dashboard", Role::Monitor)] {
        let issued = pki::issue_client(dir.path(), &name.parse().unwrap(), role, 30).unwrap();
        let cert = leaf(&issued.cert);
        verifier
            .verify_client_cert(&cert, &[], UnixTime::now())
            .unwrap();
        assert_eq!(sans(&cert), [format!("uri:fleet:role:{role}")]);
        let (_, parsed) = X509Certificate::from_der(&cert).unwrap();
        let eku = parsed.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);
        assert_eq!(hex(parsed.raw_serial()), issued.serial);
    }
}

#[test]
fn ca_is_a_ca_and_serials_differ() {
    let dir = TempDir::new();
    let ca = pki::init(dir.path(), pki::CA_DAYS).unwrap();
    let der = leaf(&ca.cert);
    let (_, parsed) = X509Certificate::from_der(&der).unwrap();
    assert!(parsed.basic_constraints().unwrap().unwrap().value.ca);
    assert_eq!(ca.serial.len(), 32);
    let a = pki::issue_client(dir.path(), &"a".parse().unwrap(), Role::Monitor, 1).unwrap();
    let b = pki::issue_client(dir.path(), &"b".parse().unwrap(), Role::Monitor, 1).unwrap();
    assert_ne!(a.serial, b.serial);
    assert_ne!(a.serial, ca.serial);
}

#[test]
fn keys_are_private_and_the_directory_is_created_0700() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("nested/pki"));
    assert_eq!(mode(&pki.dir), 0o700);
    for key in [
        dir.join("nested/pki/ca.key"),
        pki.server_key,
        pki.client_key,
    ] {
        assert_eq!(mode(&key), 0o600, "{}", key.display());
    }
    assert_eq!(mode(&pki.ca), 0o644);
}

#[test]
fn nothing_is_overwritten() {
    let dir = TempDir::new();
    pki::init(dir.path(), 30).unwrap();
    let before = std::fs::read(dir.join("ca.key")).unwrap();
    let e = pki::init(dir.path(), 30).unwrap_err().to_string();
    assert!(e.contains("refusing to overwrite"), "{e}");
    assert_eq!(std::fs::read(dir.join("ca.key")).unwrap(), before);

    let name = "hub".parse().unwrap();
    pki::issue_client(dir.path(), &name, Role::Monitor, 30).unwrap();
    let cert = std::fs::read(dir.join("hub.pem")).unwrap();
    assert!(pki::issue_client(dir.path(), &name, Role::Dispatch, 30).is_err());
    assert_eq!(std::fs::read(dir.join("hub.pem")).unwrap(), cert);

    // Only the key left over from an interrupted run still blocks a new pair.
    std::fs::write(dir.join("dev-a.key"), "").unwrap();
    assert!(pki::issue_server(dir.path(), &"dev-a".parse().unwrap(), &[], &[], 30).is_err());
    assert!(!dir.join("dev-a.pem").exists());
}

#[test]
fn a_ca_key_others_can_read_is_refused() {
    let dir = TempDir::new();
    pki::init(dir.path(), 30).unwrap();
    std::fs::set_permissions(dir.join("ca.key"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let e = pki::issue_client(dir.path(), &"hub".parse().unwrap(), Role::Monitor, 30)
        .unwrap_err()
        .to_string();
    assert!(e.contains("group or other"), "{e}");
    assert!(!dir.join("hub.key").exists());
}

#[test]
fn bad_inputs_are_refused() {
    let dir = TempDir::new();
    assert!(pki::init(dir.path(), 0).is_err());
    assert!(pki::init(dir.path(), pki::MAX_DAYS + 1).is_err());
    pki::init(dir.path(), 30).unwrap();
    let e = pki::issue_server(
        dir.path(),
        &"dev-a".parse().unwrap(),
        &["not a name".into()],
        &[],
        30,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("not a DNS name"), "{e}");
    // Without a CA there is nothing to issue from.
    let empty = TempDir::new();
    assert!(pki::issue_client(empty.path(), &"hub".parse().unwrap(), Role::Monitor, 30).is_err());
}
