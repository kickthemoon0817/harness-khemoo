//! fleet.toml: the device registry and the client TLS identity.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use fleet_proto::DeviceName;
use fleet_proto::output::TransportKind;
use rustls::ClientConfig;
use rustls::pki_types::{DnsName, ServerName};
use serde::Deserialize;

use crate::files;
use crate::transport;

/// A config error. Its message names the offending key; fleet exits 2 on it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError(msg.into())
}

/// A loaded and validated fleet.toml.
pub struct Registry {
    /// The file it was loaded from; `fleet pki` writes beside it.
    pub path: PathBuf,
    pub timeouts: Timeouts,
    /// In file order, which is the registry order everywhere.
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Connect, transport handshake and `Hello`.
    pub connect: Duration,
    /// A single-frame answer.
    pub request: Duration,
    /// The longest a stream may go without a frame.
    pub stream_idle: Duration,
}

pub struct Device {
    pub name: DeviceName,
    /// Whether `--device auto` may pick it.
    pub auto: bool,
    pub transport: Transport,
}

pub enum Transport {
    Local {
        socket: PathBuf,
    },
    Ssh {
        program: PathBuf,
        target: String,
        options: Vec<String>,
        remote_command: Vec<String>,
    },
    Tls {
        address: String,
        server_name: ServerName<'static>,
        config: Arc<ClientConfig>,
    },
}

impl Transport {
    pub fn kind(&self) -> TransportKind {
        match self {
            Self::Local { .. } => TransportKind::Local,
            Self::Ssh { .. } => TransportKind::Ssh,
            Self::Tls { .. } => TransportKind::Tls,
        }
    }
}

impl Registry {
    pub fn device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name.as_str() == name)
    }

    /// Loads and validates `path`, reading the TLS identities it names. `env` looks up
    /// `HOME` and `XDG_STATE_HOME`.
    pub fn load(path: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        let text = files::read_capped(path, files::MAX_CONFIG_LEN)
            .map_err(|e| err(format!("{}: {e}", path.display())))?;
        let text =
            String::from_utf8(text).map_err(|_| err(format!("{}: not UTF-8", path.display())))?;
        let raw: RawRegistry =
            toml::from_str(&text).map_err(|e| err(format!("{}: {e}", path.display())))?;
        let base = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let ctx = Ctx {
            base,
            home: non_empty(env("HOME")).map(PathBuf::from),
        };
        build(path, &raw, &ctx, env)
    }
}

/// Where fleet.toml is: `--config`, else `$FLEET_CONFIG`, else
/// `$HARNESS_INSTANCE_DIR/fleet.toml`. An empty variable counts as unset.
pub fn config_path(
    flag: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = non_empty(env("FLEET_CONFIG")) {
        return Ok(PathBuf::from(p));
    }
    if let Some(dir) = non_empty(env("HARNESS_INSTANCE_DIR")) {
        return Ok(PathBuf::from(dir).join("fleet.toml"));
    }
    Err(err(
        "no fleet.toml: pass --config, or set FLEET_CONFIG or HARNESS_INSTANCE_DIR",
    ))
}

fn non_empty(v: Option<OsString>) -> Option<OsString> {
    v.filter(|v| !v.is_empty())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    connect_timeout_s: Option<u64>,
    request_timeout_s: Option<u64>,
    stream_idle_timeout_s: Option<u64>,
    ssh: Option<String>,
    tls: Option<RawTls>,
    /// Ordered as in the file; each value is checked as a [`RawDevice`] in [`build`].
    #[serde(default)]
    devices: toml::Table,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    ca: String,
    cert: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    transport: TransportKind,
    auto: Option<bool>,
    socket: Option<String>,
    target: Option<String>,
    ssh_options: Option<Vec<String>>,
    remote_command: Option<Vec<String>>,
    address: Option<String>,
    server_name: Option<String>,
    ca: Option<String>,
    cert: Option<String>,
    key: Option<String>,
}

struct Ctx {
    base: PathBuf,
    home: Option<PathBuf>,
}

impl Ctx {
    /// Resolves a configured path: `~/` against `$HOME`, a relative one against the
    /// directory of fleet.toml.
    fn path(&self, key: &str, raw: &str) -> Result<PathBuf, ConfigError> {
        if raw.is_empty() || raw.contains('\0') {
            return Err(err(format!("{key}: not a valid path")));
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            let home = self
                .home
                .as_ref()
                .ok_or_else(|| err(format!("{key}: ~/ needs HOME set")))?;
            return Ok(home.join(rest));
        }
        let p = Path::new(raw);
        Ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base.join(p)
        })
    }
}

