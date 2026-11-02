use crate::{
    model::*,
    process::STOP,
    runner::{self, Workers},
    store::{self, Store},
    updates,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap,
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

/// Returns true after an idle automatic update, so main restarts the new binary.
pub fn serve(store: &Store, once: bool) -> Result<bool> {
    let _service_lock = store.transaction("service").map_err(|error| {
        if store::lock_contention(&error) {
            error.context(format!(
                "another `vardr serve` already owns {}; stop it first",
                store.root.display()
            ))
        } else {
            error
        }
    })?;
    let result = serve_loop(store, once);
    if let Err(error) = &result {
        // A bounded last-error record survives a failed startup under launchd.
        let _ = report(
            store,
            &[],
            &BTreeMap::from([("fatal".into(), format!("{error:#}"))]),
            0,
        );
    }
    result
}
fn report(
    store: &Store,
    running: &[String],
    errors: &BTreeMap<String, String>,
    pid: u32,
) -> Result<()> {
    store.report_service(&ServiceState {
        updated_at: Some(Utc::now()),
        pid,
        running: running.to_vec(),
        errors: errors.clone(),
    })
}
fn serve_loop(store: &Store, once: bool) -> Result<bool> {
    serve_loop_with_wait(store, once, || {
        thread::sleep(Duration::from_secs(2));
        true
    })
}
fn serve_loop_with_wait(store: &Store, once: bool, mut wait: impl FnMut() -> bool) -> Result<bool> {
    let mut errors = recover_interrupted(store)?;
    let mut workers = Workers::default();
    let mut last_heartbeat = Instant::now() - crate::heartbeat::INTERVAL;
    let mut last_reported = None;
    loop {
        for (id, result) in workers.reap() {
            match result {
                Ok(_) => {
                    errors.remove(&id);
                }
                Err(e) => {
                    errors.insert(id, format!("{e:#}"));
                }
            }
        }
        let stopped = STOP.load(Ordering::Relaxed);
        if stopped && workers.tasks.is_empty() {
            break;
        }
        if !stopped {
            errors.retain(|key, _| !key.starts_with("job:"));
            match dispatch_due(store, &mut workers) {
                Ok(job_errors) => {
                    errors.remove("scheduler");
                    errors.extend(job_errors);
                }
                Err(e) => {
                    errors.insert("scheduler".into(), format!("{e:#}"));
                }
            }
            if workers.tasks.is_empty() && !once {
                match updates::automatic(store) {
                    Ok(true) => {
                        report(store, &[], &errors, 0)?;
                        return Ok(true);
                    }
                    Ok(false) => (),
                    Err(e) => {
                        errors.insert("updater".into(), format!("{e:#}"));
                    }
                }
            }
        }
        let running: Vec<_> = workers.tasks.keys().cloned().collect();
        let summary = (running.clone(), errors.clone());
        if last_heartbeat.elapsed() >= crate::heartbeat::INTERVAL
            || last_reported.as_ref() != Some(&summary)
        {
            report(store, &running, &errors, std::process::id())?;
            last_heartbeat = Instant::now();
            last_reported = Some(summary);
        }
        if once {
            for (id, result) in workers.finish() {
                match result {
                    Ok(None) => {}
                    Ok(Some(run))
                        if matches!(
                            run.status.as_str(),
                            "succeeded" | "dispatched" | "skipped"
                        ) => {}
                    Ok(Some(run)) => {
                        errors.insert(id, format!("automation {}", run.status));
                    }
                    Err(e) => {
                        errors.insert(id, format!("{e:#}"));
                    }
                }
            }
            report(store, &[], &errors, 0)?;
            anyhow::ensure!(
                errors.is_empty(),
                "scheduler errors: {}",
                serde_json::to_string(&errors)?
            );
            return Ok(false);
        }
        if !wait() {
            break;
        }
    }
    report(store, &[], &errors, 0)?;
    Ok(false)
}
/// An unreserved occurrence due by `until`. Dispatch passes `until == now`.
pub(crate) fn due_by(
    job: &Job,
    state: &RuntimeState,
    now: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let baseline = job.updated_at.unwrap_or(now);
    let watermark = state.watermark.map_or(baseline, |w| w.max(baseline));
    job.schedule.due(watermark, until)
}
fn dispatch_due(store: &Store, workers: &mut Workers) -> Result<BTreeMap<String, String>> {
    let settings = store.settings()?;
    let now = Utc::now();
    let inventory = store.jobs()?;
    let mut errors: BTreeMap<_, _> = inventory
        .errors
        .into_iter()
        .map(|(id, e)| (format!("job:{id}"), e))
        .collect();
    let mut candidates = Vec::new();
    for job in inventory.jobs.into_iter().filter(|j| {
        j.enabled && j.schedule != Schedule::Manual && !workers.tasks.contains_key(&j.id)
    }) {
        let result = (|| -> Result<Option<chrono::DateTime<Utc>>> {
            if due_by(&job, &store.state(&job.id)?, now, now)?.is_none() {
                return Ok(None);
            }
            // Only due jobs need a transaction. Always re-read under the lock:
            // another runner may have changed the cursor since the first read.
            let _state_lock = store.transaction(&format!("state-{}", job.id))?;
            if !store.has_job(&job.id)? {
                return Ok(None);
            }
            let mut state = store.state(&job.id)?;
            if let Some(due) = due_by(&job, &state, now, now)? {
                if !job.catch_up && (now - due).num_seconds() > 60 {
                    state.watermark = Some(due);
                    store.save_state(&job.id, &state)?;
                } else {
                    return Ok(Some(due));
                }
            }
            Ok(None)
        })();
        match result {
            Ok(Some(due)) => candidates.push((due, job.id, job.revision)),
            Ok(None) => (),
            Err(e) => {
                errors.insert(format!("job:{}", job.id), format!("{e:#}"));
            }
        }
    }
    candidates.sort();
    for (due, id, revision) in candidates {
        if workers.tasks.len() >= settings.max_concurrency {
            break;
        }
        let reservation = (|| -> Result<bool> {
            let _state_lock = store.transaction(&format!("state-{id}"))?;
            if !store.has_job(&id)? {
                return Ok(false);
            }
            let mut state = store.state(&id)?;
            // Persist intent first. Crashes never replay a possibly side-effecting run.
            state.watermark = Some(due);
            store.save_state(&id, &state)?;
            Ok(true)
        })();
        match reservation {
            Ok(true) => (),
            Ok(false) => continue,
            Err(e) => {
                errors.insert(format!("job:{id}"), format!("{e:#}"));
                continue;
            }
        }
        let local = store.clone();
        workers.tasks.insert(
            id.clone(),
            thread::spawn(move || runner::run_scheduled(&local, &id, revision)),
        );
    }
    Ok(errors)
}
fn recover_interrupted(store: &Store) -> Result<BTreeMap<String, String>> {
    let mut errors = BTreeMap::new();
    for job in store.jobs()?.jobs {
        let id = &job.id;
        let Ok(_lock) = store.lock(&format!("run-{id}")) else {
            continue;
        };
        let result = (|| -> Result<()> {
            for mut run in store.runs(id)? {
                let investigation_running = run
                    .investigation
                    .as_ref()
                    .is_some_and(|i| i.status == "running");
                if run.status != "running" && !investigation_running {
                    continue;
                }
                if run.status == "running" {
                    run.status = "interrupted".into();
                    run.finished_at = Some(Utc::now());
                    run.error = Some(
                        "runner ended without a completion record; inspect effects before retrying"
                            .into(),
                    );
                }
                if investigation_running {
                    run.investigation = Some(Box::new(Outcome {
                        status: "interrupted".into(),
                        error: Some("investigation ended without a completion record".into()),
                        remote: None,
                    }));
                }
                store.record(&run)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            errors.insert(format!("recovery:{id}"), format!("{e:#}"));
        }
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{metrics, write_json};

    #[test]
    fn idle_heartbeat_performs_no_syncs() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().canonicalize().unwrap().join("data")).unwrap();
        let mut settings = Settings::default();
        settings.updates.automatic = true;
        write_json(&store.root.join("config.json"), &settings).unwrap();
        write_json(
            &store.root.join("updates.json"),
            &updates::UpdateState {
                last_attempt: Some(Utc::now()),
                ..Default::default()
            },
        )
        .unwrap();
        for n in 0..20 {
            let job: Job = serde_json::from_value(serde_json::json!({
                "id":format!("idle-{n}"), "name":"Idle", "purpose":"Measure idle scheduler IO",
                "cwd":store.root, "target":{"type":"command","argv":["/bin/true"]},
                "schedule":{"type":"interval","seconds":3600}
            }))
            .unwrap();
            let job = store.save(job).unwrap();
            store.save_state(&job.id, &RuntimeState::default()).unwrap();
        }
        let mut waits = 0;
        let mut measured_start = Instant::now();
        serve_loop_with_wait(&store, false, || {
            waits += 1;
            if waits == 1 {
                metrics::take();
                measured_start = Instant::now();
            }
            if waits == 17 {
                let counts = metrics::take();
                let reads = |part: &str| {
                    counts.reads.iter()
                        .filter(|(p, _)| p.parent().unwrap().ends_with(part))
                        .map(|(_, n)| n).sum::<usize>()
                };
                let file_reads = |name: &str| {
                    counts.reads.get(&store.root.join(name)).copied().unwrap_or(0)
                };
                eprintln!(
                    "Idle window ({:.3}s): job_reads={}, state_reads={}, config_reads={}, update_reads={}, locks={}, JSON_replacements={}, sync_all_calls={}, timer_waits=16",
                    measured_start.elapsed().as_secs_f64(), reads("jobs"), reads("state"),
                    file_reads("config.json"), file_reads("updates.json"), counts.locks, counts.writes, counts.syncs
                );
                assert_eq!(counts.locks, 0, "non-due jobs need no locks");
                assert!(counts.writes >= 1, "the idle window must include a heartbeat");
                assert_eq!(counts.syncs, 0);
                return false;
            }
            thread::sleep(Duration::from_secs(2));
            true
        })
        .unwrap();
    }

    #[test]
    fn scheduler_observes_edits_state_errors_and_removals() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().canonicalize().unwrap().join("data")).unwrap();
        let mut workers = Workers::default();
        let job: Job = serde_json::from_value(serde_json::json!({
            "id":"edited", "name":"Edit", "purpose":"Exercise idle dispatch",
            "cwd":store.root, "target":{"type":"command","argv":["/bin/echo","reloaded"]},
            "schedule":{"type":"interval","seconds":3600}
        }))
        .unwrap();
        let mut job = store.save(job).unwrap();
        let scan = |workers: &mut Workers| dispatch_due(&store, workers).unwrap();
        assert!(scan(&mut workers).is_empty());
        assert!(workers.tasks.is_empty());
        let path = store.root.join("jobs/edited.json");
        std::fs::write(&path, b"bad JSON").unwrap();
        assert!(scan(&mut workers).contains_key("job:edited.json"));
        assert!(workers.tasks.is_empty());
        // Repair with a due schedule, without restarting the scheduler.
        job.schedule = Schedule::Interval { seconds: 10 };
        job.updated_at = Some(Utc::now() - chrono::Duration::seconds(20));
        write_json(&path, &job).unwrap();
        // An externally advanced cursor must suppress that occurrence.
        store
            .save_state(
                &job.id,
                &RuntimeState {
                    watermark: Some(Utc::now()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(scan(&mut workers).is_empty());
        assert!(workers.tasks.is_empty());
        // A broken state must surface instead of dispatching from an assumed cursor.
        let state = store.root.join("state/edited.json");
        std::fs::write(&state, b"bad JSON").unwrap();
        assert!(scan(&mut workers).contains_key("job:edited"));
        // Removing the state restores the default and dispatches the due job.
        std::fs::remove_file(&state).unwrap();
        assert!(scan(&mut workers).is_empty());
        let results = workers.finish();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results
                .into_iter()
                .next()
                .unwrap()
                .1
                .unwrap()
                .unwrap()
                .status,
            "succeeded"
        );
        assert!(scan(&mut workers).is_empty());
        assert!(
            workers.tasks.is_empty(),
            "the reservation must prevent a replay"
        );
        store.remove(&job.id, None).unwrap();
        assert!(scan(&mut workers).is_empty());
    }
}
