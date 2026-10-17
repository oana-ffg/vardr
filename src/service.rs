use crate::store::{Store, reject_symlinks};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, process::Command};

const LABEL: &str = "dev.vardr.runner";
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub fn plan(store: &Store) -> Result<String> {
    let helper = std::env::current_exe()?;
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());
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
        escape(&path)
    ))
}
#[derive(Deserialize, Serialize)]
struct Receipt {
    path: PathBuf,
    content: String,
}
fn service_path() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("home directory unavailable")?
        .home_dir()
        .join("Library/LaunchAgents/dev.vardr.runner.plist"))
}
pub fn install(store: &Store) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "service installation currently supports macOS; run `vardr serve` under your OS supervisor elsewhere"
    );
    let _lock = store.lock("service-install")?;
    let path = service_path()?;
    reject_symlinks(&path)?;
    let content = plan(store)?;
    if path.exists() {
        anyhow::bail!(
            "{} already exists; remove the recorded service before reinstalling",
            path.display()
        );
    }
    fs::create_dir_all(path.parent().unwrap())?;
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?
        .write_all(content.as_bytes())?;
    let receipt = Receipt {
        path: path.clone(),
        content,
    };
    crate::store::write_json(&store.root.join("service-install.json"), &receipt)?;
    let uid = unsafe { libc::getuid() };
    let status = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(&path)
        .status()?;
    ensure!(
        status.success(),
        "launchctl bootstrap failed; installation receipt retained for `service remove`"
    );
    Ok(())
}
pub fn remove(store: &Store) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "service removal currently supports macOS"
    );
    let _lock = store.lock("service-install")?;
    let receipt_path = store.root.join("service-install.json");
    let receipt: Receipt = crate::store::read_json(&receipt_path)?;
    ensure!(
        receipt.path == service_path()?,
        "receipt points outside Vardr's service path"
    );
    reject_symlinks(&receipt.path)?;
    ensure!(
        fs::read_to_string(&receipt.path)? == receipt.content,
        "service was edited externally; refusing removal"
    );
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{LABEL}");
    let loaded = Command::new("launchctl")
        .args(["print", &target])
        .output()?
        .status
        .success();
    if loaded {
        ensure!(
            Command::new("launchctl")
                .args(["bootout", &target])
                .status()?
                .success(),
            "launchctl bootout failed; service preserved"
        );
    }
    fs::remove_file(receipt.path)?;
    fs::remove_file(receipt_path)?;
    Ok(())
}
