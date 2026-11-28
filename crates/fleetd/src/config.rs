//! fleetd.toml: loading, defaults, and validation of the operator's policy.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fleet_proto::{AdapterName, DeviceName, ErrorCode, ModelName, Role, WireError};
use ipnet::IpNet;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config file: pass --config, or set FLEETD_CONFIG, XDG_CONFIG_HOME or HOME")]
    NoPath,
    #[error("{}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    Parse(#[from] toml::de::Error),
    #[error("{key}: {message}")]
    Invalid { key: String, message: String },
}

fn invalid(key: impl Into<String>, message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        message: message.into(),
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub device: DeviceName,
    pub state_dir: PathBuf,
    pub session_retention: Duration,
    pub log_max_bytes: u64,
    pub kill_grace: Duration,
    pub max_connections: usize,
    pub handshake_timeout: Duration,
    pub idle_timeout: Duration,
    pub unix: UnixConfig,
    pub stdio_max_role: Role,
    pub metrics: MetricsConfig,
    pub all_users: bool,
    pub tls: Option<TlsConfig>,
    /// In name order.
    pub adapters: Vec<Adapter>,
}

#[derive(Debug, Clone)]
pub struct UnixConfig {
    pub socket: PathBuf,
    pub max_role: Role,
}

#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub sample_interval: Duration,
    pub disks: Vec<PathBuf>,
    pub gpu_probe: Option<Vec<String>>,
    pub gpu_probe_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub listen: SocketAddr,
    pub allow_cidrs: Vec<IpNet>,
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    /// The tls listener's own budget, apart from the unix one, so peers that have not
    /// authenticated can never lock out local and ssh clients.
    pub max_connections: usize,
    /// At most `max_connections`.
    pub max_connections_per_ip: usize,
    /// At most `max_connections`; bounds slow handshakes holding the budget.
    pub max_handshakes: usize,
    /// Normalized with [`normalize_serial`].
    pub revoked_serials: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct Adapter {
    pub name: AdapterName,
    /// The allowlist; the first is the default.
    pub models: Vec<ModelName>,
    pub match_exe: Vec<String>,
    /// `None` for a discovery-only adapter.
    pub launch: Option<Launch>,
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<Arg>,
    /// Canonical.
    pub workdir_roots: Vec<PathBuf>,
    pub env_allow: Vec<String>,
    /// Set after the `env_allow` copy, so the operator's values win.
    pub env: BTreeMap<String, String>,
    pub max_concurrent: u32,
    pub timeout: Duration,
    pub rlimit_nofile: u64,
}

/// One argv element, parsed at load so that rendering cannot meet an unknown placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg(Vec<Piece>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Model,
    Workdir,
}

impl Arg {
    fn parse(key: &str, s: &str) -> Result<Self, ConfigError> {
        let mut pieces = Vec::new();
        let mut rest = s;
        while !rest.is_empty() {
            let (piece, len) = if rest.starts_with("{model}") {
                (Piece::Model, "{model}".len())
            } else if rest.starts_with("{workdir}") {
                (Piece::Workdir, "{workdir}".len())
            } else if rest.starts_with(['{', '}']) {
                return Err(invalid(
                    key,
                    "only {model} and {workdir} may appear in braces",
                ));
            } else {
                let end = rest.find(['{', '}']).unwrap_or(rest.len());
                (Piece::Text(rest[..end].to_owned()), end)
            };
            pieces.push(piece);
            rest = &rest[len..];
        }
        Ok(Self(pieces))
    }

    fn uses_model(&self) -> bool {
        self.0.contains(&Piece::Model)
    }
}

impl Launch {
    /// The child's argv after `exe`.
    pub fn render(&self, model: Option<&ModelName>, workdir: &Path) -> Vec<OsString> {
        self.args
            .iter()
            .map(|arg| {
                let mut out = OsString::new();
                for piece in &arg.0 {
                    match piece {
                        Piece::Text(t) => out.push(t),
                        Piece::Model => out.push(model.map_or("", ModelName::as_str)),
                        Piece::Workdir => out.push(workdir),
                    }
                }
                out
            })
            .collect()
    }

