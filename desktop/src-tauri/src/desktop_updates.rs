use anyhow::Result;
use chrono::{Local, Utc};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;
use vardr::{
    model::parse_time,
    store::{read_json, write_json},
    updates::UpdateState,
};

use crate::Desktop;

pub fn check(app: AppHandle, manual: bool) {
    let state = app.state::<Desktop>();
    if state.updating.swap(true, Ordering::Relaxed) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let result = perform(&app, manual).await;
        if let Err(error) = result
            && manual
        {
            app.dialog()
                .message(format!("Could not update Vardr: {error:#}"))
                .title("Vardr update")
                .show(|_| {});
        }
        app.state::<Desktop>()
            .updating
            .store(false, Ordering::Relaxed);
    });
}

async fn perform(app: &AppHandle, manual: bool) -> Result<()> {
    let desktop = app.state::<Desktop>();
    let settings = desktop.store.settings()?;
    if !manual && !settings.updates.automatic {
        return Ok(());
    }
    let path = desktop.store.root.join("desktop-updates.json");
    let mut record: UpdateState = if path.exists() {
        read_json(&path)?
    } else {
        UpdateState::default()
    };
    let now = Local::now();
    if !manual
        && (now.time() < parse_time(&settings.updates.time)?
            || record
                .last_attempt
                .is_some_and(|time| time.with_timezone(&Local).date_naive() >= now.date_naive()))
    {
        return Ok(());
    }
    record.last_attempt = Some(Utc::now());
    write_json(&path, &record)?;
    let result: Result<()> = async {
        let updater = app.updater_builder().timeout(std::time::Duration::from_secs(30)).build()?;
        match updater.check().await? {
            Some(update) => {
                if record.installed_version.as_deref() != Some(update.version.as_str()) {
                    // Install the app bundle for its next launch. Do not restart the UI or scheduler.
                    desktop.installing.store(true, Ordering::Relaxed);
                    let installation = update.download_and_install(|_, _| {}, || {}).await;
                    desktop.installing.store(false, Ordering::Relaxed);
                    installation?;
                    record.installed_version = Some(update.version.clone());
                }
                if manual {
                    app.dialog().message(format!("Vardr {} is installed. Quit and reopen Vardr to use it. Your scheduler continues independently.", update.version)).title("Vardr updated").show(|_| {});
                }
            }
            None if manual => app.dialog().message("You have the latest version of Vardr.").title("Vardr update").show(|_| {}),
            None => {}
        }
        Ok(())
    }.await;
    record.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
    write_json(&path, &record)?;
    result
}
