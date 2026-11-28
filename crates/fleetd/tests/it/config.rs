use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use fleet_proto::Role;
use fleetd::config::{Config, ConfigError};

use crate::support::TempDir;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new();
        dir.mkdir("root");
        let exe = dir.join("agent");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plain = dir.join("plain");
        std::fs::write(&plain, "").unwrap();
        Self { dir }
    }

    /// `{dir}` in `toml` becomes the fixture's directory.
    fn parse(&self, toml: &str) -> Result<Config, ConfigError> {
        let text = format!("device = \"dev-a\"\nstate_dir = \"{{dir}}/state\"\n{toml}");
        Config::parse(&text.replace("{dir}", &self.dir.path().display().to_string()))
    }
}

#[test]
fn a_full_config_resolves_defaults_and_presets() {
    let f = Fixture::new();
    let cfg = f
        .parse(
            r#"
[unix]
max_role = "monitor"

[metrics]
disks = ["/"]
gpu_probe = ["/usr/bin/true", "--flag"]

[discovery]
all_users = true

[tls]
listen = "0.0.0.0:7443"
allow_cidrs = ["192.0.2.0/24", "2001:db8::/32"]
ca = "/etc/fleetd/ca.pem"
cert = "/etc/fleetd/dev-a.pem"
key = "/etc/fleetd/dev-a.key"
revoked_serials = ["00:AB:CD"]

[adapters.claude]
preset = "claude"
exe = "{dir}/agent"
workdir_roots = ["{dir}/root/../root"]
env_allow = ["ANTHROPIC_API_KEY"]

[adapters.aider]
match_exe = ["aider"]

[adapters.codex]
preset = "codex"
"#,
        )
        .unwrap();
    assert_eq!(cfg.device.as_str(), "dev-a");
    assert_eq!(cfg.log_max_bytes, 64 << 20);
    assert_eq!(cfg.kill_grace, Duration::from_secs(10));
    assert_eq!(cfg.session_retention, Duration::from_secs(604_800));
    assert_eq!(cfg.max_connections, 32);
    assert_eq!(cfg.unix.socket, f.dir.join("state/fleetd.sock"));
    assert_eq!(cfg.unix.max_role, Role::Monitor);
    assert_eq!(cfg.stdio_max_role, Role::Dispatch);
    assert!(cfg.all_users);
    let tls = cfg.tls.as_ref().unwrap();
    assert_eq!(tls.max_connections, 64);
    assert_eq!(tls.max_connections_per_ip, 8);
    assert_eq!(tls.max_handshakes, 32);
    assert!(tls.revoked_serials.contains("abcd"));

    let names: Vec<&str> = cfg.adapters.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["aider", "claude", "codex"]);
    let claude = &cfg.adapters[1];
    let models: Vec<&str> = claude
        .models
        .iter()
        .map(fleet_proto::ModelName::as_str)
        .collect();
    assert_eq!(models, ["opus", "sonnet", "haiku"]);
    assert_eq!(claude.match_exe, ["claude"]);
    let launch = claude.launch.as_ref().unwrap();
    assert_eq!(launch.max_concurrent, 1);
    assert_eq!(launch.timeout, Duration::from_secs(3600));
    assert_eq!(launch.rlimit_nofile, 4096);
    assert_eq!(
        launch.workdir_roots,
        [f.dir.join("root")],
        "roots are canonical"
    );
    let argv = launch.render(claude.models.first(), &f.dir.join("root"));
    assert_eq!(argv, ["-p", "--model", "opus"]);
    assert!(cfg.adapters[0].launch.is_none(), "no exe: discovery only");
    assert!(cfg.adapters[2].launch.is_none());
    assert_eq!(cfg.adapters[2].match_exe, ["codex"]);
}

#[test]
fn the_gemini_preset_reads_its_prompt_from_stdin() {
    let f = Fixture::new();
    let cfg = f
        .parse(
            "[adapters.gemini]\npreset = \"gemini\"\nexe = \"{dir}/agent\"\nworkdir_roots = [\"{dir}/root\"]\nmodels = [\"gemini-3-pro\"]\n",
        )
        .unwrap();
    let gemini = &cfg.adapters[0];
    assert_eq!(gemini.match_exe, ["gemini"]);
    let launch = gemini.launch.as_ref().unwrap();
    let argv = launch.render(gemini.models.first(), &f.dir.join("root"));
    assert_eq!(argv, ["--model", "gemini-3-pro"]);
}