    /// The canonical form of a requested workdir, if it is an existing directory at or under
    /// one of the roots. Canonicalizing first means neither `..` nor a symlink can leave a
    /// root, and comparing components means `/srv/work` does not admit `/srv/workshop`.
    pub fn resolve_workdir(&self, requested: &Path) -> Result<PathBuf, WireError> {
        let canonical = std::fs::canonicalize(requested)
            .map_err(|_| WireError::new(ErrorCode::NotFound, "the workdir does not exist"))?;
        if !canonical.is_dir() {
            return Err(WireError::new(
                ErrorCode::NotFound,
                "the workdir is not a directory",
            ));
        }
        if !self.workdir_roots.iter().any(|r| canonical.starts_with(r)) {
            return Err(WireError::new(
                ErrorCode::Forbidden,
                "the workdir is outside the adapter's workdir_roots",
            ));
        }
        Ok(canonical)
    }
}

/// A revoked serial as fleetd compares it: lowercase hex without `:` separators or leading
/// zeros, so `00:AB:cd` and `abcd` are the same serial. `None` if it is not hex.
pub fn normalize_serial(s: &str) -> Option<String> {
    let hex: String = s
        .chars()
        .filter(|&c| c != ':')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let trimmed = hex.trim_start_matches('0');
    Some(if trimmed.is_empty() { "0" } else { trimmed }.to_owned())
}

