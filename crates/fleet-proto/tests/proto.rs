use std::collections::BTreeMap;

use fleet_proto::output::{
    DeviceError, DeviceReport, DeviceSession, OUTPUT_VERSION, SessionsOutput, StatusOutput,
    TransportKind,
};
use fleet_proto::{
    AdapterInfo, AdapterName, Connection, DeviceInfo, DeviceName, Disk, DispatchSpec, ErrorCode,
    Gpu, Hello, Invalid, Labels, LogLines, MAX_FRAME_LEN, Memory, Metrics, ModelName, Op, Origin,
    PROTOCOL_VERSION, Prompt, ProtoError, Request, RequestBody, RequestId, Response, ResponseBody,
    Role, Session, SessionId, SessionState, StatusReport, TimeoutSecs, WireError, Workdir,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

fn id(n: u64) -> RequestId {
    RequestId::new(n).unwrap()
}

fn round_trip<T>(value: &T) -> T
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let text = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&text).unwrap();
    assert_eq!(&back, value, "round trip changed {text}");
    back
}

fn dispatch_spec() -> DispatchSpec {
    let labels: BTreeMap<String, String> = [("issue".to_owned(), "42".to_owned())].into();
    DispatchSpec {
        adapter: "claude".parse().unwrap(),
        workdir: "/srv/work/repo-42".parse().unwrap(),
        model: Some("sonnet".parse().unwrap()),
        prompt: "Fix the bug.\n\tThen test it.".parse().unwrap(),
        timeout_s: Some(TimeoutSecs::try_from(3600).unwrap()),
        labels: Labels::try_from(labels).unwrap(),
    }
}

fn session() -> Session {
    Session {
        id: "d-1700000000-1a2b3c4d".parse().unwrap(),
        adapter: "claude".parse().unwrap(),
        origin: Origin::Dispatched,
        state: SessionState::Exited { code: 0 },
        pid: None,
        started_at: 1_700_000_000,
        ended_at: Some(1_700_000_600),
        cwd: Some("/srv/work/repo-42".into()),
        exe: Some("/usr/local/bin/claude".into()),
        model: Some("sonnet".parse().unwrap()),
        summary: "claude -p (dispatched)".into(),
        labels: Labels::default(),
    }
}

fn status_report() -> StatusReport {
    StatusReport {
        device: DeviceInfo {
            name: "dev-a".parse().unwrap(),
            hostname: "dev-a.example.com".into(),
            os: "Ubuntu 24.04".into(),
            kernel: "6.8.0".into(),
            arch: "x86_64".into(),
            cpus: 16,
            fleetd_version: "0.1.0".into(),
        },
        metrics: Metrics {
            sampled_at: 1_700_000_000,
            uptime_s: 86_400,
            load: [0.5, 0.25, 0.125],
            cpu_percent: 12.5,
            memory: Memory {
                total_bytes: 64 << 30,
                used_bytes: 8 << 30,
            },
            swap: Memory {
                total_bytes: 0,
                used_bytes: 0,
            },
            disks: vec![Disk {
                mount: "/".into(),
                total_bytes: 1 << 40,
                available_bytes: 1 << 39,
            }],
            gpus: vec![Gpu {
                index: 0,
                name: "Example GPU".into(),
                util_percent: Some(50.0),
                memory_total_bytes: Some(24 << 30),
                memory_used_bytes: None,
                temperature_c: Some(61.0),
            }],
        },
        adapters: vec![AdapterInfo {
            name: "claude".parse().unwrap(),
            models: vec!["sonnet".parse().unwrap(), "opus".parse().unwrap()],
            max_concurrent: 2,
            running: 1,
        }],
    }
}

// ---- wire format ----