#[test]
fn match_exe_defaults_to_the_exe_base_name() {
    let f = Fixture::new();
    let cfg = f
        .parse("[adapters.x]\nexe = \"{dir}/agent\"\nworkdir_roots = [\"{dir}/root\"]\n")
        .unwrap();
    assert_eq!(cfg.adapters[0].match_exe, ["agent"]);
}

#[test]
fn each_bad_value_is_refused_naming_its_key() {
    let f = Fixture::new();
    let adapter = |extra: &str| {
        format!(
            "[adapters.a]\nexe = \"{{dir}}/agent\"\nworkdir_roots = [\"{{dir}}/root\"]\n{extra}"
        )
    };
    let tls = |extra: &str| format!("[tls]\nca = \"/c\"\ncert = \"/c\"\nkey = \"/k\"\n{extra}");
    let cases: Vec<(String, &str)> = vec![
        ("bogus = 1".into(), "bogus"),
        ("log_max_bytes = 100".into(), "log_max_bytes"),
        ("kill_grace_s = 0".into(), "kill_grace_s"),
        ("kill_grace_s = 301".into(), "kill_grace_s"),
        ("max_connections = 0".into(), "max_connections"),
        ("max_connections = 100000".into(), "max_connections"),
        ("handshake_timeout_s = 0".into(), "handshake_timeout_s"),
        ("idle_timeout_s = 0".into(), "idle_timeout_s"),
        ("kill_grace_s = \"10\"".into(), "kill_grace_s"),
        ("[unix]\nmax_role = \"root\"".into(), "max_role"),
        ("[unix]\nsocket = \"rel/sock\"".into(), "unix.socket"),
        ("[stdio]\nmax_role = \"admin\"".into(), "max_role"),
        ("[metrics]\nsample_interval_s = 0".into(), "metrics.sample_interval_s"),
        ("[metrics]\ndisks = [\"data\"]".into(), "metrics.disks"),
        ("[metrics]\ngpu_probe = []".into(), "metrics.gpu_probe"),
        ("[metrics]\ngpu_probe = [\"nvidia-smi\"]".into(), "metrics.gpu_probe"),
        ("[metrics]\ngpu_probe_timeout_s = 61".into(), "metrics.gpu_probe_timeout_s"),
        ("[discovery]\nall_users = \"yes\"".into(), "all_users"),
        (tls("listen = \"dev-a:7443\""), "tls.listen"),
        (tls("listen = \"0.0.0.0:7443\""), "tls.listen"),
        (tls("listen = \"[::]:7443\""), "tls.listen"),
        (tls("listen = \"192.0.2.10\""), "tls.listen"),
        (tls("listen = \"192.0.2.10:7443\"\nallow_cidrs = [\"10.0.0.0/33\"]"), "tls.allow_cidrs"),
        (tls("listen = \"192.0.2.10:7443\"\nrevoked_serials = [\"zz\"]"), "tls.revoked_serials"),
        (tls("listen = \"192.0.2.10:7443\"\nmax_connections_per_ip = 0"), "tls.max_connections_per_ip"),
        (tls("listen = \"192.0.2.10:7443\"\nmax_connections = 0"), "tls.max_connections"),
        (tls("listen = \"192.0.2.10:7443\"\nmax_connections = 4\nmax_connections_per_ip = 5"), "tls.max_connections_per_ip"),
        (tls("listen = \"192.0.2.10:7443\"\nmax_connections = 4\nmax_handshakes = 5"), "tls.max_handshakes"),
        (tls("listen = \"192.0.2.10:7443\"\nmax_handshakes = 0"), "tls.max_handshakes"),
        ("[tls]\nlisten = \"192.0.2.10:7443\"\nca = \"ca.pem\"\ncert = \"/c\"\nkey = \"/k\"".into(), "tls.ca"),
        ("[tls]\nlisten = \"192.0.2.10:7443\"\nca = \"/c\"\ncert = \"/c\"\nkey = \"k\"".into(), "tls.key"),
        ("[adapters.Bad]\nmatch_exe = [\"x\"]".into(), "adapters.Bad"),
        ("[adapters.a]\npreset = \"vim\"\nmatch_exe = [\"x\"]".into(), "preset"),
        ("[adapters.a]\nmatch_exe = []".into(), "adapters.a.match_exe"),
        ("[adapters.a]\nmatch_exe = [\"\"]".into(), "adapters.a.match_exe"),
        ("[adapters.a]\nexe = \"agent\"\nworkdir_roots = [\"/\"]".into(), "adapters.a.exe"),
        ("[adapters.a]\nexe = \"{dir}/missing\"\nworkdir_roots = [\"/\"]".into(), "adapters.a.exe"),
        ("[adapters.a]\nexe = \"{dir}/plain\"\nworkdir_roots = [\"/\"]".into(), "adapters.a.exe"),
        ("[adapters.a]\nexe = \"{dir}/root\"\nworkdir_roots = [\"/\"]".into(), "adapters.a.exe"),
        ("[adapters.a]\nexe = \"{dir}/agent\"".into(), "adapters.a.workdir_roots"),
        ("[adapters.a]\nexe = \"{dir}/agent\"\nworkdir_roots = []".into(), "adapters.a.workdir_roots"),
        ("[adapters.a]\nexe = \"{dir}/agent\"\nworkdir_roots = [\"{dir}/nope\"]".into(), "adapters.a.workdir_roots"),
        ("[adapters.a]\nexe = \"{dir}/agent\"\nworkdir_roots = [\"{dir}/agent\"]".into(), "adapters.a.workdir_roots"),
        ("[adapters.a]\nexe = \"{dir}/agent\"\nworkdir_roots = [\"root\"]".into(), "adapters.a.workdir_roots"),
        ("[adapters.a]\npreset = \"codex\"\nexe = \"{dir}/agent\"\nworkdir_roots = [\"{dir}/root\"]".into(), "adapters.a.models"),
        (adapter("args = [\"{home}\"]"), "adapters.a.args"),
        (adapter("args = [\"--x={model\"]\nmodels = [\"m\"]"), "adapters.a.args"),
        (adapter("args = [\"{model}\"]"), "adapters.a.models"),
        (adapter("models = [\"-rf\"]"), "adapters.a.models"),
        (adapter("env_allow = [\"A-B\"]"), "adapters.a.env_allow"),
        (adapter("max_concurrent = 0"), "adapters.a.max_concurrent"),
        (adapter("max_concurrent = 65"), "adapters.a.max_concurrent"),
        (adapter("timeout_s = 0"), "adapters.a.timeout_s"),
        (adapter("timeout_s = 604801"), "adapters.a.timeout_s"),
        (adapter("rlimit_nofile = 10"), "adapters.a.rlimit_nofile"),
        (adapter("prompt = \"x\""), "prompt"),
        (adapter("[adapters.a.env]\nharness_home = \"/x\""), "adapters.a.env"),
        (adapter("[adapters.a.env]\n\"A-B\" = \"/x\""), "adapters.a.env"),
        (adapter("[adapters.a.env]\nHARNESS_HOME = \"a\\u0000b\""), "adapters.a.env"),
        (adapter("[adapters.a.env]\nHARNESS_HOME = 1"), "HARNESS_HOME"),
    ];
    for (toml, key) in &cases {
        let err = f.parse(toml).expect_err(toml).to_string();
        assert!(err.contains(key), "{toml:?}: {err:?} does not name {key}");
    }
}