/// The config file to read: `--config`, else `$FLEETD_CONFIG`, else the XDG location.
pub fn resolve_path(cli: Option<PathBuf>) -> Result<PathBuf, ConfigError> {
    cli.or_else(|| env_path("FLEETD_CONFIG"))
        .or_else(|| env_path("XDG_CONFIG_HOME").map(|d| d.join("fleetd/fleetd.toml")))
        .or_else(|| env_path("HOME").map(|h| h.join(".config/fleetd/fleetd.toml")))
        .ok_or(ConfigError::NoPath)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    device: String,
    state_dir: Option<PathBuf>,
    session_retention_s: Option<u64>,
    log_max_bytes: Option<u64>,
    kill_grace_s: Option<u64>,
    max_connections: Option<u64>,
    handshake_timeout_s: Option<u64>,
    idle_timeout_s: Option<u64>,
    #[serde(default)]
    unix: RawUnix,
    #[serde(default)]
    stdio: RawStdio,
    #[serde(default)]
    metrics: RawMetrics,
    #[serde(default)]
    discovery: RawDiscovery,
    tls: Option<RawTls>,
    #[serde(default)]
    adapters: BTreeMap<String, RawAdapter>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUnix {
    socket: Option<PathBuf>,
    max_role: Option<Role>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStdio {
    max_role: Option<Role>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMetrics {
    sample_interval_s: Option<u64>,
    disks: Option<Vec<PathBuf>>,
    gpu_probe: Option<Vec<String>>,
    gpu_probe_timeout_s: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiscovery {
    #[serde(default)]
    all_users: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    listen: String,
    #[serde(default)]
    allow_cidrs: Vec<String>,
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
    max_connections: Option<u64>,
    max_connections_per_ip: Option<u64>,
    max_handshakes: Option<u64>,
    #[serde(default)]
    revoked_serials: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdapter {
    preset: Option<Preset>,
    exe: Option<PathBuf>,
    args: Option<Vec<String>>,
    models: Option<Vec<String>>,
    workdir_roots: Option<Vec<PathBuf>>,
    #[serde(default)]
    env_allow: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    max_concurrent: Option<u64>,
    timeout_s: Option<u64>,
    match_exe: Option<Vec<String>>,
    rlimit_nofile: Option<u64>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Preset {
    Claude,
    Codex,
    Agy,
}

struct PresetTable {
    args: &'static [&'static str],
    models: &'static [&'static str],
    match_exe: &'static [&'static str],
    max_concurrent: u64,
    timeout_s: u64,
}

impl Preset {
    fn table(self) -> PresetTable {
        match self {
            Self::Claude => PresetTable {
                args: &["-p", "--model", "{model}"],
                models: &["opus", "sonnet", "haiku"],
                match_exe: &["claude"],
                max_concurrent: 1,
                timeout_s: 3600,
            },
            Self::Codex => PresetTable {
                args: &["exec", "--model", "{model}", "-"],
                models: &[],
                match_exe: &["codex"],
                max_concurrent: 1,
                timeout_s: 3600,
            },
            // agy reads a prompt from stdin only as stream-json, one NDJSON message a line. fleetd
            // passes the dispatched prompt to stdin as it came, so the dispatcher sends that line.
            Self::Agy => PresetTable {
                args: &[
                    "--model",
                    "{model}",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                ],
                models: &[],
                match_exe: &["agy"],
                max_concurrent: 1,
                timeout_s: 3600,
            },
        }
    }
}

fn strings(s: &[&str]) -> Vec<String> {
    s.iter().map(|&s| s.to_owned()).collect()
}

/// `v` or `default`, which must lie in `min..=max`.
fn ranged(key: &str, v: Option<u64>, default: u64, min: u64, max: u64) -> Result<u64, ConfigError> {
    let v = v.unwrap_or(default);
    if (min..=max).contains(&v) {
        Ok(v)
    } else if max == u64::MAX {
        Err(invalid(key, format!("must be at least {min}")))
    } else {
        Err(invalid(key, format!("must be between {min} and {max}")))
    }
}

fn secs(
    key: &str,
    v: Option<u64>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<Duration, ConfigError> {
    ranged(key, v, default, min, max).map(Duration::from_secs)
}

fn absolute(key: &str, p: PathBuf) -> Result<PathBuf, ConfigError> {
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(invalid(key, "must be an absolute path"))
    }
}

/// The most connections fleetd tracks; far above any real fleet, it keeps the limits within
/// what the semaphore and the per-address table can represent.
const MAX_CONNECTIONS_LIMIT: u64 = 4096;

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses and validates a config, including the files it names: each `exe` and
    /// `workdir_roots` entry. The TLS material is checked by [`crate::tls::load`].
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: Raw = toml::from_str(text)?;
        let device: DeviceName = raw
            .device
            .parse()
            .map_err(|e: fleet_proto::Invalid| invalid("device", e.to_string()))?;
        let state_dir = match raw.state_dir {
            Some(p) => absolute("state_dir", p)?,
            None => env_path("XDG_STATE_HOME")
                .map(|d| d.join("fleetd"))
                .or_else(|| env_path("HOME").map(|h| h.join(".local/state/fleetd")))
                .ok_or_else(|| {
                    invalid(
                        "state_dir",
                        "not set, and neither XDG_STATE_HOME nor HOME is",
                    )
                })?,
        };
        let unix = UnixConfig {
            socket: match raw.unix.socket {
                Some(p) => absolute("unix.socket", p)?,
                None => state_dir.join("fleetd.sock"),
            },
            max_role: raw.unix.max_role.unwrap_or(Role::Dispatch),
        };
        let tls = raw.tls.map(parse_tls).transpose()?;
        let adapters = raw
            .adapters
            .into_iter()
            .map(|(name, a)| parse_adapter(&name, a))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            device,
            state_dir,
            session_retention: secs(
                "session_retention_s",
                raw.session_retention_s,
                604_800,
                0,
                u64::MAX,
            )?,
            log_max_bytes: ranged("log_max_bytes", raw.log_max_bytes, 64 << 20, 4096, u64::MAX)?,
            kill_grace: secs("kill_grace_s", raw.kill_grace_s, 10, 1, 300)?,
            max_connections: usize_of(ranged(
                "max_connections",
                raw.max_connections,
                32,
                1,
                MAX_CONNECTIONS_LIMIT,
            )?),
            handshake_timeout: secs("handshake_timeout_s", raw.handshake_timeout_s, 10, 1, 300)?,
            idle_timeout: secs("idle_timeout_s", raw.idle_timeout_s, 300, 1, u64::MAX)?,
            unix,
            stdio_max_role: raw.stdio.max_role.unwrap_or(Role::Dispatch),
            metrics: parse_metrics(raw.metrics)?,
            all_users: raw.discovery.all_users,
            tls,
            adapters,
        })
    }

    pub fn adapter(&self, name: &AdapterName) -> Option<&Adapter> {
        self.adapters.iter().find(|a| &a.name == name)
    }
}