#[test]
fn requests_have_the_documented_wire_form() {
    let cases = [
        (
            RequestBody::Status {},
            json!({"id": 1, "body": {"op": "status"}}),
        ),
        (
            RequestBody::Sessions {},
            json!({"id": 1, "body": {"op": "sessions"}}),
        ),
        (
            RequestBody::Logs {
                session: "d-1".parse().unwrap(),
                lines: LogLines::default(),
                follow: true,
            },
            json!({"id": 1, "body": {"op": "logs", "session": "d-1", "lines": 200, "follow": true}}),
        ),
        (
            RequestBody::Cancel {
                session: "d-1".parse().unwrap(),
            },
            json!({"id": 1, "body": {"op": "cancel", "session": "d-1"}}),
        ),
        (
            RequestBody::Wait {
                session: "d-1".parse().unwrap(),
            },
            json!({"id": 1, "body": {"op": "wait", "session": "d-1"}}),
        ),
        (
            RequestBody::Dispatch(dispatch_spec()),
            json!({"id": 1, "body": {
                "op": "dispatch",
                "adapter": "claude",
                "workdir": "/srv/work/repo-42",
                "model": "sonnet",
                "prompt": "Fix the bug.\n\tThen test it.",
                "timeout_s": 3600,
                "labels": {"issue": "42"},
            }}),
        ),
    ];
    for (body, expected) in cases {
        let req = Request { id: id(1), body };
        assert_eq!(serde_json::to_value(&req).unwrap(), expected);
        round_trip(&req);
    }
}

#[test]
fn optional_request_fields_default() {
    let req: Request =
        serde_json::from_value(json!({"id": 9, "body": {"op": "logs", "session": "p-77"}}))
            .unwrap();
    assert_eq!(
        req.body,
        RequestBody::Logs {
            session: "p-77".parse().unwrap(),
            lines: LogLines::default(),
            follow: false,
        }
    );
    let req: Request = serde_json::from_value(json!({"id": 9, "body": {
        "op": "dispatch", "adapter": "codex", "workdir": "/w", "prompt": "hi"}}))
    .unwrap();
    let RequestBody::Dispatch(spec) = req.body else {
        panic!("not a dispatch")
    };
    assert_eq!(spec.model, None);
    assert_eq!(spec.timeout_s, None);
    assert!(spec.labels.is_empty());
}

#[test]
fn responses_round_trip() {
    let bodies = [
        ResponseBody::Status(Box::new(status_report())),
        ResponseBody::Sessions {
            sessions: vec![session()],
            truncated: false,
        },
        ResponseBody::Dispatched {
            session: "d-1".parse().unwrap(),
        },
        ResponseBody::Cancelled {
            session: "d-1".parse().unwrap(),
        },
        ResponseBody::Event {
            data: "line one\nline two\n".into(),
        },
        ResponseBody::End {
            state: SessionState::Running {},
        },
        ResponseBody::End {
            state: SessionState::Signalled { signal: 9 },
        },
        ResponseBody::End {
            state: SessionState::Cancelled {},
        },
        ResponseBody::End {
            state: SessionState::TimedOut {},
        },
        ResponseBody::End {
            state: SessionState::Failed {
                message: "exe not found".into(),
            },
        },
        ResponseBody::Error(WireError::new(
            ErrorCode::Busy,
            "claude is at max_concurrent",
        )),
    ];
    for body in bodies {
        round_trip(&Response { id: 3, body });
    }
}

#[test]
fn responses_have_the_documented_wire_form() {
    let end = Response {
        id: 4,
        body: ResponseBody::End {
            state: SessionState::Exited { code: 2 },
        },
    };
    assert_eq!(
        serde_json::to_value(&end).unwrap(),
        json!({"id": 4, "body": {"kind": "end", "state": {"kind": "exited", "code": 2}}})
    );
    let err = Response::error(0, ErrorCode::TooLarge, "frame too large");
    assert_eq!(
        serde_json::to_value(&err).unwrap(),
        json!({"id": 0, "body": {"kind": "error", "code": "too_large", "message": "frame too large"}})
    );
    let status = serde_json::to_value(Response {
        id: 5,
        body: ResponseBody::Status(Box::new(status_report())),
    })
    .unwrap();
    assert_eq!(status["body"]["kind"], "status");
    assert_eq!(status["body"]["device"]["name"], "dev-a");
    assert_eq!(
        status["body"]["metrics"]["gpus"][0]["memory_used_bytes"],
        Value::Null
    );
}