fn range(
    key: &str,
    v: Option<u64>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<Duration, ConfigError> {
    let v = v.unwrap_or(default);
    if (min..=max).contains(&v) {
        Ok(Duration::from_secs(v))
    } else if max == u64::MAX {
        Err(err(format!("{key}: must be at least {min}")))
    } else {
        Err(err(format!("{key}: must be between {min} and {max}")))
    }
}

fn build(
    path: &Path,
    raw: &RawRegistry,
    ctx: &Ctx,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Registry, ConfigError> {
    let timeouts = Timeouts {
        connect: range("connect_timeout_s", raw.connect_timeout_s, 10, 1, 300)?,
        request: range("request_timeout_s", raw.request_timeout_s, 30, 1, 3600)?,
        stream_idle: range(
            "stream_idle_timeout_s",
            raw.stream_idle_timeout_s,
            120,
            60,
            u64::MAX,
        )?,
    };
    let ssh = match raw.ssh.as_deref() {
        None => PathBuf::from("ssh"),
        Some("") => return Err(err("ssh: must not be empty")),
        // A bare name is looked up in PATH when spawned; anything with a slash is a path.
        Some(name) if !name.contains('/') => PathBuf::from(name),
        Some(p) => ctx.path("ssh", p)?,
    };
    // Devices sharing an identity share one client config.
    let mut identities: HashMap<[PathBuf; 3], Arc<ClientConfig>> = HashMap::new();
    let mut devices = Vec::with_capacity(raw.devices.len());
    for (name, dev) in &raw.devices {
        let key = format!("devices.{name}");
        let dev: RawDevice = dev
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| err(format!("{key}: {}", e.message())))?;
        let dev = &dev;
        let name = DeviceName::from_str(name).map_err(|e| err(format!("{key}: {e}")))?;
        if name.as_str() == "auto" {
            return Err(err(format!("{key}: auto is reserved for --device auto")));
        }
        let transport = match dev.transport {
            TransportKind::Local => {
                only(&key, dev, &["socket"])?;
                let socket = match &dev.socket {
                    Some(s) => ctx.path(&format!("{key}.socket"), s)?,
                    None => default_socket(ctx, env).ok_or_else(|| {
                        err(format!(
                            "{key}.socket: no default without HOME or XDG_STATE_HOME"
                        ))
                    })?,
                };
                Transport::Local { socket }
            }
            TransportKind::Ssh => {
                only(&key, dev, &["target", "ssh_options", "remote_command"])?;
                ssh_transport(&key, dev, &ssh)?
            }
            TransportKind::Tls => {
                only(&key, dev, &["address", "server_name", "ca", "cert", "key"])?;
                tls_transport(&key, &name, dev, raw.tls.as_ref(), ctx, &mut identities)?
            }
        };
        devices.push(Device {
            name,
            auto: dev.auto.unwrap_or(true),
            transport,
        });
    }
    Ok(Registry {
        path: path.to_path_buf(),
        timeouts,
        devices,
    })
}

/// Rejects keys that belong to another transport, so a typo'd table fails loudly instead
/// of silently using defaults.
fn only(key: &str, dev: &RawDevice, allowed: &[&str]) -> Result<(), ConfigError> {
    let set = [
        ("socket", dev.socket.is_some()),
        ("target", dev.target.is_some()),
        ("ssh_options", dev.ssh_options.is_some()),
        ("remote_command", dev.remote_command.is_some()),
        ("address", dev.address.is_some()),
        ("server_name", dev.server_name.is_some()),
        ("ca", dev.ca.is_some()),
        ("cert", dev.cert.is_some()),
        ("key", dev.key.is_some()),
    ];
    match set
        .iter()
        .find(|(k, present)| *present && !allowed.contains(k))
    {
        Some((k, _)) => Err(err(format!(
            "{key}.{k}: does not apply to transport {}",
            transport_name(dev.transport)
        ))),
        None => Ok(()),
    }
}

fn transport_name(t: TransportKind) -> &'static str {
    match t {
        TransportKind::Local => "local",
        TransportKind::Ssh => "ssh",
        TransportKind::Tls => "tls",
    }
}