fn usize_of(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn parse_metrics(raw: RawMetrics) -> Result<MetricsConfig, ConfigError> {
    let disks = raw
        .disks
        .unwrap_or_else(|| vec![PathBuf::from("/")])
        .into_iter()
        .map(|p| absolute("metrics.disks", p))
        .collect::<Result<_, _>>()?;
    if let Some(argv) = &raw.gpu_probe {
        match argv.first() {
            None => return Err(invalid("metrics.gpu_probe", "must not be empty")),
            Some(exe) if !Path::new(exe).is_absolute() => {
                return Err(invalid(
                    "metrics.gpu_probe",
                    "the first element must be an absolute path",
                ));
            }
            Some(_) => {}
        }
    }
    Ok(MetricsConfig {
        sample_interval: secs(
            "metrics.sample_interval_s",
            raw.sample_interval_s,
            5,
            1,
            3600,
        )?,
        disks,
        gpu_probe: raw.gpu_probe,
        gpu_probe_timeout: secs(
            "metrics.gpu_probe_timeout_s",
            raw.gpu_probe_timeout_s,
            5,
            1,
            60,
        )?,
    })
}

fn parse_tls(raw: RawTls) -> Result<TlsConfig, ConfigError> {
    let listen: SocketAddr = raw.listen.parse().map_err(|_| {
        invalid(
            "tls.listen",
            "must be IP:port, such as 192.0.2.10:7443; host names are not accepted",
        )
    })?;
    let allow_cidrs: Vec<IpNet> = raw
        .allow_cidrs
        .iter()
        .map(|c| {
            c.parse()
                .map_err(|_| invalid("tls.allow_cidrs", format!("{c:?} is not a CIDR")))
        })
        .collect::<Result<_, _>>()?;
    if listen.ip().to_canonical().is_unspecified() && allow_cidrs.is_empty() {
        return Err(invalid(
            "tls.listen",
            "a wildcard address listens on every interface; set allow_cidrs, or bind one address",
        ));
    }
    let revoked_serials = raw
        .revoked_serials
        .iter()
        .map(|s| {
            normalize_serial(s)
                .ok_or_else(|| invalid("tls.revoked_serials", format!("{s:?} is not hex")))
        })
        .collect::<Result<_, _>>()?;
    let max_connections = usize_of(ranged(
        "tls.max_connections",
        raw.max_connections,
        64,
        1,
        MAX_CONNECTIONS_LIMIT,
    )?);
    // Unset, each limit defaults to no more than the budget it divides.
    let within = |key: &str, v: Option<u64>, default: u64| {
        let default = default.min(max_connections as u64);
        let v = usize_of(ranged(key, v, default, 1, MAX_CONNECTIONS_LIMIT)?);
        if v > max_connections {
            return Err(invalid(key, "must not exceed tls.max_connections"));
        }
        Ok(v)
    };
    let max_connections_per_ip =
        within("tls.max_connections_per_ip", raw.max_connections_per_ip, 8)?;
    let max_handshakes = within("tls.max_handshakes", raw.max_handshakes, 32)?;
    Ok(TlsConfig {
        listen,
        allow_cidrs,
        ca: absolute("tls.ca", raw.ca)?,
        cert: absolute("tls.cert", raw.cert)?,
        key: absolute("tls.key", raw.key)?,
        max_connections,
        max_connections_per_ip,
        max_handshakes,
        revoked_serials,
    })
}

fn parse_adapter(name: &str, raw: RawAdapter) -> Result<Adapter, ConfigError> {
    let key = |field: &str| format!("adapters.{name}.{field}");
    let adapter_name: AdapterName = name
        .parse()
        .map_err(|e: fleet_proto::Invalid| invalid(format!("adapters.{name}"), e.to_string()))?;
    let preset = raw.preset.map(Preset::table);
    let args = raw
        .args
        .or_else(|| preset.as_ref().map(|p| strings(p.args)))
        .unwrap_or_default()
        .iter()
        .map(|a| Arg::parse(&key("args"), a))
        .collect::<Result<Vec<_>, _>>()?;
    let models = raw
        .models
        .or_else(|| preset.as_ref().map(|p| strings(p.models)))
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            m.parse::<ModelName>()
                .map_err(|e| invalid(key("models"), e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let match_exe = raw
        .match_exe
        .or_else(|| preset.as_ref().map(|p| strings(p.match_exe)))
        .or_else(|| {
            raw.exe
                .as_ref()
                .and_then(|e| e.file_name())
                .map(|n| vec![n.to_string_lossy().into_owned()])
        })
        .unwrap_or_default();
    if match_exe.iter().any(String::is_empty) {
        return Err(invalid(key("match_exe"), "entries must not be empty"));
    }

    let Some(exe) = raw.exe else {
        if match_exe.is_empty() {
            return Err(invalid(
                key("match_exe"),
                "a discovery-only adapter (no exe) needs match_exe",
            ));
        }
        return Ok(Adapter {
            name: adapter_name,
            models,
            match_exe,
            launch: None,
        });
    };

    let exe = absolute(&key("exe"), exe)?;
    let meta = std::fs::metadata(&exe).map_err(|e| invalid(key("exe"), e.to_string()))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(invalid(key("exe"), "must be an executable regular file"));
    }
    if args.iter().any(Arg::uses_model) && models.is_empty() {
        return Err(invalid(
            key("models"),
            "must not be empty when args use {model}",
        ));
    }
    let roots = raw
        .workdir_roots
        .ok_or_else(|| invalid(key("workdir_roots"), "required with exe"))?;
    if roots.is_empty() {
        return Err(invalid(key("workdir_roots"), "must not be empty"));
    }
    let workdir_roots = roots
        .into_iter()
        .map(|r| {
            let r = absolute(&key("workdir_roots"), r)?;
            let canonical = std::fs::canonicalize(&r)
                .map_err(|e| invalid(key("workdir_roots"), format!("{}: {e}", r.display())))?;
            if canonical.is_dir() {
                Ok(canonical)
            } else {
                Err(invalid(
                    key("workdir_roots"),
                    format!("{} is not a directory", r.display()),
                ))
            }
        })
        .collect::<Result<_, _>>()?;
    if let Some(bad) = raw.env_allow.iter().find(|n| !is_env_name(n)) {
        return Err(invalid(
            key("env_allow"),
            format!("{bad:?} is not a variable name"),
        ));
    }
    if let Some(bad) = raw.env.keys().find(|n| !is_static_env_name(n)) {
        return Err(invalid(
            key("env"),
            format!("{bad:?} is not an upper-case variable name"),
        ));
    }
    if let Some((name, _)) = raw.env.iter().find(|(_, v)| v.contains('\0')) {
        return Err(invalid(
            key("env"),
            format!("the value of {name} contains NUL"),
        ));
    }
    let max_concurrent = ranged(
        &key("max_concurrent"),
        raw.max_concurrent,
        preset.as_ref().map_or(1, |p| p.max_concurrent),
        1,
        64,
    )?;
    Ok(Adapter {
        name: adapter_name,
        models,
        match_exe,
        launch: Some(Launch {
            exe,
            args,
            workdir_roots,
            env_allow: raw.env_allow,
            env: raw.env,
            max_concurrent: u32::try_from(max_concurrent).unwrap_or(64),
            timeout: secs(
                &key("timeout_s"),
                raw.timeout_s,
                preset.as_ref().map_or(3600, |p| p.timeout_s),
                1,
                604_800,
            )?,
            rlimit_nofile: ranged(&key("rlimit_nofile"), raw.rlimit_nofile, 4096, 64, u64::MAX)?,
        }),
    })
}

fn is_env_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `[A-Z_][A-Z0-9_]*`: the operator's static variables.
fn is_static_env_name(s: &str) -> bool {
    is_env_name(s) && !s.bytes().any(|b| b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_placeholders_and_reject_other_braces() {
        let arg = Arg::parse("k", "--cd={workdir}/x").unwrap();
        assert_eq!(
            arg.0,
            vec![
                Piece::Text("--cd=".into()),
                Piece::Workdir,
                Piece::Text("/x".into())
            ]
        );
        for bad in ["{home}", "{model", "x}", "{}", "{{model}}"] {
            assert!(Arg::parse("k", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn serials_normalize() {
        assert_eq!(normalize_serial("00:AB:cd").as_deref(), Some("abcd"));
        assert_eq!(normalize_serial("000").as_deref(), Some("0"));
        assert_eq!(normalize_serial("xyz"), None);
        assert_eq!(normalize_serial(""), None);
    }

    #[test]
    fn env_names() {
        assert!(is_env_name("ANTHROPIC_API_KEY"));
        assert!(is_env_name("_x1"));
        assert!(!is_env_name("1X"));
        assert!(!is_env_name("A-B"));
        assert!(!is_env_name(""));
        assert!(is_static_env_name("HARNESS_HOME"));
        assert!(!is_static_env_name("harness_home"));
    }
}