#[test]
fn error_codes_are_stable_strings() {
    let codes = [
        (ErrorCode::Unauthorized, "unauthorized"),
        (ErrorCode::Forbidden, "forbidden"),
        (ErrorCode::BadRequest, "bad_request"),
        (ErrorCode::NotFound, "not_found"),
        (ErrorCode::Busy, "busy"),
        (ErrorCode::TooLarge, "too_large"),
        (ErrorCode::VersionMismatch, "version_mismatch"),
        (ErrorCode::Unavailable, "unavailable"),
        (ErrorCode::Internal, "internal"),
        (ErrorCode::Unreachable, "unreachable"),
        (ErrorCode::Timeout, "timeout"),
        (ErrorCode::Protocol, "protocol"),
    ];
    for (code, text) in codes {
        assert_eq!(serde_json::to_value(code).unwrap(), json!(text));
        assert_eq!(code.as_str(), text);
        assert_eq!(
            serde_json::from_value::<ErrorCode>(json!(text)).unwrap(),
            code
        );
    }
}

#[test]
fn hub_output_round_trips() {
    let status = StatusOutput {
        version: OUTPUT_VERSION,
        generated_at: 1_700_000_000,
        devices: vec![
            DeviceReport {
                name: "dev-a".parse().unwrap(),
                transport: TransportKind::Tls,
                ok: true,
                error: None,
                status: Some(status_report()),
                sessions: vec![session()],
                sessions_truncated: false,
            },
            DeviceReport {
                name: "dev-b".parse().unwrap(),
                transport: TransportKind::Ssh,
                ok: false,
                error: Some(WireError::new(ErrorCode::Unreachable, "connection refused")),
                status: None,
                sessions: vec![],
                sessions_truncated: false,
            },
        ],
    };
    let v = serde_json::to_value(&status).unwrap();
    assert_eq!(v["devices"][1]["transport"], "ssh");
    assert_eq!(v["devices"][1]["error"]["code"], "unreachable");
    round_trip(&status);
    round_trip(&SessionsOutput {
        version: OUTPUT_VERSION,
        generated_at: 1_700_000_000,
        sessions: vec![DeviceSession {
            device: "dev-a".parse().unwrap(),
            session: session(),
        }],
        errors: vec![DeviceError {
            device: "dev-b".parse().unwrap(),
            error: WireError::new(ErrorCode::Timeout, "no answer in 30s"),
        }],
        truncated: vec![],
    });
}

// ---- strict decoding ----

#[test]
fn unknown_fields_are_rejected() {
    let cases = [
        json!({"id": 1, "body": {"op": "status"}, "extra": 1}),
        json!({"id": 1, "body": {"op": "status", "extra": 1}}),
        json!({"id": 1, "body": {"op": "cancel", "session": "d-1", "signal": 9}}),
        json!({"id": 1, "body": {"op": "dispatch", "adapter": "claude", "workdir": "/w",
            "prompt": "hi", "exe": "/bin/sh"}}),
        json!({"id": 1, "body": {"op": "dispatch", "adapter": "claude", "workdir": "/w",
            "prompt": "hi", "env": {"LD_PRELOAD": "/tmp/x.so"}}}),
        json!({"id": 1, "body": {"op": "reboot"}}),
    ];
    for case in cases {
        assert!(
            serde_json::from_value::<Request>(case.clone()).is_err(),
            "accepted {case}"
        );
    }

    let mut status = serde_json::to_value(Response {
        id: 1,
        body: ResponseBody::Status(Box::new(status_report())),
    })
    .unwrap();
    status["body"]["metrics"]["extra"] = json!(1);
    assert!(serde_json::from_value::<Response>(status).is_err());

    let mut sessions = serde_json::to_value(Response {
        id: 1,
        body: ResponseBody::Sessions {
            sessions: vec![session()],
            truncated: false,
        },
    })
    .unwrap();
    sessions["body"]["sessions"][0]["argv"] = json!(["claude", "--secret"]);
    assert!(serde_json::from_value::<Response>(sessions).is_err());

    let end = json!({"id": 1, "body": {"kind": "end", "state": {"kind": "running", "x": 1}}});
    assert!(serde_json::from_value::<Response>(end).is_err());

    let error = json!({"id": 1, "body": {"kind": "error", "code": "busy", "message": "m", "x": 1}});
    assert!(serde_json::from_value::<Response>(error).is_err());
}