fn default_socket(ctx: &Ctx, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let state = non_empty(env("XDG_STATE_HOME"))
        .map(PathBuf::from)
        .or_else(|| ctx.home.as_ref().map(|h| h.join(".local/state")))?;
    Some(state.join("fleetd/fleetd.sock"))
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

/// ssh options a device may not set: each would forward the hub's agent, display or ports to
/// the device, run a local command or load a provider library, pull in config that could, reuse
/// or open a control socket, send the hub's environment, or weaken host key checking.
const SSH_DENIED_OPTIONS: &[&str] = &[
    "ForwardAgent",
    "ForwardX11",
    "ForwardX11Trusted",
    "ProxyCommand",
    "LocalCommand",
    "PermitLocalCommand",
    "RemoteCommand",
    "LocalForward",
    "RemoteForward",
    "DynamicForward",
    "Tunnel",
    "KnownHostsCommand",
    "Include",
    "Match",
    "PKCS11Provider",
    "SecurityKeyProvider",
    "ProxyJump",
    "ControlPath",
    "ControlMaster",
    "ControlPersist",
    "SendEnv",
    "SetEnv",
    "StrictHostKeyChecking",
    "UserKnownHostsFile",
    "GlobalKnownHostsFile",
    "HostKeyAlias",
    "VerifyHostKeyDNS",
    "CheckHostIP",
];

fn ssh_transport(key: &str, dev: &RawDevice, program: &Path) -> Result<Transport, ConfigError> {
    let target = dev
        .target
        .clone()
        .ok_or_else(|| err(format!("{key}.target: required for transport ssh")))?;
    if target.is_empty()
        || target.starts_with('-')
        || target.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(err(format!(
            "{key}.target: must be host or user@host, not starting with -"
        )));
    }
    let options = dev.ssh_options.clone().unwrap_or_default();
    for o in &options {
        let name = match o.split_once('=') {
            Some((k, _))
                if !k.is_empty()
                    && k.bytes().all(|b| b.is_ascii_alphanumeric())
                    && !has_control(o) =>
            {
                k
            }
            _ => return Err(err(format!("{key}.ssh_options: each must be Key=value"))),
        };
        if let Some(denied) = SSH_DENIED_OPTIONS
            .iter()
            .find(|d| d.eq_ignore_ascii_case(name))
        {
            return Err(err(format!(
                "{key}.ssh_options: {denied} is not allowed: it forwards to the device, runs a \
                 command, or reads more config"
            )));
        }
    }
    let remote_command = dev
        .remote_command
        .clone()
        .unwrap_or_else(|| vec!["fleetd".into(), "--stdio".into()]);
    if remote_command.is_empty()
        || remote_command
            .iter()
            .any(|a| a.is_empty() || has_control(a))
    {
        return Err(err(format!(
            "{key}.remote_command: must be a non-empty list of non-empty strings"
        )));
    }
    Ok(Transport::Ssh {
        program: program.to_path_buf(),
        target,
        options,
        remote_command,
    })
}

fn tls_transport(
    key: &str,
    name: &DeviceName,
    dev: &RawDevice,
    tls: Option<&RawTls>,
    ctx: &Ctx,
    identities: &mut HashMap<[PathBuf; 3], Arc<ClientConfig>>,
) -> Result<Transport, ConfigError> {
    let address = dev
        .address
        .clone()
        .ok_or_else(|| err(format!("{key}.address: required for transport tls")))?;
    // An IPv6 host needs brackets, or its last group would read as the port.
    let valid_address = address.rsplit_once(':').is_some_and(|(host, port)| {
        let host_ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            Some(v6) => v6.parse::<std::net::Ipv6Addr>().is_ok(),
            None => !host.is_empty() && !host.contains(':') && !has_control(host),
        };
        host_ok && port.parse::<u16>().is_ok_and(|p| p != 0)
    });
    if !valid_address {
        return Err(err(format!(
            "{key}.address: must be host:port, with an IPv6 host in brackets"
        )));
    }
    let server_name = dev.server_name.clone().unwrap_or_else(|| name.to_string());
    let server_name = DnsName::try_from(server_name)
        .map_err(|_| err(format!("{key}.server_name: not a DNS name")))?;

    let pick = |field: &str, own: Option<&String>, shared: Option<&String>| {
        let raw = own.or(shared).ok_or_else(|| {
            err(format!(
                "{key}.{field}: required for transport tls, here or in [tls]"
            ))
        })?;
        let k = if own.is_some() {
            format!("{key}.{field}")
        } else {
            format!("tls.{field}")
        };
        Ok::<_, ConfigError>((ctx.path(&k, raw)?, k))
    };
    let (ca, ca_key) = pick("ca", dev.ca.as_ref(), tls.map(|t| &t.ca))?;
    let (cert, cert_key) = pick("cert", dev.cert.as_ref(), tls.map(|t| &t.cert))?;
    let (pkey, pkey_key) = pick("key", dev.key.as_ref(), tls.map(|t| &t.key))?;
    let id = [ca, cert, pkey];
    let config = if let Some(c) = identities.get(&id) {
        Arc::clone(c)
    } else {
        let [ca, cert, pkey] = &id;
        let c = transport::client_config(ca, cert, pkey).map_err(|e| {
            let k = match e.file {
                transport::IdentityFile::Ca => ca_key.as_str(),
                transport::IdentityFile::Cert => cert_key.as_str(),
                transport::IdentityFile::Key => pkey_key.as_str(),
                transport::IdentityFile::Pair => key,
            };
            err(format!("{k}: {}", e.message))
        })?;
        let c = Arc::new(c);
        identities.insert(id, Arc::clone(&c));
        c
    };
    Ok(Transport::Tls {
        address,
        server_name: ServerName::DnsName(server_name),
        config,
    })
}
