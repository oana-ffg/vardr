//! Signed GitHub Release updates. Follows the standalone-helper distribution
//! pattern in Ratatoskr Skill Sync (MIT, copyright 2026 Oana Goge).
use crate::{
    model::parse_time,
    store::{Store, read_json, write_json},
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    time::Duration,
};

pub const ENDPOINT: &str = "https://github.com/oana-ffg/vardr/releases/latest/download/latest.json";
pub const PUBLIC_KEY: &str = include_str!("update-public-key.txt");
const MAX_BINARY: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Asset {
    pub url: String,
    pub signature: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Release {
    pub version: String,
    pub platforms: BTreeMap<String, Asset>,
}
#[derive(Default, Deserialize, Serialize)]
pub struct UpdateState {
    pub last_attempt: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub installed_version: Option<String>,
}
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(concat!("vardr/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .timeout(Duration::from_secs(30))
        .build()?)
}
pub fn check() -> Result<Option<Release>> {
    let response = client()?.get(ENDPOINT).send()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    response
        .error_for_status()?
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "update manifest too large");
    let release: Release = serde_json::from_slice(&bytes)?;
    if semver::Version::parse(&release.version)?
        <= semver::Version::parse(env!("CARGO_PKG_VERSION"))?
    {
        return Ok(None);
    }
    Ok(Some(release))
}
pub fn platform() -> String {
    format!(
        "{}-{}",
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            std::env::consts::OS
        },
        std::env::consts::ARCH
    )
}
pub fn verify(public_key: &str, bytes: &[u8], signature: &str) -> Result<()> {
    let key_text =
        String::from_utf8(base64::engine::general_purpose::STANDARD.decode(public_key.trim())?)?;
    let key = minisign_verify::PublicKey::decode(&key_text)?;
    let signature =
        String::from_utf8(base64::engine::general_purpose::STANDARD.decode(signature.trim())?)?;
    key.verify(
        bytes,
        &minisign_verify::Signature::decode(&signature)?,
        false,
    )?;
    Ok(())
}
pub fn install(release: &Release) -> Result<()> {
    ensure!(
        semver::Version::parse(&release.version)?
            > semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
        "refusing a downgrade"
    );
    let asset = release
        .platforms
        .get(&platform())
        .context("release has no binary for this platform")?;
    // An untrusted manifest may select a URL but cannot replace the trust key.
    let url = reqwest::Url::parse(&asset.url)?;
    ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "update asset must use HTTPS without credentials"
    );
    let mut bytes = Vec::new();
    client()?
        .get(url)
        .send()?
        .error_for_status()?
        .take(MAX_BINARY + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_BINARY, "update binary too large");
    verify(PUBLIC_KEY, &bytes, &asset.signature)?;
    let executable = std::env::current_exe()?;
    let parent = executable
        .parent()
        .context("installed binary has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    let path = file.into_temp_path();
    let output = String::from_utf8(crate::process::capture(
        &[path.to_string_lossy().into_owned(), "--version".into()],
        parent,
        1024,
        Duration::from_secs(10),
    )?)?;
    ensure!(
        output.trim() == format!("vardr {}", release.version),
        "signed binary version differs from the manifest"
    );
    self_replace::self_replace(&path)?;
    Ok(())
}

pub fn automatic(store: &Store) -> Result<bool> {
    let settings = store.settings()?;
    if !settings.updates.automatic {
        return Ok(false);
    }
    let path = store.root.join("updates.json");
    let mut state: UpdateState = if path.exists() {
        read_json(&path)?
    } else {
        UpdateState::default()
    };
    let now = Local::now();
    if now.time() < parse_time(&settings.updates.time)?
        || state
            .last_attempt
            .is_some_and(|t| t.with_timezone(&Local).date_naive() >= now.date_naive())
    {
        return Ok(false);
    }
    let _lock = store.lock("update")?;
    state.last_attempt = Some(Utc::now());
    write_json(&path, &state)?;
    let result = (|| {
        if let Some(release) = check()? {
            install(&release)?;
            state.installed_version = Some(release.version);
            Ok(true)
        } else {
            Ok(false)
        }
    })();
    state.last_error = result
        .as_ref()
        .err()
        .map(|e: &anyhow::Error| format!("{e:#}"));
    write_json(&path, &state)?;
    result
}