#[test]
fn hello_tolerates_unknown_fields_so_versions_can_be_compared() {
    let hello: Hello = serde_json::from_value(
        json!({"protocol_version": 7, "role": "monitor", "features": ["future"]}),
    )
    .unwrap();
    assert_eq!(hello.protocol_version, 7);
    assert_eq!(hello.role, Role::Monitor);
}

#[test]
fn request_id_zero_is_rejected() {
    assert!(serde_json::from_value::<Request>(json!({"id": 0, "body": {"op": "status"}})).is_err());
}

// ---- validation ----

#[test]
fn names_are_validated() {
    for ok in ["claude", "codex", "my_agent-2", "a"] {
        assert!(ok.parse::<AdapterName>().is_ok(), "{ok}");
    }
    for bad in [
        "",
        "-x",
        "Claude",
        "a b",
        "a/b",
        "x".repeat(33).as_str(),
        "a\0b",
    ] {
        assert!(bad.parse::<AdapterName>().is_err(), "{bad:?}");
    }
    for ok in [
        "sonnet",
        "claude-opus-4-1",
        "gpt-5.1-codex",
        "org/model:8b",
        "m@2+x",
    ] {
        assert!(ok.parse::<ModelName>().is_ok(), "{ok}");
    }
    for bad in ["", "--model", ".hidden", "a b", "x".repeat(129).as_str()] {
        assert!(bad.parse::<ModelName>().is_err(), "{bad:?}");
    }
    for ok in ["d-1700000000-1a2b3c4d", "p-4242"] {
        assert!(ok.parse::<SessionId>().is_ok(), "{ok}");
    }
    for bad in ["", "../etc", "d.1", "x".repeat(65).as_str()] {
        assert!(bad.parse::<SessionId>().is_err(), "{bad:?}");
    }
}

#[test]
fn workdir_must_be_absolute_bounded_and_nul_free() {
    assert!("/srv/work".parse::<Workdir>().is_ok());
    assert_eq!(
        "srv/work".parse::<Workdir>(),
        Err(Invalid::NotAbsolute { field: "workdir" })
    );
    assert_eq!(
        "".parse::<Workdir>(),
        Err(Invalid::Empty { field: "workdir" })
    );
    assert_eq!(
        "/srv/\0work".parse::<Workdir>(),
        Err(Invalid::BadChar { field: "workdir" })
    );
    let long = format!("/{}", "a".repeat(Workdir::MAX_LEN));
    assert!(matches!(
        long.parse::<Workdir>(),
        Err(Invalid::TooLong { .. })
    ));
}

#[test]
fn prompt_is_bounded_and_free_of_control_characters() {
    assert!("line\n\tindented\r\n".parse::<Prompt>().is_ok());
    assert!("é and ☃".parse::<Prompt>().is_ok());
    assert!(Prompt::try_from("a".repeat(Prompt::MAX_LEN)).is_ok());
    assert!(matches!(
        Prompt::try_from("a".repeat(Prompt::MAX_LEN + 1)),
        Err(Invalid::TooLong { .. })
    ));
    assert!(matches!("".parse::<Prompt>(), Err(Invalid::Empty { .. })));
    for bad in [
        "nul\0byte",
        "escape\x1b[31m",
        "bell\x07",
        "del\x7f",
        "csi\u{9b}31m",
    ] {
        assert!(
            matches!(bad.parse::<Prompt>(), Err(Invalid::BadChar { .. })),
            "{bad:?}"
        );
    }
}

