use serde_json::{Value, json};
use std::{process::Command, thread};
use tiny_http::{Header, Response, Server};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_vardr")
}

#[test]
fn cli_import_edit_run_and_structured_errors() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("data");
    let fixture = temp.path().join("job.json");
    std::fs::write(&fixture,serde_json::to_vec(&json!({"id":"cli-test","name":"CLI test","purpose":"Verify a complete CLI path","cwd":temp.path(),"target":{"type":"command","argv":["/bin/echo","hello-cli"]},"schedule":{"type":"manual"}})).unwrap()).unwrap();
    let invoke = |args: &[&str]| {
        Command::new(bin())
            .arg("--home")
            .arg(&home)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(invoke(&["put", fixture.to_str().unwrap()]).status.success());
    let paused = invoke(&["pause", "cli-test"]);
    assert!(paused.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&paused.stdout).unwrap()["enabled"],
        false
    );
    assert!(invoke(&["run", "cli-test"]).status.success());
    let logs = invoke(&["logs", "cli-test"]);
    assert!(
        String::from_utf8(logs.stdout)
            .unwrap()
            .contains("hello-cli")
    );
    let missing = invoke(&["show", "does-not-exist"]);
    assert!(!missing.status.success());
    let error = serde_json::from_slice::<Value>(&missing.stderr).unwrap()["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("no job named does-not-exist") && error.contains("vardr list"),
        "{error}"
    );
    let error_of = |output: std::process::Output| {
        assert!(!output.status.success());
        serde_json::from_slice::<Value>(&output.stderr).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let absent = temp.path().join("absent.json");
    let error = error_of(invoke(&["put", absent.to_str().unwrap()]));
    assert!(error.contains("absent.json"), "{error}");
    std::fs::write(home.join("config.json"), b"{").unwrap();
    let error = error_of(invoke(&["status"]));
    assert!(error.contains("config.json"), "{error}");
    std::fs::write(home.join("config.json"), br#"{"max_concurrency":0}"#).unwrap();
    let error = error_of(invoke(&["status"]));
    assert!(
        error.contains("config.json") && error.contains("max_concurrency"),
        "{error}"
    );
}

fn t3_dispatch(lose_receipt: bool, gopass: bool) {
    let server = Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", server.server_addr());
    let thread = thread::spawn(move || {
        for stage in 0..3 {
            let mut request = server.recv().unwrap();
            let auth = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .unwrap();
            assert_eq!(auth.value.as_str(), "Bearer vardr-test-only-credential");
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            let body: Value = serde_json::from_str(&body).unwrap();
            if stage > 0 {
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|h| h.field.equiv("mcp-session-id")
                            && h.value.as_str() == "test-session")
                );
            }
            if stage == 0 {
                assert_eq!(body["method"], "initialize");
                request.respond(Response::from_string(json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}).to_string()).with_header(Header::from_bytes("Content-Type","application/json").unwrap()).with_header(Header::from_bytes("mcp-session-id","test-session").unwrap())).unwrap();
            } else if stage == 1 {
                assert_eq!(body["method"], "notifications/initialized");
                request.respond(Response::empty(202)).unwrap();
            } else {
                assert_eq!(body["method"], "tools/call");
                assert_eq!(body["params"]["name"], "t3_thread_launch");
                assert_eq!(body["params"]["arguments"]["projectId"], "test-project");
                assert_eq!(
                    body["params"]["arguments"]["runtimeMode"],
                    "approval-required"
                );
                if lose_receipt {
                    request.respond(Response::empty(502)).unwrap();
                    continue;
                }
                let envelope = json!({"jsonrpc":"2.0","id":body["id"],"result":{"content":[],"structuredContent":{"threadId":"test-thread","runId":"test-run","status":"running"}}});
                request
                    .respond(
                        Response::from_string(format!(
                            "event: message\r\ndata: {envelope}\r\n\r\n"
                        ))
                        .with_header(
                            Header::from_bytes("Content-Type", "text/event-stream").unwrap(),
                        ),
                    )
                    .unwrap();
            }
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("data");
    let fixture = temp.path().join("t3.json");
    let mut target = json!({"type":"t3","endpoint":endpoint,"project_id":"test-project"});
    if gopass {
        target["authorization_gopass"] = json!("test-key");
        let shim = temp.path().join("gopass");
        std::fs::write(
            &shim,
            "#!/bin/sh\nprintf '%s\\n' 'Bearer vardr-test-only-credential'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        target["authorization_env"] = json!("VARDR_TEST_T3_AUTH");
    }
    std::fs::write(&fixture,serde_json::to_vec(&json!({"id":"t3-test","name":"T3 test","purpose":"Verify real MCP HTTP wire handling against a local fixture","cwd":temp.path(),"target":target,"prompt":"Read-only test","schedule":{"type":"manual"}})).unwrap()).unwrap();
    assert!(
        Command::new(bin())
            .arg("--home")
            .arg(&home)
            .arg("put")
            .arg(&fixture)
            .output()
            .unwrap()
            .status
            .success()
    );
    let result = Command::new(bin())
        .arg("--home")
        .arg(&home)
        .args(["run", "t3-test"])
        .env("VARDR_TEST_T3_AUTH", "Bearer vardr-test-only-credential")
        .env(
            "PATH",
            format!(
                "{}:{}",
                temp.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success() != lose_receipt,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        result["status"],
        if lose_receipt {
            "dispatch_unknown"
        } else {
            "dispatched"
        }
    );
    if lose_receipt {
        assert!(result["investigation"].is_null());
    } else {
        assert_eq!(result["remote"]["threadId"], "test-thread");
    }
    thread.join().unwrap();
}

#[test]
fn t3_mcp_negotiates_session_and_records_dispatch_not_success() {
    t3_dispatch(false, false);
}
#[test]
fn t3_lost_launch_response_is_unknown_and_never_retried() {
    t3_dispatch(true, false);
}
#[test]
fn t3_can_resolve_credentials_from_a_bounded_secret_manager() {
    t3_dispatch(false, true);
}

#[test]
fn private_hosts_round_trip_through_cli_and_unknown_hosts_fail() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("data");
    let run = |args: &[&str]| {
        Command::new(bin())
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()
            .unwrap()
    };
    let result = run(&["hosts", "add", "test-mac", "--ssh", "test-mac"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let hosts: Value = serde_json::from_slice(&run(&["hosts", "list"]).stdout).unwrap();
    assert_eq!(hosts[0]["ssh"], "test-mac");
    assert!(
        !run(&["hosts", "add", "test-mac", "--ssh", "other"])
            .status
            .success()
    );
    assert!(
        !run(&["hosts", "add", "bad", "--ssh=host;touch bad"])
            .status
            .success()
    );
    let missing = run(&["discover", "--host", "absent", "--json"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("unknown host"));
    assert!(run(&["hosts", "remove", "test-mac"]).status.success());
    let hosts: Value = serde_json::from_slice(&run(&["hosts", "list"]).stdout).unwrap();
    assert_eq!(hosts, json!([]));
    assert!(!run(&["hosts", "remove", "absent"]).status.success());
}

#[test]
fn bridge_preserves_command_environment_and_rejects_conflicting_remote_edits() {
    use std::io::Write;
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("data");
    let request = |operation: Value| {
        let mut child = Command::new(bin())
            .arg("--home")
            .arg(&home)
            .arg("bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                serde_json::to_string(&json!({"protocol":1,"operation":operation}))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let job = json!({"id":"environment-test","name":"Environment test","purpose":"Preserve the original command environment across remote edits","cwd":temp.path(),"target":{"type":"command","argv":["/bin/sh","-c","printf '%s' \"$VARDR_FIXTURE_VALUE\""],"env":{"VARDR_FIXTURE_VALUE":"value with spaces and 'quotes'"}},"schedule":{"type":"manual"}});
    let saved = request(json!({"type":"save","job":job}))["result"].clone();
    assert_eq!(saved["revision"], 1);
    let snapshot = request(json!({"type":"snapshot"}));
    assert_eq!(snapshot["result"]["jobs"][0]["job"], saved);
    let mut changed = saved.clone();
    changed["schedule"] =
        json!({"type":"daily","time":"09:15","timezone":"Europe/Copenhagen","weekdays":[]});
    let changed = request(json!({"type":"save","job":changed}))["result"].clone();
    assert_eq!(changed["target"], saved["target"]);
    assert_eq!(changed["revision"], 2);
    assert!(
        request(json!({"type":"save","job":saved}))["error"]
            .as_str()
            .unwrap()
            .contains("changed since")
    );
    let output = Command::new(bin())
        .arg("--home")
        .arg(&home)
        .args(["run", "environment-test"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        request(json!({"type":"logs","id":"environment-test"}))["result"]["text"],
        "value with spaces and 'quotes'"
    );
    assert!(
        request(json!({"type":"remove","id":"environment-test","revision":1}))["error"]
            .as_str()
            .unwrap()
            .contains("changed since")
    );
    assert_eq!(
        request(json!({"type":"remove","id":"environment-test","revision":2}))["result"],
        json!({"removed":"environment-test"})
    );
    assert_eq!(
        request(json!({"type":"snapshot"}))["result"]["jobs"],
        json!([])
    );
}

#[test]
fn cli_remove_deletes_a_job_with_its_state_and_history() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("data");
    let fixture = temp.path().join("job.json");
    std::fs::write(&fixture,serde_json::to_vec(&json!({"id":"remove-test","name":"Remove test","purpose":"Verify job removal","cwd":temp.path(),"target":{"type":"command","argv":["/bin/echo","removed"]},"schedule":{"type":"manual"}})).unwrap()).unwrap();
    let invoke = |args: &[&str]| {
        Command::new(bin())
            .arg("--home")
            .arg(&home)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    };
    let error = |output: std::process::Output| {
        assert!(!output.status.success());
        serde_json::from_slice::<Value>(&output.stderr).unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(invoke(&["put", fixture.to_str().unwrap()]).status.success());
    assert!(invoke(&["run", "remove-test"]).status.success());
    let store = vardr::store::Store::open(home.canonicalize().unwrap()).unwrap();
    store
        .save_state("remove-test", &vardr::model::RuntimeState::default())
        .unwrap();
    let job = store.root.join("jobs/remove-test.json");
    let state = store.root.join("state/remove-test.json");
    let runs = store.root.join("runs/remove-test");
    assert!(job.exists() && state.exists() && runs.exists());

    let running = store.lock("run-remove-test").unwrap();
    assert!(error(invoke(&["remove", "remove-test"])).contains("remove-test is running"));
    assert!(job.exists() && state.exists() && runs.exists());
    drop(running);

    let removed = invoke(&["remove", "remove-test"]);
    assert!(removed.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&removed.stdout).unwrap(),
        json!({"removed":"remove-test"})
    );
    assert!(!job.exists() && !state.exists() && !runs.exists());
    assert!(error(invoke(&["show", "remove-test"])).contains("no job named remove-test"));
    assert!(error(invoke(&["remove", "remove-test"])).contains("no job named remove-test"));
    assert_eq!(
        serde_json::from_slice::<Value>(&invoke(&["list"]).stdout).unwrap()["jobs"],
        json!([])
    );

    // A removal interrupted after deleting the definition is finished by a retry.
    // A dashboard removal, which names a revision, refuses and leaves it alone.
    store
        .save_state("remove-test", &vardr::model::RuntimeState::default())
        .unwrap();
    assert!(
        format!("{:#}", store.remove("remove-test", Some(1)).unwrap_err())
            .contains("no job named remove-test")
    );
    assert!(state.exists());
    assert!(invoke(&["remove", "remove-test"]).status.success());
    assert!(!state.exists());

    // A dispatch reserved before the removal starts nothing and records nothing.
    assert!(
        vardr::runner::run_scheduled(&store, "remove-test", 1)
            .unwrap()
            .is_none()
    );
    assert!(!runs.exists());

    // An unreadable revision record refuses recreation instead of restarting at 1.
    // Root bypasses permissions, so the check only holds for ordinary users.
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let removed = store.root.join("removed");
        std::fs::set_permissions(&removed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = invoke(&["put", fixture.to_str().unwrap()]);
        std::fs::set_permissions(&removed, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error(refused).contains("Permission denied"));
        assert!(!job.exists());
    }

    // A job recreated with the ID continues its revisions, so an edit or dispatch
    // prepared for the removed revision 1 cannot apply to the replacement.
    let recreated = invoke(&["put", fixture.to_str().unwrap()]);
    assert_eq!(
        serde_json::from_slice::<Value>(&recreated.stdout).unwrap()["revision"],
        2
    );
    let mut stale: Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    stale["revision"] = json!(1);
    std::fs::write(&fixture, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert!(error(invoke(&["put", fixture.to_str().unwrap()])).contains("job changed"));
    let skipped = vardr::runner::run_scheduled(&store, "remove-test", 1)
        .unwrap()
        .unwrap();
    assert_eq!(skipped.status, "skipped");
    assert_eq!(skipped.job_revision, 2);
}

#[test]
fn rescheduling_a_daily_job_keeps_or_sets_weekdays() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("data");
    let fixture = temp.path().join("job.json");
    std::fs::write(
        &fixture,
        serde_json::to_vec(&json!({
            "id": "weekdays",
            "name": "Weekdays",
            "purpose": "Keep weekdays across a time change",
            "cwd": temp.path(),
            "target": {"type": "command", "argv": ["/bin/true"]},
            "schedule": {
                "type": "daily",
                "time": "09:00",
                "timezone": "Europe/Copenhagen",
                "weekdays": [1, 2, 3, 4, 5]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let invoke = |args: &[&str]| {
        let output = Command::new(bin())
            .arg("--home")
            .arg(&home)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    invoke(&["put", fixture.to_str().unwrap()]);
    let moved = invoke(&[
        "schedule",
        "weekdays",
        "--daily",
        "10:00",
        "--timezone",
        "Europe/Copenhagen",
    ]);
    assert_eq!(moved["schedule"]["time"], "10:00");
    assert_eq!(moved["schedule"]["weekdays"], json!([1, 2, 3, 4, 5]));
    let weekend = invoke(&[
        "schedule",
        "weekdays",
        "--daily",
        "11:00",
        "--timezone",
        "Europe/Copenhagen",
        "--weekdays",
        "6, 7",
    ]);
    assert_eq!(weekend["schedule"]["weekdays"], json!([6, 7]));
    let every_day = invoke(&[
        "schedule",
        "weekdays",
        "--daily",
        "12:00",
        "--timezone",
        "Europe/Copenhagen",
        "--weekdays",
        "all",
    ]);
    assert_eq!(every_day["schedule"]["time"], "12:00");
    assert_eq!(every_day["schedule"]["weekdays"], json!([]));
    let rejected = Command::new(bin())
        .arg("--home")
        .arg(&home)
        .args([
            "schedule",
            "weekdays",
            "--daily",
            "13:00",
            "--timezone",
            "Europe/Copenhagen",
            "--weekdays",
            "9",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let error = String::from_utf8(rejected.stderr).unwrap();
    assert!(error.contains("weekdays"), "{error}");
    let kept = invoke(&["show", "weekdays"]);
    assert_eq!(kept["schedule"]["time"], "12:00");
    assert_eq!(kept["schedule"]["weekdays"], json!([]));
}