#[test]
fn bad_top_level_values_are_refused() {
    for (toml, key) in [
        ("device = \"Dev_A\"\nstate_dir = \"/x\"", "device"),
        ("device = \"dev-a\"\nstate_dir = \"rel\"", "state_dir"),
        ("state_dir = \"/x\"", "device"),
    ] {
        let err = Config::parse(toml).expect_err(toml).to_string();
        assert!(err.contains(key), "{toml:?}: {err}");
    }
}

#[test]
fn tls_limits_default_within_a_small_budget() {
    let f = Fixture::new();
    let cfg = f
        .parse("[tls]\nlisten = \"192.0.2.10:7443\"\nmax_connections = 4\nca = \"/c\"\ncert = \"/c\"\nkey = \"/k\"\n")
        .unwrap();
    let tls = cfg.tls.unwrap();
    assert_eq!((tls.max_connections_per_ip, tls.max_handshakes), (4, 4));
}

#[test]
fn a_wildcard_listen_is_allowed_with_allow_cidrs() {
    let f = Fixture::new();
    f.parse("[tls]\nlisten = \"[::]:7443\"\nallow_cidrs = [\"100.64.0.0/10\"]\nca = \"/c\"\ncert = \"/c\"\nkey = \"/k\"\n")
        .unwrap();
}

#[test]
fn a_missing_file_is_a_read_error() {
    let err = Config::load(std::path::Path::new("/nonexistent/fleetd.toml")).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }), "{err}");
}
