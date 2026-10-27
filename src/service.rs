use crate::store::{Store, reject_symlinks};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const LABEL: &str = "com.firefungames.vardr.runner";
/// Schedulers installed before 0.5.0; `vardr migrate` replaces them.
pub(crate) const LEGACY_LABEL: &str = "dev.vardr.runner";
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn current_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
}
pub fn plan(store: &Store) -> Result<String> {
    plan_with_helper(store, &store.root.join("bin/vardr"), &current_path())
}
fn plan_with_helper(store: &Store, helper: &Path, path: &str) -> Result<String> {
    let items = [
        helper.to_string_lossy().into_owned(),
        "--home".into(),
        store.root.to_string_lossy().into_owned(),
        "serve".into(),
    ];
    let args = items
        .iter()
        .map(|x| format!("<string>{}</string>", escape(x)))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array>{args}</array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>\n<key>StandardOutPath</key><string>/dev/null</string>\n<key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n",
        escape(path)
    ))
}
#[derive(Deserialize, Serialize)]
pub(crate) struct Receipt {
    pub(crate) path: PathBuf,
    pub(crate) content: String,
}
pub(crate) fn plist_path(label: &str) -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("home directory unavailable")?
        .home_dir()
        .join(format!("Library/LaunchAgents/{label}.plist")))
}
pub fn install(store: &Store) -> Result<()> {
    install_from(store, &std::env::current_exe()?)
}
/// Desktop bundles supply their CLI explicitly; launchd must never run the UI executable.
pub fn install_from(store: &Store, source: &Path) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "service installation currently supports macOS; run `vardr serve` under your OS supervisor elsewhere"
    );
    let _lock = store.lock("service-install")?;
    let legacy = plist_path(LEGACY_LABEL)?;
    ensure!(
        fs::symlink_metadata(&legacy).is_err(),
        "{} from Vardr before 0.5.0 is still installed; run `vardr migrate`, with the --home it was installed from, to replace it",
        legacy.display()
    );
    let path = plist_path(LABEL)?;
    reject_symlinks(&path)?;
    if path.exists() {
        anyhow::bail!(
            "{} already exists; remove the recorded service before reinstalling",
            path.display()
        );
    }
    write(store, source, &current_path())?;
    bootstrap(&path).context("installation receipt retained for `service remove`")
}
/// Write the LaunchAgent and its receipt without loading it. An existing LaunchAgent is
/// accepted only if it is exactly what this writes, which completes an interrupted
/// write. The caller holds `service-install`.
pub(crate) fn write(store: &Store, source: &Path, launch_path: &str) -> Result<PathBuf> {
    let path = plist_path(LABEL)?;
    reject_symlinks(&path)?;
    let _service = store
        .lock("service")
        .context("stop the running scheduler before installing")?;
    let helper = stage_helper(store, source)?;
    let content = plan_with_helper(store, &helper, launch_path)?;
    match fs::read_to_string(&path) {
        Ok(existing) => ensure!(
            existing == content,
            "{} already exists; remove the recorded service before reinstalling",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path.parent().unwrap())?;
            use std::io::Write;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?
                .write_all(content.as_bytes())?;
        }
        Err(error) => return Err(error.into()),
    }
    let receipt = Receipt {
        path: path.clone(),
        content,
    };
    crate::store::write_json(&store.root.join("service-install.json"), &receipt)?;
    Ok(path)
}
pub fn remove(store: &Store) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "service removal currently supports macOS"
    );
    let _lock = store.lock("service-install")?;
    let receipt = owned_receipt(store, &plist_path(LABEL)?)?;
    bootout(LABEL)?;
    fs::remove_file(receipt.path)?;
    fs::remove_file(store.root.join("service-install.json"))?;
    Ok(())
}
/// The caller holds `service-install`. Refuse a LaunchAgent this data directory did not
/// write, or one edited since.
pub(crate) fn owned_receipt(store: &Store, expected: &Path) -> Result<Receipt> {
    let receipt: Receipt = crate::store::read_json(&store.root.join("service-install.json"))?;
    ensure!(
        receipt.path == expected,
        "the recorded service is {}, not {}; `vardr migrate` replaces a scheduler from before 0.5.0",
        receipt.path.display(),
        expected.display()
    );
    reject_symlinks(&receipt.path)?;
    ensure!(
        fs::read_to_string(&receipt.path)? == receipt.content,
        "service was edited externally; refusing to change it"
    );
    Ok(receipt)
}
fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}
pub(crate) fn loaded(label: &str) -> Result<bool> {
    Ok(Command::new("launchctl")
        .args(["print", &format!("{}/{label}", domain())])
        .output()?
        .status
        .success())
}
/// Returns whether launchd had the agent loaded.
pub(crate) fn bootout(label: &str) -> Result<bool> {
    let loaded = loaded(label)?;
    if loaded {
        ensure!(
            Command::new("launchctl")
                .args(["bootout", &format!("{}/{label}", domain())])
                .status()?
                .success(),
            "launchctl bootout failed; service preserved"
        );
    }
    Ok(loaded)
}
pub(crate) fn bootstrap(plist: &Path) -> Result<()> {
    ensure!(
        Command::new("launchctl")
            .args(["bootstrap", &domain()])
            .arg(plist)
            .status()?
            .success(),
        "launchctl bootstrap failed for {}",
        plist.display()
    );
    Ok(())
}

/// Keep the service executable outside the app so an app update cannot remove it.
pub fn stage_helper(store: &Store, source: &Path) -> Result<PathBuf> {
    let directory = store.root.join("bin");
    let destination = directory.join("vardr");
    reject_symlinks(&destination)?;
    fs::create_dir_all(&directory)?;
    let version = |path: &Path| -> Result<semver::Version> {
        let output = crate::process::capture(
            &[path.to_string_lossy().into_owned(), "--version".into()],
            &directory,
            1024,
            std::time::Duration::from_secs(10),
        )?;
        let text = std::str::from_utf8(&output)?;
        Ok(semver::Version::parse(
            text.trim()
                .strip_prefix("vardr ")
                .context("invalid helper version")?,
        )?)
    };
    let source_version = version(source)?;
    if destination.exists() && version(&destination)? >= source_version {
        return Ok(destination);
    }
    let file = tempfile::NamedTempFile::new_in(&directory)?;
    fs::copy(source, file.path())?;
    file.as_file().sync_all()?;
    file.persist(&destination).map_err(|e| e.error)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(destination)
}
