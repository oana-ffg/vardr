use crate::model::*;
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

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
            if !p.extension().is_some_and(|x| x == "json") {
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
    pub fn job(&self, id: &str) -> Result<Job> {
        validate_id(id)?;
        read_json(&self.root.join("jobs").join(format!("{id}.json")))
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
    pub fn lock(&self, name: &str) -> Result<File> {
        let file = self.lock_file(name)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .with_context(|| format!("{name} is already running or being changed"))?;
        Ok(file)
    }
    pub fn transaction(&self, name: &str) -> Result<File> {
        let file = self.lock_file(name)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
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
            Ok(()) => Ok(false),
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
