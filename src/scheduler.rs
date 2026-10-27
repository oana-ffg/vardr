use crate::{
    model::*,
    process::STOP,
    runner::{self, Workers},
    store::{Store, write_json},
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
    let _service_lock = store.transaction("service")?;
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
    write_json(
        &store.root.join("service.json"),
        &ServiceState {
            updated_at: Some(Utc::now()),
            pid,
            running: running.to_vec(),
            errors: errors.clone(),
        },
    )
}
fn serve_loop(store: &Store, once: bool) -> Result<bool> {
    let mut errors = recover_interrupted(store)?;
    let mut workers = Workers::default();
    let mut last_heartbeat = Instant::now() - Duration::from_secs(31);
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
        if last_heartbeat.elapsed() >= Duration::from_secs(30)
            || last_reported.as_ref() != Some(&summary)
        {
            report(store, &running, &errors, std::process::id())?;
            last_heartbeat = Instant::now();
            last_reported = Some(summary);
        }
        if once {
            for (id, result) in workers.finish() {
                match result {
                    Ok(run)
                        if matches!(
                            run.status.as_str(),
                            "succeeded" | "dispatched" | "skipped"
                        ) => {}
                    Ok(run) => {
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
        thread::sleep(Duration::from_secs(2));
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
    for job in inventory
        .jobs
        .into_iter()
        .filter(|j| j.enabled && !workers.tasks.contains_key(&j.id))
    {
        let result = (|| -> Result<Option<chrono::DateTime<Utc>>> {
            let _state_lock = store.transaction(&format!("state-{}", job.id))?;
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
        let reservation = (|| -> Result<()> {
            let _state_lock = store.transaction(&format!("state-{id}"))?;
            let mut state = store.state(&id)?;
            // Persist intent first. Crashes never replay a possibly side-effecting run.
            state.watermark = Some(due);
            store.save_state(&id, &state)
        })();
        if let Err(e) = reservation {
            errors.insert(format!("job:{id}"), format!("{e:#}"));
            continue;
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
