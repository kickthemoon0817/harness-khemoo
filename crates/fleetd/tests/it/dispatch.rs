use std::time::{Duration, Instant};

use fleet_proto::{ResponseBody, Role, SessionState};
use serde_json::json;

use crate::support::{
    TempDir, adapter, alive, base_config, collect, connect, dispatch, error_code, eventually,
    request, start, wait,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prompt_arrives_on_stdin_and_never_in_argv() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "echo \"argv:$*\"\necho \"stdin:$(cat)\"\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &["--flag", "{workdir}"], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;

    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "PROMPT-TEXT\nsecond line"}),
    )
    .await
    .unwrap();
    let (out, state) = wait(&mut conn, &id).await;
    assert_eq!(state, SessionState::Exited { code: 0 });
    assert_eq!(
        out,
        format!(
            "argv:--flag {}\nstdin:PROMPT-TEXT\nsecond line\n",
            root.display()
        )
    );
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_static_env_table_is_set_after_the_copy() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script(
        "agent",
        "echo \"$TARGET_REPO|$HOME|$HARNESS_INSTANCE_DIR\"\n",
    );
    let env = "[adapters.a.env]\nTARGET_REPO = \"/srv/repo\"\nHOME = \"/srv/home\"\nHARNESS_INSTANCE_DIR = \"/srv/instance\"";
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, env);
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (out, _) = wait(&mut conn, &id).await;
    assert_eq!(
        out, "/srv/repo|/srv/home|/srv/instance\n",
        "operator values win over the copy"
    );
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_and_adapter_are_set_over_the_env_table() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "echo \"$FLEET_SESSION|$FLEET_ADAPTER\"\n");
    let env = "[adapters.a.env]\nFLEET_SESSION = \"forged\"\nFLEET_ADAPTER = \"forged\"";
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, env);
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (out, _) = wait(&mut conn, &id).await;
    assert_eq!(out, format!("{id}|a\n"));
    running.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tick_file_records_the_agent_until_it_ends() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    // The agent reads the file once fleetd has written it, then says what it should hold.
    let agent = dir.script(
        "agent",
        "for _ in $(seq 100); do [ -s \"$FLEET_SESSION_TICK\" ] && break; sleep 0.05; done\n\
         stat=$(cat /proc/$$/stat); set -- ${stat##*) }\n\
         echo \"$FLEET_SESSION_TICK\"\n\
         echo \"$(stat -c %a \"$FLEET_SESSION_TICK\") $(cat \"$FLEET_SESSION_TICK\")\"\n\
         echo \"600 $$ ${20} $(cat /proc/sys/kernel/random/boot_id)\"\n",
    );
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (out, _) = wait(&mut conn, &id).await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].ends_with(&format!("{id}/tick")), "{out}");
    assert_eq!(
        lines[1], lines[2],
        "the file holds the agent's pid and start time, 0600"
    );
    eventually("the tick file to go once the session ended", || {
        !std::path::Path::new(lines[0]).exists()
    })
    .await;
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_workdir_must_stay_under_a_root() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let inside = dir.mkdir("root/repo");
    dir.mkdir("rootkit");
    let outside = dir.mkdir("outside");
    std::os::unix::fs::symlink(&outside, root.join("link-out")).unwrap();
    std::os::unix::fs::symlink(&inside, root.join("link-in")).unwrap();
    std::fs::write(root.join("file"), "").unwrap();
    let agent = dir.script("agent", "pwd\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let try_dir = |w: String| json!({"adapter": "a", "workdir": w, "prompt": "p"});
    let r = |p: &str| format!("{}/{p}", root.display());

    for (workdir, code) in [
        (r("../outside"), "forbidden"),
        (r("repo/../../outside"), "forbidden"),
        (r("link-out"), "forbidden"),
        (format!("{}/rootkit", dir.path().display()), "forbidden"),
        ("/".to_owned(), "forbidden"),
        (r("missing"), "not_found"),
        (r("file"), "not_found"),
    ] {
        let err = dispatch(&mut conn, try_dir(workdir.clone()))
            .await
            .unwrap_err();
        assert_eq!(error_code(&err), code, "{workdir}");
    }
    // A relative workdir does not decode, which closes the connection.
    let (mut other, _) = connect(&running, Role::Dispatch).await;
    let err = dispatch(&mut other, try_dir("relative".into()))
        .await
        .unwrap_err();
    assert_eq!(error_code(&err), "bad_request");

    // Inside, even through a symlink, the agent runs in the canonical directory.
    let id = dispatch(&mut conn, try_dir(r("link-in"))).await.unwrap();
    let (out, _) = wait(&mut conn, &id).await;
    assert_eq!(out.trim_end(), inside.display().to_string());
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_come_from_the_allowlist() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "echo \"model=$1\"\n");
    let toml = base_config(&dir)
        + &adapter(
            "a",
            &agent,
            &["{model}"],
            &root,
            "models = [\"first\", \"second\"]",
        );
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let spec = |model: Option<&str>| {
        let mut s = json!({"adapter": "a", "workdir": root, "prompt": "p"});
        if let Some(m) = model {
            s["model"] = json!(m);
        }
        s
    };

    let err = dispatch(&mut conn, spec(Some("third"))).await.unwrap_err();
    assert_eq!(error_code(&err), "forbidden");
    let (mut other, _) = connect(&running, Role::Dispatch).await;
    let err = dispatch(&mut other, spec(Some("-rf"))).await.unwrap_err();
    assert_eq!(
        error_code(&err),
        "bad_request",
        "an invalid model name does not decode"
    );

    for (asked, ran) in [(None, "first"), (Some("second"), "second")] {
        let id = dispatch(&mut conn, spec(asked)).await.unwrap();
        let (out, _) = wait(&mut conn, &id).await;
        assert_eq!(out, format!("model={ran}\n"));
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_cannot_name_an_exe_args_or_env() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "true\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    for extra in [
        json!({"exe": "/bin/rm"}),
        json!({"args": ["-rf", "/"]}),
        json!({"env": {"LD_PRELOAD": "x"}}),
    ] {
        let (mut conn, _) = connect(&running, Role::Dispatch).await;
        let mut spec = json!({"adapter": "a", "workdir": root, "prompt": "p"});
        spec.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let err = dispatch(&mut conn, spec).await.unwrap_err();
        assert_eq!(error_code(&err), "bad_request", "{extra}");
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_and_discovery_only_adapters_are_refused() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let toml = base_config(&dir) + "[adapters.watch]\nmatch_exe = [\"fleetd-test-none\"]\n";
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    for (name, code) in [("watch", "forbidden"), ("nope", "not_found")] {
        let err = dispatch(
            &mut conn,
            json!({"adapter": name, "workdir": root, "prompt": "p"}),
        )
        .await
        .unwrap_err();
        assert_eq!(error_code(&err), code, "{name}");
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_concurrent_caps_running_sessions() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "cat >/dev/null\nexec sleep 60\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "max_concurrent = 2");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let spec = json!({"adapter": "a", "workdir": root, "prompt": "p"});

    let first = dispatch(&mut conn, spec.clone()).await.unwrap();
    let second = dispatch(&mut conn, spec.clone()).await.unwrap();
    let err = dispatch(&mut conn, spec.clone()).await.unwrap_err();
    assert_eq!(error_code(&err), "busy");

    let status = request(&mut conn, 2, json!({"op": "status"})).await;
    let ResponseBody::Status(report) = status.body else {
        panic!("{status:?}")
    };
    assert_eq!(
        (
            report.adapters[0].running,
            report.adapters[0].max_concurrent
        ),
        (2, 2)
    );

    let resp = request(&mut conn, 3, json!({"op": "cancel", "session": first})).await;
    assert!(
        matches!(resp.body, ResponseBody::Cancelled { .. }),
        "{resp:?}"
    );
    let (_, state) = wait(&mut conn, &first).await;
    assert_eq!(state, SessionState::Cancelled {});
    let third = dispatch(&mut conn, spec).await.unwrap();

    for id in [second, third] {
        request(&mut conn, 4, json!({"op": "cancel", "session": id})).await;
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_past_its_timeout_is_killed() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    // It ignores SIGTERM, so only the SIGKILL after the grace ends it.
    let agent = dir.script("agent", "trap '' TERM\necho started\nsleep 60\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "timeout_s = 1");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;

    let began = Instant::now();
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p", "timeout_s": 600}),
    )
    .await
    .unwrap();
    let (out, state) = wait(&mut conn, &id).await;
    assert_eq!(state, SessionState::TimedOut {});
    assert_eq!(out, "started\n");
    let took = began.elapsed();
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(10),
        "{took:?}"
    );
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_timeout_can_only_shorten() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "exec sleep 60\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "timeout_s = 600");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p", "timeout_s": 1}),
    )
    .await
    .unwrap();
    let (_, state) = tokio::time::timeout(Duration::from_secs(10), wait(&mut conn, &id))
        .await
        .unwrap();
    assert_eq!(state, SessionState::TimedOut {});
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_kills_the_whole_process_group() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    // Both the shell and its child ignore SIGTERM, so the group needs the SIGKILL.
    let agent = dir.script(
        "agent",
        "trap '' TERM\nsleep 60 &\necho $! > child.pid\necho ready\nwait\n",
    );
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let pid_file = root.join("child.pid");
    eventually("the child to start", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|s| s.ends_with('\n'))
    })
    .await;
    let child: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(child));

    let resp = request(&mut conn, 2, json!({"op": "cancel", "session": id})).await;
    assert!(
        matches!(resp.body, ResponseBody::Cancelled { .. }),
        "{resp:?}"
    );
    let (_, state) = wait(&mut conn, &id).await;
    assert_eq!(state, SessionState::Cancelled {});
    eventually("the child to die", || !alive(child)).await;

    // Cancelling an ended session succeeds and signals nothing.
    let resp = request(&mut conn, 3, json!({"op": "cancel", "session": id})).await;
    assert!(
        matches!(resp.body, ResponseBody::Cancelled { .. }),
        "{resp:?}"
    );
    let resp = request(
        &mut conn,
        4,
        json!({"op": "cancel", "session": "d-1-00000000"}),
    )
    .await;
    assert_eq!(error_code(&resp.body), "not_found");
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helpers_left_in_the_group_are_ended_with_the_agent() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "sleep 60 &\necho $! > child.pid\nexit 3\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (_, state) = wait(&mut conn, &id).await;
    assert_eq!(
        state,
        SessionState::Exited { code: 3 },
        "the agent's own exit is kept"
    );
    let child: i32 = std::fs::read_to_string(root.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    eventually("the helper to die", || !alive(child)).await;
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_is_capped_and_the_agent_never_blocks() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    // About 1 MiB, far past the 4096-byte cap and any pipe buffer.
    let agent = dir.script(
        "agent",
        "i=0\nwhile [ $i -lt 16384 ]; do echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; i=$((i+1)); done\necho done >&2\n",
    );
    let toml = base_config(&dir)
        .replace("kill_grace_s = 1", "kill_grace_s = 1\nlog_max_bytes = 4096")
        + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (out, state) = wait(&mut conn, &id).await;
    assert_eq!(state, SessionState::Exited { code: 0 });
    assert_eq!(out.len(), 4096 + "[fleetd: log truncated]\n".len());
    assert!(
        out.ends_with("\n[fleetd: log truncated]\n"),
        "{:?}",
        &out[4000..]
    );
    let log = dir.join(&format!("state/sessions/{id}/output.log"));
    assert_eq!(std::fs::metadata(log).unwrap().len(), out.len() as u64);
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_tail_and_follow() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script(
        "agent",
        "i=1\nwhile [ $i -le 50 ]; do echo $i; i=$((i+1)); done\nwhile [ ! -e go ]; do sleep 0.1; done\necho after\n",
    );
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let log = dir.join(&format!("state/sessions/{id}/output.log"));
    eventually("50 lines", || {
        std::fs::read_to_string(&log).is_ok_and(|s| s.ends_with("50\n"))
    })
    .await;

    conn.send(&json!({"id": 5, "body": {"op": "logs", "session": id, "lines": 3}}))
        .await
        .unwrap();
    let (out, state) = collect(&mut conn, 5).await;
    assert_eq!(out, "48\n49\n50\n");
    assert_eq!(state, SessionState::Running {});

    conn.send(&json!({"id": 6, "body": {"op": "logs", "session": id, "lines": 1, "follow": true}}))
        .await
        .unwrap();
    std::fs::write(root.join("go"), "").unwrap();
    let (out, state) = collect(&mut conn, 6).await;
    assert_eq!(out, "50\nafter\n");
    assert_eq!(state, SessionState::Exited { code: 0 });

    let (out, _) = wait(&mut conn, &id).await;
    assert!(out.starts_with("1\n2\n3\n") && out.ends_with("50\nafter\n"));
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_marks_running_sessions_failed() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "exec sleep 60\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let resp = request(&mut conn, 2, json!({"op": "sessions"})).await;
    let ResponseBody::Sessions { sessions, .. } = resp.body else {
        panic!("{resp:?}")
    };
    let pid = sessions[0].pid.unwrap();
    drop(conn);
    running.shutdown().await;
    assert!(
        alive(pid.try_into().unwrap()),
        "shutdown leaves sessions running"
    );

    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let resp = request(&mut conn, 2, json!({"op": "sessions"})).await;
    let ResponseBody::Sessions { sessions, .. } = resp.body else {
        panic!("{resp:?}")
    };
    assert_eq!(sessions[0].id.as_str(), id);
    assert_eq!(
        sessions[0].state,
        SessionState::Failed {
            message: "fleetd restarted while the session ran".into()
        }
    );
    assert_eq!(sessions[0].pid, None);
    let group = rustix::process::Pid::from_raw(pid.try_into().unwrap()).unwrap();
    rustix::process::kill_process_group(group, rustix::process::Signal::KILL).unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ended_sessions_past_retention_are_deleted() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "echo hi\n");
    let toml = base_config(&dir).replace(
        "kill_grace_s = 1",
        "kill_grace_s = 1\nsession_retention_s = 0",
    ) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let session_dir = dir.join(&format!("state/sessions/{id}"));
    eventually("the session to be deleted", || !session_dir.exists()).await;
    let resp = request(&mut conn, 2, json!({"op": "logs", "session": id})).await;
    assert_eq!(error_code(&resp.body), "not_found");
    running.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlimits_apply_on_linux() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    // Reading stdin first: fleetd sets the limits before it writes the prompt.
    let agent = dir.script("agent", "cat >/dev/null\nulimit -n\nulimit -c\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "rlimit_nofile = 100");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (out, _) = wait(&mut conn, &id).await;
    assert_eq!(out, "100\n0\n");
    running.shutdown().await;
}
