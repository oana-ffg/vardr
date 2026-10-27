//! One-time move of macOS installations from before 0.5.0 to the `com.firefungames.vardr`
//! identity. Remove once no supported installation predates 0.5.0.
use crate::{
    model::ServiceState,
    service,
    store::{DefaultHome, Lock, Store, default_home, read_json},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const LEGACY_LABEL: &str = "dev.vardr.runner";

#[derive(Serialize)]
pub struct Migrated {
    pub home: PathBuf,
    /// Whether a pre-0.5.0 scheduler was replaced by one under the new identity.
    pub scheduler: bool,
}

/// Move the default data directory and replace its scheduler, whose helper is copied
/// from `source`. Refuses instead of interrupting running scheduled work.
pub fn migrate(source: &Path) -> Result<Migrated> {
    ensure!(
        cfg!(target_os = "macos"),
        "only macOS installations from before 0.5.0 need migration"
    );
    let DefaultHome::NeedsMigration { legacy, home } = default_home()? else {
        bail!("nothing to migrate: no Vardr data from before 0.5.0 was found");
    };
    let old = Store::open(legacy.canonicalize()?)?;
    let installing = old.lock("service-install")?;
    let legacy_plist = service::plist_path(LEGACY_LABEL)?;
    let launch_path = if old.root.join("service-install.json").exists() {
        let receipt = service::owned_receipt(&old, &legacy_plist)?;
        Some(launch_path(&receipt.path)?)
    } else {
        ensure!(
            fs::symlink_metadata(&legacy_plist).is_err(),
            "{} was not installed from this Vardr data; remove it before migrating",
            legacy_plist.display()
        );
        None
    };
    let loaded = launch_path.is_some() && stop(&old, &legacy_plist)?;
    let moved = (|| -> Result<()> {
        let _locks = quiesce(&old)?;
        fs::rename(&old.root, &home)?;
        Ok(())
    })();
    drop(installing);
    if let Err(error) = moved {
        if loaded && let Err(restart) = service::bootstrap(&legacy_plist) {
            return Err(error.context(format!(
                "Vardr data was not moved, and its previous scheduler could not be restarted: {restart:#}"
            )));
        }
        return Err(error.context("Vardr data was not moved"));
    }
    let Some(launch_path) = launch_path else {
        return Ok(Migrated {
            home,
            scheduler: false,
        });
    };
    let store = Store::open(home.canonicalize()?)?;
    // The moved receipt and the legacy plist describe the retired scheduler.
    (|| -> Result<()> {
        fs::remove_file(&legacy_plist)?;
        fs::remove_file(store.root.join("service-install.json"))?;
        service::install_with_path(&store, source, &launch_path)
    })()
    .with_context(|| {
        format!(
            "Vardr data moved to {}, but replacing its scheduler failed",
            home.display()
        )
    })?;
    Ok(Migrated {
        home,
        scheduler: true,
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

/// Stop the old scheduler while idle and wait until it releases the store. Returns
/// whether launchd had it loaded, so a failed move can restart it.
fn stop(old: &Store, plist: &Path) -> Result<bool> {
    if old.is_locked("service")? {
        let path = old.root.join("service.json");
        let state: ServiceState = if path.exists() {
            read_json(&path)?
        } else {
            ServiceState::default()
        };
        // Work dispatched after this check is interrupted and recorded as such by the
        // scheduler's normal shutdown and recovery.
        ensure!(
            state.running.is_empty(),
            "scheduled work is running ({}); migrate once it finishes",
            state.running.join(", ")
        );
    }
    let loaded = service::bootout(LEGACY_LABEL)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while old.is_locked("service")? {
        if Instant::now() >= deadline {
            let restart = if loaded {
                service::bootstrap(plist).map_or_else(
                    |e| format!("; restarting it also failed: {e:#}"),
                    |()| "; it was restarted".into(),
                )
            } else {
                "; it is not run by the recorded LaunchAgent".into()
            };
            bail!("the previous scheduler did not stop within 60 seconds{restart}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(loaded)
}

/// Hold every store lock so no command, dashboard run or investigation uses the
/// directory while it moves. The caller already holds `service-install`.
fn quiesce(store: &Store) -> Result<Vec<Lock>> {
    let mut locks = Vec::new();
    for entry in fs::read_dir(store.root.join("locks"))? {
        let path = entry?.path();
        let Some(name) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|_| path.extension().is_some_and(|ext| ext == "lock"))
        else {
            continue;
        };
        if name != "service-install" {
            locks.push(store.lock(name).context(
                "Vardr data is in use; quit other Vardr windows and commands, then retry",
            )?);
        }
    }
    Ok(locks)
}
