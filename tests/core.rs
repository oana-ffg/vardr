use chrono::{DateTime, Utc};
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
use vardr::{
    model::*,
    runner, scheduler,
    store::{Store, read_json, write_json},
};

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}
fn store() -> (tempfile::TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().canonicalize().unwrap().join("vardr")).unwrap();
    let mut settings = Settings::default();
    settings.updates.automatic = false;
    write_json(&store.root.join("config.json"), &settings).unwrap();
    (temp, store)
}
fn job(cwd: PathBuf, argv: &[&str]) -> Job {
    Job {
        id: "test-job".into(),
        name: "Test job".into(),
        purpose: "Exercise the real runner without external side effects".into(),
        project: "Tests".into(),
        machine: "fixture".into(),
        cwd,
        target: Target::Command {
            env: Default::default(),
            argv: argv.iter().map(|s| (*s).into()).collect(),
        },
        prompt: String::new(),
        schedule: Schedule::Manual,
        enabled: true,
        catch_up: false,
        timeout_seconds: 10,
        investigation: None,
        revision: 0,
        updated_at: None,
    }
}
#[test]
fn schedules_coalesce_missed_runs_and_ignore_clock_rollback() {
    let s = Schedule::Interval { seconds: 60 };
    let before = at("2026-01-01T00:00:00Z");
    assert_eq!(
        s.due(before, at("2026-01-01T00:05:30Z")).unwrap(),
        Some(at("2026-01-01T00:05:00Z"))
    );
    assert_eq!(s.due(before, at("2025-12-31T23:59:00Z")).unwrap(), None);
}
#[test]
fn daily_respects_timezone_weekdays_and_dst() {
    let daily = Schedule::Daily {
        time: "02:30".into(),
        timezone: "Europe/Copenhagen".into(),
        weekdays: vec![],
    };
    // Spring-forward skips a nonexistent time; it does not invent a 03:30 run.
    assert_eq!(
        daily
            .due(at("2026-03-28T23:00:00Z"), at("2026-03-29T04:00:00Z"))
            .unwrap(),
        None
    );
    // Fall-back runs only at the earlier occurrence.
    let first = at("2026-10-25T00:30:00Z");
    assert_eq!(
        daily
            .due(at("2026-10-24T23:00:00Z"), at("2026-10-25T02:00:00Z"))
            .unwrap(),
        Some(first)
    );
    assert_eq!(daily.due(first, at("2026-10-25T02:00:00Z")).unwrap(), None);
    let weekdays = Schedule::Daily {
        time: "09:00".into(),
        timezone: "Europe/Copenhagen".into(),
        weekdays: vec![1, 2, 3, 4, 5],
    };
    assert_eq!(
        weekdays
            .due(at("2026-10-16T08:00:00Z"), at("2026-10-18T20:00:00Z"))
            .unwrap(),
        None
    );
}
#[test]
fn revisions_prevent_lost_updates() {
    let (_temp, store) = store();
    let saved = store
        .save(job(store.root.clone(), &["/usr/bin/true"]))
        .unwrap();
    let stale = saved.clone();
    let mut changed = saved;
    changed.enabled = false;
    assert_eq!(store.save(changed).unwrap().revision, 2);
    assert!(store.save(stale).is_err());
    assert!(!store.job("test-job").unwrap().enabled);
}

#[test]
fn concurrent_field_edits_serialize_and_no_op_edits_keep_the_baseline() {
    let (_temp, store) = store();
    let saved = store.save(job(store.root.clone(), &["/bin/true"])).unwrap();
    let pausers: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                store.update("test-job", |job| {
                    job.enabled = false;
                    Ok(())
                })
            })
        })
        .collect();
    for pauser in pausers {
        assert!(!pauser.join().unwrap().unwrap().enabled);
    }
    let paused = store.job("test-job").unwrap();
    assert_eq!(paused.revision, saved.revision + 1);
    let again = store
        .update("test-job", |job| {
            job.enabled = false;
            Ok(())
        })
        .unwrap();
    assert_eq!(again.revision, paused.revision);
    assert_eq!(again.updated_at, paused.updated_at);
}