#[test]
fn a_maximal_dispatch_fits_in_one_frame() {
    // Quotes and backslashes are the worst case JSON escaping can do to a valid prompt.
    let mut spec = dispatch_spec();
    spec.prompt = Prompt::try_from("\"\\".repeat(Prompt::MAX_LEN / 2)).unwrap();
    let req = Request {
        id: id(u64::MAX),
        body: RequestBody::Dispatch(spec),
    };
    assert!(serde_json::to_vec(&req).unwrap().len() <= MAX_FRAME_LEN);
}

#[test]
fn device_names_are_validated() {
    for ok in ["dev-a", "dev-a.lab", "0box"] {
        assert!(ok.parse::<DeviceName>().is_ok(), "{ok}");
    }
    for bad in [
        "",
        "Dev-a",
        "-dev",
        ".dev",
        "dev_a",
        "x".repeat(64).as_str(),
    ] {
        assert!(bad.parse::<DeviceName>().is_err(), "{bad:?}");
    }
}

#[test]
fn labels_are_validated() {
    let ok: BTreeMap<String, String> = [("issue".into(), "42".into())].into();
    let labels = Labels::try_from(ok).unwrap();
    assert_eq!(labels.get("issue"), Some("42"));

    let too_many: BTreeMap<String, String> = (0..=Labels::MAX_ENTRIES)
        .map(|i| (format!("k{i}"), String::new()))
        .collect();
    assert!(matches!(
        Labels::try_from(too_many),
        Err(Invalid::TooMany { .. })
    ));
    for (k, v) in [
        ("Issue", "42"),
        ("", "42"),
        ("issue", "4\n2"),
        ("issue", &"x".repeat(Labels::MAX_VALUE_LEN + 1)),
    ] {
        let map: BTreeMap<String, String> = [(k.to_owned(), v.to_owned())].into();
        assert!(Labels::try_from(map).is_err(), "{k:?}={v:?}");
    }
}

#[test]
fn bounded_numbers_are_validated() {
    assert!(TimeoutSecs::try_from(0).is_err());
    assert_eq!(TimeoutSecs::try_from(1).unwrap().get(), 1);
    assert_eq!(TimeoutSecs::try_from(604_800).unwrap().get(), 604_800);
    assert!(TimeoutSecs::try_from(604_801).is_err());
    assert!(TimeoutSecs::try_from(u64::MAX).is_err());
    assert!("12x".parse::<TimeoutSecs>().is_err());
    assert_eq!(LogLines::default().get(), 200);
    assert!(LogLines::try_from(10_001).is_err());
}

#[test]
fn invalid_values_fail_decoding() {
    let base = json!({"op": "dispatch", "adapter": "claude", "workdir": "/w", "prompt": "hi"});
    let cases = [
        ("workdir", json!("relative/dir")),
        ("workdir", json!("/w\u{0}")),
        ("prompt", json!("")),
        ("prompt", json!("a\u{0}b")),
        ("prompt", json!("a".repeat(Prompt::MAX_LEN + 1))),
        ("adapter", json!("../bin/sh")),
        ("model", json!("-e")),
        ("timeout_s", json!(0)),
        ("timeout_s", json!(-5)),
        ("labels", json!({"BAD": "x"})),
    ];
    for (field, value) in cases {
        let mut body = base.clone();
        body[field] = value;
        let req = json!({"id": 1, "body": body});
        assert!(
            serde_json::from_value::<Request>(req).is_err(),
            "accepted {field}"
        );
    }
    let bad_lines = json!({"id": 1, "body": {"op": "logs", "session": "d-1", "lines": 0}});
    assert!(serde_json::from_value::<Request>(bad_lines).is_err());
}

