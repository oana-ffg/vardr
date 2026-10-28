use crate::model::*;
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Release flock explicitly: a concurrent fork can inherit the open file description,
/// so closing this process's descriptor alone need not release the lock on Linux.
pub struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Where a command's data lives.
pub enum Home {
    Ready(PathBuf),
    /// Pre-0.5.0 data, or an unfinished move, that `vardr migrate` must complete first.
    NeedsMigration {
        from: PathBuf,
        to: PathBuf,
    },
}

/// Present from just before a migration moves data until its scheduler is replaced.
pub const MIGRATION_RECORD: &str = "migration.json";

fn project_data(qualifier: &str, organization: &str) -> Result<PathBuf> {
    Ok(
        directories::ProjectDirs::from(qualifier, organization, "vardr")
            .context("home directory required")?
            .data_local_dir()
            .to_owned(),
    )
}

/// Compares locations that may not exist yet.
fn same_location(a: &Path, b: &Path) -> bool {
    let resolve = |path: &Path| {
        path.canonicalize()
            .ok()
            .or_else(|| Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?)))
            .unwrap_or_else(|| path.to_owned())
    };
    resolve(a) == resolve(b)
}

/// Resolve `--home`/`VARDR_HOME`, or the default. Never start an empty installation
/// beside unmigrated data: that would hide existing jobs and could install a second
/// scheduler.
pub fn resolve_home(explicit: Option<PathBuf>) -> Result<Home> {
    let home = project_data("com", "firefungames")?;
    // The pre-0.5.0 macOS default. Other platforms derive one path from either identity.
    let legacy = Some(project_data("dev", "vardr")?).filter(|legacy| *legacy != home);
    let root = match explicit {
        Some(root) => {
            if let Some(legacy) = &legacy {
                ensure!(
                    !same_location(&root, legacy),
                    "{} is Vardr's data directory from before 0.5.0; `vardr migrate` without --home or VARDR_HOME moves it to {}",
                    root.display(),
                    home.display()
                );
            }
            root
        }
        // After migration a regular file marks the legacy path; see `migration`.
        None => match legacy {
            Some(legacy) if fs::symlink_metadata(&legacy).is_ok_and(|m| !m.is_file()) => {
                // A migration interrupted before its swap leaves its note at `home`.
                let interrupted = legacy.join(MIGRATION_RECORD).exists()
                    && fs::symlink_metadata(&home).is_ok_and(|m| m.is_file());
                ensure!(
                    interrupted || fs::symlink_metadata(&home).is_err(),
                    "Vardr data exists at both {} and {}; keep only one before continuing",
                    legacy.display(),
                    home.display()
                );
                return Ok(Home::NeedsMigration {
                    from: legacy,
                    to: home,
                });
            }
            _ => home,
        },
    };
    Ok(if root.join(MIGRATION_RECORD).exists() {
        Home::NeedsMigration {
            from: root.clone(),
            to: root,
        }
    } else {
        Home::Ready(root)
    })
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}
impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure!(root.is_absolute(), "data directory must be absolute");
        reject_symlinks(&root)?;
        for sub in ["jobs", "runs", "locks", "state"] {
            reject_symlinks(&root.join(sub))?;
            fs::create_dir_all(root.join(sub))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }
    pub fn settings(&self) -> Result<Settings> {
        let p = self.root.join("config.json");
        let s: Settings = if p.exists() {
            read_json(&p)?
        } else {
            Settings::default()
        };
        s.validate()?;
        Ok(s)
    }
    pub fn jobs(&self) -> Result<JobInventory> {
        let mut inventory = JobInventory::default();
        for entry in fs::read_dir(self.root.join("jobs"))? {
            let p = entry?.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let result = (|| -> Result<Job> {
                let job: Job = read_json(&p)?;
                job.validate()?;
                ensure!(
                    p.file_stem().unwrap() == job.id.as_str(),
                    "job ID does not match filename"
                );
                Ok(job)
            })();
            match result {
                Ok(job) => inventory.jobs.push(job),
                Err(e) => {
                    inventory.errors.insert(
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        format!("{e:#}"),
                    );
                }
            }
        }
        inventory.jobs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(inventory)
    }
    fn job_path(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.root.join("jobs").join(format!("{id}.json")))
    }
    pub fn job(&self, id: &str) -> Result<Job> {
        let path = self.job_path(id)?;
        ensure!(path.exists(), "no job named {id}");
        read_json(&path)
    }
    /// Callers that write per-job files check this under the registry lock or the
    /// job's state lock. `remove` holds both while deleting, so a removed job's files
    /// are never recreated.
    pub fn has_job(&self, id: &str) -> Result<bool> {
        Ok(self.job_path(id)?.exists())
    }
    /// Delete a job with its schedule state and run history; refused while it runs.
    /// Also finishes an interrupted removal whose definition is already gone. Lock
    /// files stay: a waiter may still hold one open, and unlinking it would let a
    /// recreated job with the same ID take a second, independent lock.
    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.job_path(id)?;
        let _registry = self.transaction("registry")?;
        let _run = self
            .lock(&format!("run-{id}"))
            .with_context(|| format!("{id} is running; remove it after the run finishes"))?;
        let _state = self.transaction(&format!("state-{id}"))?;
        let state = self.root.join("state").join(format!("{id}.json"));
        let runs = self.root.join("runs").join(id);
        ensure!(
            path.exists() || state.exists() || runs.exists(),
            "no job named {id}"
        );
        // The definition goes first: without it nothing schedules the job, whereas
        // a job without its state could replay occurrences it already ran.
        for file in [&path, &state] {
            reject_symlinks(file)?;
            match fs::remove_file(file) {
                Ok(()) => {
                    File::open(file.parent().context("missing parent directory")?)?.sync_all()?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        reject_symlinks(&runs)?;
        if runs.exists() {
            fs::remove_dir_all(&runs)?;
        }
        Ok(())
    }
    pub fn save(&self, mut job: Job) -> Result<Job> {
        job.validate()?;
        let _lock = self.transaction("registry")?;
        let path = self.root.join("jobs").join(format!("{}.json", job.id));
        let previous: Option<Job> = if path.exists() {
            Some(read_json(&path)?)
        } else {
            None
        };
        ensure!(
            job.revision == previous.as_ref().map_or(0, |x| x.revision),
            "job changed since it was read; fetch it again before saving"
        );
        job.revision += 1;
        job.updated_at = Some(Utc::now());
        // The scheduler uses updated_at as a floor after a definition change. This avoids
        // a second state-file write, and prevents stale cursors from replaying old work.
        write_json(&path, &job)?;
        Ok(job)
    }
    pub fn state(&self, id: &str) -> Result<RuntimeState> {
        validate_id(id)?;
        let p = self.root.join("state").join(format!("{id}.json"));
        if p.exists() {
            read_json(&p)
        } else {
            Ok(RuntimeState::default())
        }
    }
    pub fn save_state(&self, id: &str, state: &RuntimeState) -> Result<()> {
        validate_id(id)?;
        write_json(&self.root.join("state").join(format!("{id}.json")), state)
    }
    pub fn run_dir(&self, job: &str, run: &str) -> Result<PathBuf> {
        validate_id(job)?;
        validate_id(run)?;
        Ok(self.root.join("runs").join(job).join(run))
    }
    pub fn record(&self, run: &Run) -> Result<()> {
        let dir = self.run_dir(&run.job_id, &run.id)?;
        fs::create_dir_all(&dir)?;
        write_json(&dir.join("run.json"), run)
    }
    pub fn runs(&self, id: &str) -> Result<Vec<Run>> {
        validate_id(id)?;
        let path = self.root.join("runs").join(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut runs = Vec::new();
        for entry in fs::read_dir(path)? {
            let path = entry?.path().join("run.json");
            if path.exists() {
                runs.push(read_json::<Run>(&path)?);
            }
        }
        runs.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        Ok(runs)
    }
    pub fn prune(&self, id: &str, keep: usize) -> Result<()> {
        for run in self
            .runs(id)?
            .into_iter()
            .filter(|r| r.finished_at.is_some())
            .skip(keep)
        {
            let dir = self.run_dir(id, &run.id)?;
            reject_symlinks(&dir)?;
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
    fn lock_file(&self, name: &str) -> Result<File> {
        validate_id(name)?;
        let p = self.root.join("locks").join(format!("{name}.lock"));
        reject_symlinks(&p)?;
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(p)?)
    }
    pub fn lock(&self, name: &str) -> Result<Lock> {
        let file = self.lock_file(name)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .with_context(|| format!("{name} is already running or being changed"))?;
        Ok(Lock(file))
    }
    pub fn transaction(&self, name: &str) -> Result<Lock> {
        let file = self.lock_file(name)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Lock(file)),
                Err(e)
                    if e.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(e) => return Err(e).with_context(|| format!("timed out acquiring {name}")),
            }
        }
    }
    pub fn is_locked(&self, name: &str) -> Result<bool> {
        let file = self.lock_file(name)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                fs2::FileExt::unlock(&file)?;
                Ok(false)
            }
            Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(true),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn reject_symlinks(path: &Path) -> Result<()> {
    // Canonicalize the caller's data root before using it; still refuse redirections
    // introduced under that root, including a symlink in a missing leaf's ancestors.
    for p in path.ancestors() {
        if let Ok(metadata) = fs::symlink_metadata(p) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "refusing symlink: {}",
                p.display()
            );
        }
    }
    Ok(())
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    reject_symlinks(path)?;
    let mut data = Vec::new();
    File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 4 * 1024 * 1024, "JSON file too large");
    Ok(serde_json::from_slice(&data)?)
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    reject_symlinks(path)?;
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut tmp, value)?;
    tmp.write_all(b"\n")?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