#[test]
fn scheduled_dispatch_rechecks_pause_revision_and_manual_overlap() {
    let (_temp, store) = store();
    let mut j = store
        .save(job(store.root.clone(), &["/bin/echo", "should-not-run"]))
        .unwrap();
    let old_revision = j.revision;
    j.enabled = false;
    let j = store.save(j).unwrap();
    assert_eq!(
        runner::run_scheduled(&store, &j.id, old_revision)
            .unwrap()
            .unwrap()
            .status,
        "skipped"
    );
    let mut j = j;
    j.enabled = true;
    let j = store.save(j).unwrap();
    let _manual = store.lock("run-test-job").unwrap();
    assert_eq!(
        runner::run_scheduled(&store, &j.id, j.revision)
            .unwrap()
            .unwrap()
            .status,
        "skipped"
    );
}
#[test]
fn runner_records_failures_and_investigates_once_per_cooldown() {
    let (_temp, store) = store();
    let mut j = job(
        store.root.clone(),
        &["/bin/sh", "-c", "echo failure-evidence; exit 7"],
    );
    j.investigation = Some(Investigation {
        enabled: true,
        target: Target::Command {
            env: Default::default(),
            argv: vec!["/bin/echo".into(), "investigated".into()],
        },
        cooldown_seconds: 3600,
        timeout_seconds: 10,
    });
    store.save(j).unwrap();
    let first = runner::run(&store, "test-job").unwrap();
    assert_eq!(first.status, "failed");
    assert_eq!(first.investigation.unwrap().status, "succeeded");
    assert!(
        runner::logs(&store, "test-job", false)
            .unwrap()
            .contains("failure-evidence")
    );
    assert!(
        runner::logs(&store, "test-job", true)
            .unwrap()
            .contains("investigated")
    );
    let second = runner::run(&store, "test-job").unwrap();
    assert_eq!(second.status, "failed");
    assert_eq!(second.investigation.unwrap().status, "cooldown");
    // The cooldown run has no investigation log; the previous one stays reachable.
    assert!(
        runner::logs(&store, "test-job", true)
            .unwrap()
            .contains("investigated")
    );
}
#[test]
fn command_investigator_receives_failure_prompt_on_stdin() {
    let (_temp, store) = store();
    let mut j = job(
        store.root.clone(),
        &["/bin/sh", "-c", "echo failure-evidence; exit 7"],
    );
    j.investigation = Some(Investigation {
        enabled: true,
        target: Target::Command {
            env: Default::default(),
            argv: vec!["/bin/cat".into()],
        },
        cooldown_seconds: 0,
        timeout_seconds: 10,
    });
    store.save(j).unwrap();
    let run = runner::run(&store, "test-job").unwrap();
    assert_eq!(run.status, "failed");
    assert_eq!(run.investigation.unwrap().status, "succeeded");
    let prompt = runner::logs(&store, "test-job", true).unwrap();
    assert!(prompt.contains("Investigate this failed automation"));
    assert!(prompt.contains("failure-evidence"));
}
#[test]
fn command_job_prompt_stays_off_stdin() {
    let (_temp, store) = store();
    let mut j = job(store.root.clone(), &["/bin/cat"]);
    j.prompt = "should-not-reach-stdin".into();
    store.save(j).unwrap();
    let run = runner::run(&store, "test-job").unwrap();
    assert_eq!(run.status, "succeeded");
    assert!(
        !runner::logs(&store, "test-job", false)
            .unwrap()
            .contains("should-not-reach-stdin")
    );
}
#[test]
fn timeout_terminates_descendants_and_retains_original_result() {
    let (_temp, store) = store();
    let mut j = job(store.root.clone(), &["/bin/sh", "-c", "sleep 60 & wait"]);
    j.timeout_seconds = 1;
    store.save(j).unwrap();
    let start = Instant::now();
    let record = runner::run(&store, "test-job").unwrap();
    assert_eq!(record.status, "timed_out");
    assert!(start.elapsed() < Duration::from_secs(5));
}
#[test]
fn command_launch_failure_is_durable_and_overlap_is_rejected() {
    let (_temp, store) = store();
    store
        .save(job(store.root.clone(), &["/vardr-test-does-not-exist"]))
        .unwrap();
    let lock = store.lock("run-test-job").unwrap();
    assert!(runner::run(&store, "test-job").is_err());
    drop(lock);
    let run = runner::run(&store, "test-job").unwrap();
    assert_eq!(run.status, "failed");
    assert!(run.finished_at.is_some());
    assert!(run.error.unwrap().contains("could not start"));
}
#[test]
fn logs_are_bounded_and_old_finished_runs_are_pruned() {
    let (_temp, store) = store();
    let mut settings = store.settings().unwrap();
    settings.log_limit_bytes = 4096;
    settings.retained_runs = 1;
    write_json(&store.root.join("config.json"), &settings).unwrap();
    store
        .save(job(
            store.root.clone(),
            &["/bin/sh", "-c", "yes evidence | head -c 100000"],
        ))
        .unwrap();
    runner::run(&store, "test-job").unwrap();
    runner::run(&store, "test-job").unwrap();
    assert_eq!(store.runs("test-job").unwrap().len(), 1);
    let text = runner::logs(&store, "test-job", false).unwrap();
    assert!(text.len() < 4300);
    assert!(text.contains("output limit reached"));
}
#[test]
fn scheduler_dispatches_once_and_recovers_unfinished_records() {
    let (_temp, store) = store();
    let mut j = job(store.root.clone(), &["/bin/echo", "scheduled"]);
    j.schedule = Schedule::Interval { seconds: 10 };
    let mut j = store.save(j).unwrap();
    // Move registration into the past to simulate an already-installed job.
    j.updated_at = Some(Utc::now() - chrono::Duration::seconds(20));
    write_json(&store.root.join("jobs/test-job.json"), &j).unwrap();
    assert!(!scheduler::serve(&store, true).unwrap());
    assert!(!scheduler::serve(&store, true).unwrap());
    assert_eq!(store.runs("test-job").unwrap().len(), 1);
    let mut record = store.runs("test-job").unwrap().remove(0);
    record.status = "running".into();
    record.finished_at = None;
    store.record(&record).unwrap();
    scheduler::serve(&store, true).unwrap();
    assert_eq!(store.runs("test-job").unwrap()[0].status, "interrupted");
}
#[test]
fn registration_does_not_run_a_past_daily_slot() {
    let (_temp, store) = store();
    let mut j = job(store.root.clone(), &["/usr/bin/true"]);
    j.schedule = Schedule::Daily {
        time: "00:00".into(),
        timezone: "UTC".into(),
        weekdays: vec![],
    };
    j.catch_up = true;
    store.save(j).unwrap();
    scheduler::serve(&store, true).unwrap();
    assert!(store.runs("test-job").unwrap().is_empty());
}
#[test]
fn invalid_inputs_and_symlink_redirection_are_rejected() {
    let (_temp, store) = store();
    assert!(validate_id("../escape").is_err());
    assert!(serde_json::from_str::<Schedule>(r#"{"type":"webhook"}"#).is_err());
    assert!(Schedule::Interval { seconds: 0 }.validate().is_err());
    let mut j = job(store.root.clone(), &["/usr/bin/true"]);
    j.purpose = String::new();
    assert!(store.save(j).is_err());
    #[cfg(unix)]
    {
        let other = store.root.join("other.json");
        write_json(&other, &Settings::default()).unwrap();
        std::os::unix::fs::symlink(&other, store.root.join("redirect.json")).unwrap();
        assert!(read_json::<Settings>(&store.root.join("redirect.json")).is_err());
    }
    assert!(
        fs::read_dir(store.root.join("jobs"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn second_scheduler_names_the_owner() {
    let (_temp, store) = store();
    let _running = store.lock("service").unwrap();
    let error = format!("{:#}", scheduler::serve(&store, true).unwrap_err());
    assert!(
        error.contains("another `vardr serve` already owns"),
        "{error}"
    );
}
#[test]
fn malformed_job_and_missing_directory_do_not_block_other_jobs() {
    let (_temp, store) = store();
    let mut good = job(store.root.clone(), &["/bin/echo", "still-runs"]);
    good.schedule = Schedule::Interval { seconds: 10 };
    let mut good = store.save(good).unwrap();
    good.updated_at = Some(Utc::now() - chrono::Duration::seconds(20));
    write_json(&store.root.join("jobs/test-job.json"), &good).unwrap();
    fs::write(store.root.join("jobs/broken.json"), b"invalid json").unwrap();
    assert!(scheduler::serve(&store, true).is_err());
    assert_eq!(store.runs("test-job").unwrap()[0].status, "succeeded");
    let heartbeat: ServiceState = read_json(&store.root.join("service.json")).unwrap();
    assert!(heartbeat.errors.values().any(|e| e.contains("broken.json")));
    let mut missing = job(store.root.join("missing-directory"), &["/bin/echo", "no"]);
    missing.id = "missing-directory".into();
    store.save(missing).unwrap();
    let missing = runner::run(&store, "missing-directory").unwrap();
    assert_eq!(missing.status, "failed");
    let error = missing.error.unwrap();
    assert!(
        error.contains("working directory") && error.contains("does not exist"),
        "{error}"
    );
    let file_cwd = store.root.join("not-a-directory");
    fs::write(&file_cwd, b"file").unwrap();
    let mut file_job = job(file_cwd, &["/bin/echo", "no"]);
    file_job.id = "not-a-directory".into();
    store.save(file_job).unwrap();
    let file_run = runner::run(&store, "not-a-directory").unwrap();
    assert_eq!(file_run.status, "failed");
    let error = file_run.error.unwrap();
    assert!(error.contains("is not a directory"), "{error}");
    assert_eq!(store.jobs().unwrap().jobs.len(), 3);
}

#[test]
fn detached_descendant_cannot_hold_pipes_or_stdin_open() {
    let (_temp, store) = store();
    let pid_path = store.root.join("escaped.pid");
    let code = format!(
        "import os,time,pathlib\np=pathlib.Path({:?})\nif os.fork()==0:\n os.setsid();p.write_text(str(os.getpid()));time.sleep(30);os._exit(0)\nwhile not p.exists():time.sleep(.005)\nos._exit(0)\n",
        pid_path.to_str().unwrap()
    );
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(s) = fs::read_to_string(&self.0)
                && let Ok(pid) = s.parse::<i32>()
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
    let _cleanup = Cleanup(pid_path);
    let start = Instant::now();
    let outcome = vardr::process::execute(
        &["python3".into(), "-c".into(), code],
        &store.root,
        Some(&"x".repeat(2_000_000)),
        &store.root.join("escaped.log"),
        4096,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(outcome.status, "succeeded");
    assert!(start.elapsed() < Duration::from_secs(3));
}