// ---- roles ----

#[test]
fn role_permissions() {
    let table = [
        (Op::Status, true, true),
        (Op::Sessions, true, true),
        (Op::Logs, true, true),
        (Op::Wait, true, true),
        (Op::Dispatch, false, true),
        (Op::Cancel, false, true),
    ];
    assert_eq!(table.len(), Op::ALL.len());
    for (op, monitor, dispatch) in table {
        assert_eq!(Role::Monitor.permits(op), monitor, "monitor {op:?}");
        assert_eq!(Role::Dispatch.permits(op), dispatch, "dispatch {op:?}");
        assert_eq!(
            op.required_role(),
            if monitor {
                Role::Monitor
            } else {
                Role::Dispatch
            }
        );
    }
    assert_eq!(RequestBody::Dispatch(dispatch_spec()).op(), Op::Dispatch);
    assert_eq!("monitor".parse::<Role>(), Ok(Role::Monitor));
    assert!("admin".parse::<Role>().is_err());
    assert_eq!(
        serde_json::to_value(Role::Dispatch).unwrap(),
        json!("dispatch")
    );
}

// ---- framing and handshake ----

#[tokio::test]
async fn frames_carry_messages_both_ways() {
    let (a, b) = tokio::io::duplex(4096);
    let (mut client, mut server) = (Connection::new(a), Connection::new(b));
    let req = Request {
        id: id(1),
        body: RequestBody::Dispatch(dispatch_spec()),
    };
    client.send(&req).await.unwrap();
    assert_eq!(server.recv::<Request>().await.unwrap(), Some(req));
    let resp = Response {
        id: 1,
        body: ResponseBody::Dispatched {
            session: "d-1".parse().unwrap(),
        },
    };
    server.send(&resp).await.unwrap();
    assert_eq!(client.recv::<Response>().await.unwrap(), Some(resp));
    drop(server);
    assert!(client.recv::<Response>().await.unwrap().is_none());
}

#[tokio::test]
async fn frames_are_big_endian_u32_length_prefixed_json() {
    let (a, mut b) = tokio::io::duplex(4096);
    let mut conn = Connection::new(a);
    conn.send(&json!({"x": 1})).await.unwrap();
    let mut buf = [0u8; 11];
    tokio::io::AsyncReadExt::read_exact(&mut b, &mut buf)
        .await
        .unwrap();
    assert_eq!(&buf, b"\0\0\0\x07{\"x\":1}");
}

#[tokio::test]
async fn oversize_frames_are_refused_both_ways() {
    let (a, mut b) = tokio::io::duplex(64);
    let mut conn = Connection::new(a);
    let huge = "a".repeat(MAX_FRAME_LEN);
    assert!(matches!(conn.send(&huge).await, Err(ProtoError::TooLarge)));

    let len = u32::try_from(MAX_FRAME_LEN + 1).unwrap();
    b.write_all(&len.to_be_bytes()).await.unwrap();
    assert!(matches!(
        conn.recv::<Value>().await,
        Err(ProtoError::TooLarge)
    ));
}

#[tokio::test]
async fn bad_frames_are_reported() {
    let (a, mut b) = tokio::io::duplex(64);
    let mut conn = Connection::new(a);
    b.write_all(b"\0\0\0\x03{x}").await.unwrap();
    assert!(matches!(
        conn.recv::<Request>().await,
        Err(ProtoError::Decode(_))
    ));
    // A frame cut short by the peer closing is an i/o error, not a clean close.
    b.write_all(b"\0\0\0\x09{").await.unwrap();
    drop(b);
    assert!(matches!(
        conn.recv::<Request>().await,
        Err(ProtoError::Io(_))
    ));
}

