mod common;

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TempDir, make_pki, write_config};
use fleet::registry::{ConfigError, Registry, Transport, config_path};
use rustls::pki_types::ServerName;

fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
        .collect();
    move |k| map.get(k).cloned()
}

fn load(path: &Path) -> Result<Registry, ConfigError> {
    Registry::load(path, &env(&[("HOME", "/home/dev")]))
}

fn load_err(text: &str) -> String {
    let dir = TempDir::new();
    let path = write_config(dir.path(), text);
    match load(&path) {
        Ok(_) => panic!("loaded:\n{text}"),
        Err(e) => e.0,
    }
}

#[test]
fn config_path_precedence() {
    let all = env(&[
        ("FLEET_CONFIG", "/etc/fleet.toml"),
        ("HARNESS_INSTANCE_DIR", "/srv/instance"),
    ]);
    assert_eq!(
        config_path(Some(Path::new("x.toml")), &all).unwrap(),
        PathBuf::from("x.toml")
    );
    assert_eq!(
        config_path(None, &all).unwrap(),
        PathBuf::from("/etc/fleet.toml")
    );
    let instance = env(&[
        ("FLEET_CONFIG", ""),
        ("HARNESS_INSTANCE_DIR", "/srv/instance"),
    ]);
    assert_eq!(
        config_path(None, &instance).unwrap(),
        PathBuf::from("/srv/instance/fleet.toml")
    );
    assert!(config_path(None, &env(&[])).is_err());
}

#[test]
fn an_empty_variable_counts_as_unset() {
    let instance = env(&[
        ("FLEET_CONFIG", ""),
        ("HARNESS_INSTANCE_DIR", "/srv/instance"),
    ]);
    assert_eq!(
        config_path(None, &instance).unwrap(),
        PathBuf::from("/srv/instance/fleet.toml")
    );
    // An empty instance dir must not resolve to a relative `fleet.toml` in the cwd.
    let both_empty = env(&[("FLEET_CONFIG", ""), ("HARNESS_INSTANCE_DIR", "")]);
    let e = config_path(None, &both_empty).unwrap_err();
    assert!(e.0.contains("no fleet.toml"), "{}", e.0);
    let config_only = env(&[
        ("FLEET_CONFIG", "/etc/fleet.toml"),
        ("HARNESS_INSTANCE_DIR", ""),
    ]);
    assert_eq!(
        config_path(None, &config_only).unwrap(),
        PathBuf::from("/etc/fleet.toml")
    );
}

#[test]
fn loads_every_transport_in_file_order_with_defaults() {
    let dir = TempDir::new();
    make_pki(&dir.join("pki"));
    let path = write_config(
        dir.path(),
        r#"
request_timeout_s = 5

[tls]
ca = "pki/ca.pem"
cert = "pki/hub.pem"
key = "pki/hub.key"

[devices.zeta]
transport = "local"

[devices.dev-a]
transport = "tls"
address = "192.0.2.10:7443"

[devices.alpha]
transport = "ssh"
target = "dev@dev-b.example.com"
ssh_options = ["Port=2222"]
auto = false

[devices.here]
transport = "local"
socket = "~/run/fleetd.sock"
"#,
    );
    let reg = Registry::load(
        &path,
        &env(&[("HOME", "/home/dev"), ("XDG_STATE_HOME", "/state")]),
    )
    .unwrap();
    assert_eq!(reg.path, path);
    assert_eq!(reg.timeouts.connect, Duration::from_secs(10));
    assert_eq!(reg.timeouts.request, Duration::from_secs(5));
    assert_eq!(reg.timeouts.stream_idle, Duration::from_secs(120));
    let names: Vec<&str> = reg.devices.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["zeta", "dev-a", "alpha", "here"]);
    let autos: Vec<bool> = reg.devices.iter().map(|d| d.auto).collect();
    assert_eq!(autos, [true, true, false, true]);

    let Transport::Local { socket } = &reg.devices[0].transport else {
        panic!()
    };
    assert_eq!(socket, Path::new("/state/fleetd/fleetd.sock"));
    let Transport::Tls {
        address,
        server_name,
        ..
    } = &reg.devices[1].transport
    else {
        panic!()
    };
    assert_eq!(address, "192.0.2.10:7443");
    assert_eq!(server_name, &ServerName::try_from("dev-a").unwrap());
    let Transport::Ssh {
        program,
        target,
        options,
        remote_command,
    } = &reg.devices[2].transport
    else {
        panic!()
    };
    assert_eq!(program, Path::new("ssh"));
    assert_eq!(target, "dev@dev-b.example.com");
    assert_eq!(options, &["Port=2222"]);
    assert_eq!(remote_command, &["fleetd", "--stdio"]);
    let Transport::Local { socket } = &reg.devices[3].transport else {
        panic!()
    };
    assert_eq!(socket, Path::new("/home/dev/run/fleetd.sock"));
}

#[test]
fn socket_default_falls_back_to_home_and_relative_paths_to_the_config_dir() {
    let dir = TempDir::new();
    let path = write_config(
        dir.path(),
        "ssh = \"bin/ssh\"\n[devices.a]\ntransport = \"local\"\n[devices.b]\ntransport = \"ssh\"\ntarget = \"b\"\n",
    );
    let reg = load(&path).unwrap();
    let Transport::Local { socket } = &reg.devices[0].transport else {
        panic!()
    };
    assert_eq!(
        socket,
        Path::new("/home/dev/.local/state/fleetd/fleetd.sock")
    );
    let Transport::Ssh { program, .. } = &reg.devices[1].transport else {
        panic!()
    };
    assert_eq!(program, &dir.join("bin/ssh"));
}

