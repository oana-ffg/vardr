//! One-time move of macOS installations from before 0.5.0 to the `com.firefungames.vardr`
//! identity. Remove once no supported installation predates 0.5.0.
//!
//! A record is written before data moves and removed once the new scheduler runs, so a
//! failed or interrupted migration is finished by running `vardr migrate` again.
use crate::{
    model::ServiceState,
    service::{self, LABEL, LEGACY_LABEL, Receipt},
    store::{Home, Lock, MIGRATION_RECORD, Store, read_json, resolve_home, write_json},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// The longest the old scheduler may take to stop. Migration refuses when a job is due
/// within it, so the old scheduler has nothing to dispatch while stopping.
const STOP_WINDOW: Duration = Duration::from_secs(60);

#[derive(Serialize)]
pub struct Migrated {
    pub home: PathBuf,
    /// Whether a pre-0.5.0 scheduler was replaced by one under the new identity.
    pub scheduler: bool,
    /// Jobs whose last scheduled occurrence may not have run; see `preserve_failures`.
    pub unconfirmed: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Record {
    scheduler: Option<Legacy>,
    unconfirmed: Vec<String>,
}

/// The stopped pre-0.5.0 scheduler: its unedited LaunchAgent and the PATH its jobs use.
#[derive(Deserialize, Serialize)]
struct Legacy {
    receipt: Receipt,
    launch_path: String,
}

/// Move the default data directory, or keep an explicit one, and replace its pre-0.5.0
/// scheduler with one whose helper is copied from `source`. Refuses instead of
/// interrupting running work.
pub fn migrate(explicit: Option<PathBuf>, source: &Path) -> Result<Migrated> {
    let (from, to) = match resolve_home(explicit)? {
        Home::NeedsMigration { from, to } => (from, to),
        Home::Ready(root) if root.is_dir() => (root.clone(), root),
        Home::Ready(_) => bail!("nothing to migrate: no Vardr data from before 0.5.0 was found"),
    };
    let moving = from != to;
    let old = Store::open(from.canonicalize()?)?;
    if moving || !old.root.join(MIGRATION_RECORD).exists() {
        start(&old, &to, moving)?;
    }
    finish(&to, source)
}

fn start(old: &Store, to: &Path, moving: bool) -> Result<()> {
    let _installing = old.lock("service-install")?;
    let legacy_plist = service::plist_path(LEGACY_LABEL)?;
    let receipt = old.root.join("service-install.json");
    let scheduler = if receipt.exists() && read_json::<Receipt>(&receipt)?.path == legacy_plist {
        let receipt = service::owned_receipt(old, &legacy_plist)?;
        Some(Legacy {
            launch_path: launch_path(&receipt.path)?,
            receipt,
        })
    } else {
        ensure!(
            !moving || fs::symlink_metadata(&legacy_plist).is_err(),
            "{} was not installed from this Vardr data; remove it before migrating",
            legacy_plist.display()
        );
        None
    };
    ensure!(
        moving || scheduler.is_some(),
        "nothing to migrate: {} has no scheduler from before 0.5.0",
        old.root.display()
    );
    idle(old)?;
    let checked = Instant::now();
    let loaded = scheduler.is_some() && service::bootout(LEGACY_LABEL)?;
    let deadline = if scheduler.is_some() {
        checked + STOP_WINDOW
    } else {
        checked
    };
    (|| -> Result<()> {
        let mut locks = vec![stopped(old, deadline)?];
        locks.extend(quiesce(old)?);
        let path = old.root.join(MIGRATION_RECORD);
        // An earlier attempt interrupted before its swap may have preserved failures.
        let mut unconfirmed = if path.exists() {
            read_json::<Record>(&path)?.unconfirmed
        } else {
            Vec::new()
        };
        unconfirmed.extend(preserve_failures(old)?);
        write_json(
            &path,
            &Record {
                scheduler,
                unconfirmed,
            },
        )?;
        if moving {
            swap(&old.root, to)?;
        }
        Ok(())
    })()
    .map_err(|error| {
        if !loaded {
            return error;
        }
        match service::bootstrap(&legacy_plist) {
            Ok(()) => anyhow!("{error:#}; the previous scheduler was restarted"),
            Err(restart) => {
                anyhow!("{error:#}; restarting the previous scheduler also failed: {restart:#}")
            }
        }
    })
}

/// Each step is idempotent, so this also resumes after a failure.
fn finish(root: &Path, source: &Path) -> Result<Migrated> {
    let store = Store::open(root.canonicalize()?)?;
    let _installing = store.lock("service-install")?;
    let path = store.root.join(MIGRATION_RECORD);
    let record: Record = read_json(&path)?;
    let replaced = record.scheduler.is_some();
    let unconfirmed = record.unconfirmed;
    if let Some(legacy) = record.scheduler {
        (|| -> Result<()> {
            let plist = service::plist_path(LABEL)?;
            if fs::symlink_metadata(&plist).is_err() {
                service::write(&store, source, &legacy.launch_path)?;
            } else {
                service::owned_receipt(&store, &plist)?;
            }
            if !service::loaded(LABEL)? {
                service::bootstrap(&plist)?;
            }
            // The old agent was stopped before the move; retire it only once its
            // replacement runs.
            let old = &legacy.receipt.path;
            if fs::symlink_metadata(old).is_ok() {
                ensure!(
                    fs::read_to_string(old)? == legacy.receipt.content,
                    "{} was edited during migration; refusing to remove it",
                    old.display()
                );
                fs::remove_file(old)?;
            }
            Ok(())
        })()
        .with_context(|| {
            format!(
                "Vardr data is in {}, but its scheduler was not replaced; run `vardr migrate` again to finish",
                store.root.display()
            )
        })?;
    }
    fs::remove_file(path)?;
    Ok(Migrated {
        home: store.root,
        scheduler: replaced,
        unconfirmed,
    })
}

/// Keep the PATH that jobs already run with, not the migrating process's PATH.
fn launch_path(plist: &Path) -> Result<String> {
    let output = crate::process::capture(
        &[
            "/usr/bin/plutil".into(),
            "-extract".into(),
            "EnvironmentVariables.PATH".into(),
            "raw".into(),
            "-o".into(),
            "-".into(),
            plist.to_string_lossy().into_owned(),
        ],
        plist
            .parent()
            .context("LaunchAgent has no parent directory")?,
        64 * 1024,
        Duration::from_secs(10),
    )
    .context("could not read the previous scheduler's PATH")?;
    Ok(String::from_utf8(output)?
        .strip_suffix('\n')
        .context("unexpected plutil output")?
        .to_owned())
}

/// Refuse unless nothing runs and no job is due before the old scheduler has stopped.
/// Pre-0.5.0 code cannot be paused, so this is checked without holding its locks. A run
/// it reserved in the instant before the check is recorded as interrupted, as with any
/// scheduler stop.
fn idle(store: &Store) -> Result<()> {
    let now = Utc::now();
    let until = now + chrono::Duration::from_std(STOP_WINDOW)?;
    let service = store.root.join("service.json");
    if store.is_locked("service")? && service.exists() {
        let state: ServiceState = read_json(&service)?;
        ensure!(
            state.running.is_empty(),
            "{} is running; migrate once it finishes",
            state.running.join(", ")
        );
    }
    for job in store.jobs()?.jobs {
        ensure!(
            !store.is_locked(&format!("run-{}", job.id))?,
            "{} is running; migrate once it finishes",
            job.id
        );
        let state = store.state(&job.id)?;
        if job.enabled
            && let Some(due) = crate::scheduler::due_by(&job, &state, now, until)?
        {
            bail!(
                "{} is due at {}; migrate once it has run",
                job.id,
                due.with_timezone(&Local).format("%H:%M:%S")
            );
        }
    }
    Ok(())
}

/// With the scheduler stopped, hold every lock so no other command uses the data while
/// it moves. Run locks come first: a run in progress needs the others to finish.
fn quiesce(store: &Store) -> Result<Vec<Lock>> {
    let jobs: Vec<String> = store.jobs()?.jobs.into_iter().map(|job| job.id).collect();
    let mut locks = Vec::new();
    for id in &jobs {
        locks.push(
            store
                .lock(&format!("run-{id}"))
                .with_context(|| format!("{id} is running; migrate once it finishes"))?,
        );
    }
    locks.push(store.transaction("registry")?);
    ensure!(
        store.jobs()?.jobs.iter().map(|job| &job.id).eq(jobs.iter()),
        "job definitions changed while migrating; retry"
    );
    let held: BTreeSet<String> = jobs
        .iter()
        .map(|id| format!("run-{id}"))
        .chain(["registry", "service", "service-install"].map(String::from))
        .collect();
    for entry in fs::read_dir(store.root.join("locks"))? {
        let path = entry?.path();
        let Some(name) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|_| path.extension().is_some_and(|ext| ext == "lock"))
        else {
            continue;
        };
        if !held.contains(name) {
            locks.push(store.transaction(name).context(
                "Vardr data is in use; quit other Vardr windows and commands, then retry",
            )?);
        }
    }
    Ok(locks)
}

/// A stopped 0.4.x scheduler keeps worker failures only in its final status report,
/// which the replacement scheduler overwrites. A worker can fail before recording its
/// reserved run, so keep each failure in that job's history. Nothing is replayed.
fn preserve_failures(store: &Store) -> Result<Vec<String>> {
    let path = store.root.join("service.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut state: ServiceState = read_json(&path)?;
    let mut preserved = Vec::new();
    for job in store.jobs()?.jobs {
        let Some(error) = state.errors.remove(&job.id) else {
            continue;
        };
        let mut run = crate::runner::new_record(&job);
        run.status = "interrupted".into();
        run.finished_at = Some(Utc::now());
        run.error = Some(format!(
            "the scheduler stopped for the 0.5.0 migration had reported: {error}; this occurrence may not have run, so inspect its effects before retrying"
        ));
        store.record(&run)?;
        preserved.push(job.id);
    }
    if !preserved.is_empty() {
        write_json(&path, &state)?;
    }
    Ok(preserved)
}

/// Wait until the stopped scheduler releases the store.
fn stopped(store: &Store, deadline: Instant) -> Result<Lock> {
    loop {
        match store.lock("service") {
            Ok(lock) => return Ok(lock),
            Err(error) if Instant::now() >= deadline => {
                return Err(error.context("the previous scheduler did not stop"));
            }
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Atomically put the data at `to` and leave a file at `from`. Pre-0.5.0 processes that
/// still use the old path then fail instead of recreating an empty installation there.
fn swap(from: &Path, to: &Path) -> Result<()> {
    use std::io::Write;
    let note = format!(
        "Vardr 0.5.0 moved this data directory to {}\n",
        to.display()
    );
    match fs::OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(mut file) => file.write_all(note.as_bytes())?,
        // Left by a migration interrupted before its swap.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
            fs::symlink_metadata(to)?.is_file() && fs::read_to_string(to)? == note,
            "{} already exists",
            to.display()
        ),
        Err(error) => return Err(error.into()),
    }
    let path = |path: &Path| CString::new(path.as_os_str().as_bytes());
    if unsafe { libc::renamex_np(path(from)?.as_ptr(), path(to)?.as_ptr(), libc::RENAME_SWAP) } != 0
    {
        let error = std::io::Error::last_os_error();
        fs::remove_file(to).with_context(|| format!("could not move Vardr's data: {error}"))?;
        return Err(error).context("could not move Vardr's data");
    }
    Ok(())
}