#[tokio::test]
async fn handshake_grants_the_lesser_role() {
    for (asked, max, granted) in [
        (Role::Dispatch, Role::Dispatch, Role::Dispatch),
        (Role::Monitor, Role::Dispatch, Role::Monitor),
        (Role::Dispatch, Role::Monitor, Role::Monitor),
    ] {
        let (a, b) = tokio::io::duplex(4096);
        let (mut client, mut server) = (Connection::new(a), Connection::new(b));
        let (c, s) = tokio::join!(client.client_hello(asked), server.server_hello(max));
        assert_eq!(c.unwrap(), granted);
        assert_eq!(s.unwrap(), granted);
    }
}

#[tokio::test]
async fn version_mismatch_fails_both_sides() {
    let (a, b) = tokio::io::duplex(4096);
    let (mut client, mut server) = (Connection::new(a), Connection::new(b));
    client
        .send(&json!({"protocol_version": PROTOCOL_VERSION + 1, "role": "monitor"}))
        .await
        .unwrap();
    let theirs = PROTOCOL_VERSION + 1;
    assert!(matches!(
        server.server_hello(Role::Dispatch).await,
        Err(ProtoError::VersionMismatch { ours: PROTOCOL_VERSION, theirs: t }) if t == theirs
    ));
    // The server still told the client its version.
    let hello: Hello = client.recv().await.unwrap().unwrap();
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);

    let (a, b) = tokio::io::duplex(4096);
    let (mut client, mut server) = (Connection::new(a), Connection::new(b));
    let fake_server = async {
        let _: Hello = server.recv().await.unwrap().unwrap();
        server
            .send(&json!({"protocol_version": 99, "role": "monitor"}))
            .await
            .unwrap();
    };
    let (c, ()) = tokio::join!(client.client_hello(Role::Monitor), fake_server);
    assert!(matches!(
        c,
        Err(ProtoError::VersionMismatch { theirs: 99, .. })
    ));
}

#[tokio::test]
async fn a_later_hello_with_unknown_values_reports_the_version_mismatch() {
    let (a, b) = tokio::io::duplex(4096);
    let (mut client, mut server) = (Connection::new(a), Connection::new(b));
    client
        .send(&json!({"protocol_version": PROTOCOL_VERSION + 1, "role": "admin"}))
        .await
        .unwrap();
    assert!(matches!(
        server.server_hello(Role::Dispatch).await,
        Err(ProtoError::VersionMismatch { .. })
    ));
    let hello: Hello = client.recv().await.unwrap().unwrap();
    assert_eq!(hello.role, Role::Monitor);

    // At the same version, an unknown role is simply undecodable.
    let (a, b) = tokio::io::duplex(4096);
    let (mut client, mut server) = (Connection::new(a), Connection::new(b));
    client
        .send(&json!({"protocol_version": PROTOCOL_VERSION, "role": "admin"}))
        .await
        .unwrap();
    assert!(matches!(
        server.server_hello(Role::Dispatch).await,
        Err(ProtoError::Decode(_))
    ));
}

#[tokio::test]
async fn into_parts_keeps_bytes_read_past_the_hello() {
    let (a, b) = tokio::io::duplex(4096);
    let (mut client, mut server) = (Connection::new(a), Connection::new(b));
    let pipelined = async {
        client
            .send(&json!({"protocol_version": PROTOCOL_VERSION, "role": "monitor"}))
            .await
            .unwrap();
        client.send(&json!({"next": "frame"})).await.unwrap();
        client
    };
    let (client, s) = tokio::join!(pipelined, server.server_hello(Role::Dispatch));
    assert_eq!(s.unwrap(), Role::Monitor);
    let (_io, buffered) = server.into_parts();
    let expected = b"\0\0\0\x10{\"next\":\"frame\"}";
    assert!(expected.starts_with(&buffered));
    drop(client);
}

#[tokio::test]
async fn handshake_with_a_closed_peer_fails() {
    let (a, b) = tokio::io::duplex(64);
    drop(b);
    let mut server = Connection::new(a);
    assert!(matches!(
        server.server_hello(Role::Monitor).await,
        Err(ProtoError::Closed)
    ));
}