#[test]
fn an_empty_registry_loads() {
    let dir = TempDir::new();
    assert!(
        load(&write_config(dir.path(), ""))
            .unwrap()
            .devices
            .is_empty()
    );
}

#[test]
fn errors_name_the_key() {
    let cases: &[(&str, &str)] = &[
        ("colour = 1", "colour"),
        ("connect_timeout_s = 0", "connect_timeout_s"),
        ("request_timeout_s = 3601", "request_timeout_s"),
        ("stream_idle_timeout_s = 59", "stream_idle_timeout_s"),
        ("connect_timeout_s = \"10\"", "connect_timeout_s"),
        ("[devices.Dev]\ntransport = \"local\"", "devices.Dev"),
        ("[devices.auto]\ntransport = \"local\"", "devices.auto"),
        ("[devices.a]\ntransport = \"carrier-pigeon\"", "devices.a"),
        ("[devices.a]\ntransport = \"local\"\nport = 1", "port"),
        (
            "[devices.a]\ntransport = \"local\"\ntarget = \"x\"",
            "devices.a.target",
        ),
        ("[devices.a]\ntransport = \"ssh\"", "devices.a.target"),
        (
            "[devices.a]\ntransport = \"ssh\"\ntarget = \"-oProxyCommand=x\"",
            "devices.a.target",
        ),
        (
            "[devices.a]\ntransport = \"ssh\"\ntarget = \"a b\"",
            "devices.a.target",
        ),
        (
            "[devices.a]\ntransport = \"ssh\"\ntarget = \"a\"\nssh_options = [\"-v\"]",
            "devices.a.ssh_options",
        ),
        (
            "[devices.a]\ntransport = \"ssh\"\ntarget = \"a\"\nremote_command = []",
            "devices.a.remote_command",
        ),
        ("[devices.a]\ntransport = \"tls\"", "devices.a.address"),
        (
            "[devices.a]\ntransport = \"tls\"\naddress = \"dev-a\"",
            "devices.a.address",
        ),
        (
            "[devices.a]\ntransport = \"tls\"\naddress = \"dev-a:0\"",
            "devices.a.address",
        ),
        (
            "[devices.a]\ntransport = \"tls\"\naddress = \"::1:7443\"",
            "devices.a.address",
        ),
        (
            "[devices.a]\ntransport = \"tls\"\naddress = \"dev-a:7443\"",
            "devices.a.ca",
        ),
        (
            "[devices.a]\ntransport = \"tls\"\naddress = \"dev-a:7443\"\nserver_name = \"not a name\"",
            "devices.a.server_name",
        ),
        ("[tls]\nca = \"ca.pem\"", "cert"),
        ("ssh = \"\"", "ssh"),
    ];
    for (text, key) in cases {
        let message = load_err(text);
        assert!(
            message.contains(key),
            "{text:?}: {message:?} does not name {key}"
        );
    }
}

#[test]
fn a_missing_file_is_an_error() {
    let dir = TempDir::new();
    let e = load(&dir.join("absent.toml")).err().unwrap();
    assert!(e.0.contains("absent.toml"), "{}", e.0);
}

#[test]
fn tls_identity_files_are_checked_at_load() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("pki"));
    let text = |key: &Path| {
        format!(
            "[tls]\nca = \"pki/ca.pem\"\ncert = \"pki/hub.pem\"\nkey = \"{}\"\n\
             [devices.dev-a]\ntransport = \"tls\"\naddress = \"127.0.0.1:7443\"\n",
            key.display()
        )
    };
    let path = write_config(dir.path(), &text(&pki.client_key));
    assert!(load(&path).is_ok());

    std::fs::set_permissions(&pki.client_key, std::fs::Permissions::from_mode(0o640)).unwrap();
    let e = load(&path).err().unwrap().0;
    assert!(e.contains("tls.key") && e.contains("group or other"), "{e}");

    std::fs::set_permissions(&pki.client_key, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(load(&path).is_ok());

    // A key that is not the certificate's.
    let path = write_config(dir.path(), &text(&pki.server_key));
    let e = load(&path).err().unwrap().0;
    assert!(e.contains("devices.dev-a"), "{e}");

    let path = write_config(dir.path(), &text(&dir.join("pki/none.key")));
    let e = load(&path).err().unwrap().0;
    assert!(e.contains("tls.key"), "{e}");
}

#[test]
fn a_device_can_bring_its_own_identity() {
    let dir = TempDir::new();
    make_pki(&dir.join("pki"));
    let path = write_config(
        dir.path(),
        "[devices.dev-a]\ntransport = \"tls\"\naddress = \"127.0.0.1:7443\"\n\
         server_name = \"dev-a.example.com\"\n\
         ca = \"pki/ca.pem\"\ncert = \"pki/hub.pem\"\nkey = \"pki/hub.key\"\n",
    );
    let reg = load(&path).unwrap();
    let Transport::Tls { server_name, .. } = &reg.devices[0].transport else {
        panic!()
    };
    assert_eq!(
        server_name,
        &ServerName::try_from("dev-a.example.com").unwrap()
    );
}
