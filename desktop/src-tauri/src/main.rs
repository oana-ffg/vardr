mod desktop_updates;
#[cfg(target_os = "macos")]
mod macos_update;

use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};
use tauri::{
    AppHandle, Manager, RunEvent, WindowEvent,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use vardr::{
    migration, process, service,
    store::{Home, Store, resolve_home},
    ui::Dashboard,
};

#[derive(PartialEq)]
enum Lifecycle {
    Ready,
    Installing,
    Closing,
}

struct Desktop {
    store: Store,
    activity: Arc<AtomicUsize>,
    server: Mutex<Option<JoinHandle<()>>>,
    updating: AtomicBool,
    lifecycle: Mutex<Lifecycle>,
}

/// Set while a migration runs; quitting then could stop it between steps.
static MIGRATING: AtomicBool = AtomicBool::new(false);

fn explicit_home() -> Option<PathBuf> {
    std::env::var_os("VARDR_HOME").map(PathBuf::from)
}

/// `None` while data from before 0.5.0 awaits migration.
fn data_directory() -> Result<Option<PathBuf>> {
    let Home::Ready(root) = resolve_home(explicit_home())? else {
        return Ok(None);
    };
    std::fs::create_dir_all(&root)?;
    Ok(Some(root.canonicalize()?))
}

/// The app opens no window until the user has moved or declined to move the data.
fn offer_migration(app: &AppHandle) {
    let app = app.clone();
    app.clone()
        .dialog()
        .message("This version of Vardr moves its data and scheduler to a new signed identity. Your automations, history, schedules and settings are kept. If a scheduled job is running, Vardr asks you to try again once it finishes.\n\nmacOS will ask again for permissions such as Full Disk Access and Automation.")
        .title("Move Vardr data")
        .buttons(MessageDialogButtons::OkCancelCustom("Move".into(), "Quit".into()))
        .show(move |confirmed| {
            if !confirmed {
                app.exit(0);
                return;
            }
            MIGRATING.store(true, Ordering::Relaxed);
            tauri::async_runtime::spawn_blocking(move || {
                let result = helper().and_then(|path| migration::migrate(explicit_home(), &path));
                MIGRATING.store(false, Ordering::Relaxed);
                match result {
                    Ok(migrated) if !migrated.reported.is_empty() => {
                        let handle = app.clone();
                        let jobs: Vec<_> = migrated.reported.iter().map(|failure| failure.job.as_str()).collect();
                        app.dialog()
                            .message(format!("Vardr's data moved. Before stopping, the scheduler had reported errors for {}; their last scheduled runs may not have happened. Details are kept in {}.", jobs.join(", "), migrated.home.join(migration::REPORTED).display()))
                            .title("Move Vardr data")
                            .show(move |_| handle.restart());
                    }
                    Ok(_) => app.restart(),
                    Err(error) => {
                        let handle = app.clone();
                        app.dialog()
                            .message(format!("Could not move Vardr's data: {error:#}"))
                            .title("Move Vardr data")
                            .show(move |_| handle.exit(0));
                    }
                }
            });
        });
}

fn helper() -> Result<PathBuf> {
    let directory = std::env::current_exe()?
        .parent()
        .context("app has no executable directory")?
        .to_owned();
    let path = directory.join("vardr");
    anyhow::ensure!(
        path.is_file(),
        "Bundled Vardr CLI is missing. Reinstall the application."
    );
    Ok(path)
}

fn menus(app: &AppHandle) -> Result<()> {
    let updates = MenuItem::with_id(app, "updates", "Check for Updates…", true, None::<&str>)?;
    let app_menu = Submenu::with_items(
        app,
        "Vardr",
        true,
        &[
            &PredefinedMenuItem::about(app, Some("About Vardr"), None)?,
            &updates,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::show_all(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, None)?,
        ],
    )?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let start = MenuItem::with_id(
        app,
        "start",
        "Start Scheduler at Login…",
        true,
        None::<&str>,
    )?;
    let stop = MenuItem::with_id(
        app,
        "stop",
        "Stop and Remove Scheduler…",
        true,
        None::<&str>,
    )?;
    let data = MenuItem::with_id(app, "data", "Open Data Folder", true, None::<&str>)?;
    let cli = MenuItem::with_id(app, "cli", "Copy CLI Command", true, None::<&str>)?;
    let scheduler = Submenu::with_items(
        app,
        "Scheduler",
        true,
        &[
            &start,
            &stop,
            &PredefinedMenuItem::separator(app)?,
            &data,
            &cli,
        ],
    )?;
    let window = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
        ],
    )?;
    app.set_menu(Menu::with_items(
        app,
        &[&app_menu, &edit, &scheduler, &window],
    )?)?;
    Ok(())
}

fn menu_action(app: &AppHandle, id: &str) {
    if id == "updates" {
        desktop_updates::check(app.clone(), true);
        return;
    }
    let desktop = app.state::<Desktop>();
    if id == "data" {
        if let Err(error) = std::process::Command::new("/usr/bin/open")
            .arg(&desktop.store.root)
            .spawn()
        {
            app.dialog()
                .message(format!("Could not open the data folder: {error}"))
                .show(|_| {});
        }
        return;
    }
    if id == "cli" {
        let result = (|| -> Result<()> {
            use std::io::Write;
            let path = helper()?;
            // Shell single-quoting keeps spaces and user-controlled path characters literal.
            let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
            let command = format!(
                "{} --home {}",
                quote(&path.to_string_lossy()),
                quote(&desktop.store.root.to_string_lossy())
            );
            let mut child = std::process::Command::new("/usr/bin/pbcopy")
                .stdin(std::process::Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .context("clipboard unavailable")?
                .write_all(command.as_bytes())?;
            anyhow::ensure!(child.wait()?.success(), "clipboard command failed");
            Ok(())
        })();
        if let Err(error) = result {
            app.dialog()
                .message(format!("Could not copy the CLI command: {error:#}"))
                .show(|_| {});
        }
        return;
    }
    if id != "start" && id != "stop" {
        return;
    }
    let installing = id == "start";
    let question = if installing {
        "Start Vardr’s scheduler now and at login? Enabled jobs will run even when this app is closed."
    } else {
        "Stop Vardr’s scheduler and remove its login service? Active scheduled jobs will be interrupted. Your definitions and history will be kept."
    };
    let app = app.clone();
    app.clone()
        .dialog()
        .message(question)
        .title("Vardr scheduler")
        .buttons(MessageDialogButtons::OkCancel)
        .show(move |confirmed| {
            if !confirmed {
                return;
            }
            tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<Desktop>();
                let result = if installing {
                    helper().and_then(|path| service::install_from(&state.store, &path))
                } else {
                    service::remove(&state.store)
                };
                if let Err(error) = result {
                    app.dialog()
                        .message(format!("Could not change the scheduler: {error:#}"))
                        .title("Vardr scheduler")
                        .show(|_| {});
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval("document.getElementById('refresh').click()");
                }
            });
        });
}

fn main() {
    // LaunchServices supplies a smaller PATH than a terminal. Include installed agent CLIs.
    let mut paths: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if let Some(base) = directories::BaseDirs::new() {
        for path in [
            base.home_dir().join(".local/bin"),
            base.home_dir().join(".cargo/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ] {
            if path.is_dir() && !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    if let Ok(path) = std::env::join_paths(paths) {
        // Set only before Tauri or any Vardr threads are started.
        unsafe {
            std::env::set_var("PATH", path);
        }
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let Some(root) = data_directory()? else {
                offer_migration(app.handle());
                return Ok(());
            };
            let store = Store::open(root)?;
            let dashboard = Dashboard::bind(0)?;
            let url: tauri::Url = dashboard.entry_url().parse()?;
            let origin = url.origin();
            let activity = dashboard.activity();
            let local = store.clone();
            let handle = app.handle().clone();
            let server = std::thread::spawn(move || {
                if let Err(error) = dashboard.run(&local) {
                    handle
                        .dialog()
                        .message(format!("Vardr’s dashboard stopped: {error:#}"))
                        .title("Vardr")
                        .show(|_| {});
                }
            });
            app.manage(Desktop {
                store,
                activity,
                server: Mutex::new(Some(server)),
                updating: AtomicBool::new(false),
                lifecycle: Mutex::new(Lifecycle::Ready),
            });
            menus(app.handle())?;
            // Local HTTP remains authenticated; the page receives no Tauri IPC capabilities.
            tauri::WebviewWindowBuilder::from_config(app, &app.config().app.windows[0])?
                .on_navigation(move |target| {
                    if target.origin() == origin
                        || target.scheme() == "tauri"
                        || target.host_str() == Some("tauri.localhost")
                    {
                        return true;
                    }
                    if target.scheme() == "t3-thread" && target.host_str() == Some("v1") {
                        let _ = std::process::Command::new("/usr/bin/open")
                            .arg(target.as_str())
                            .spawn();
                    }
                    false
                })
                .build()?
                .navigate(url)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                while !process::STOP.load(Ordering::Relaxed) {
                    desktop_updates::check(handle.clone(), false);
                    for _ in 0..60 {
                        if process::STOP.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            });
            Ok(())
        })
        .on_menu_event(|app, event| menu_action(app, event.id().as_ref()))
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("Could not open Vardr");
    app.run(|app, event| match event {
        RunEvent::ExitRequested { api, .. } => {
            let Some(desktop) = app.try_state::<Desktop>() else {
                if MIGRATING.load(Ordering::Relaxed) {
                    api.prevent_exit();
                    app.dialog().message("Vardr is moving its data. It quits or restarts when finished.").title("Move Vardr data").show(|_| {});
                }
                return;
            };
            let mut lifecycle = desktop.lifecycle.lock().unwrap();
            if *lifecycle == Lifecycle::Installing {
                api.prevent_exit();
                app.dialog().message("An app update is being installed. Please quit once it finishes.").title("Vardr update").show(|_| {});
            } else if desktop.activity.load(Ordering::Relaxed) > 0 && !process::STOP.load(Ordering::Relaxed) {
                api.prevent_exit();
                let handle = app.clone();
                app.dialog().message("A manually started job is still running. Quit and interrupt it? Scheduled jobs run independently and will continue.").title("Quit Vardr?").buttons(MessageDialogButtons::OkCancel).show(move |confirmed| {
                    if confirmed { process::STOP.store(true, Ordering::Relaxed); handle.exit(0); }
                });
            } else {
                *lifecycle = Lifecycle::Closing;
            }
        }
        RunEvent::Exit => {
            process::STOP.store(true, Ordering::Relaxed);
            if let Some(server) = app.try_state::<Desktop>().and_then(|desktop| desktop.server.lock().unwrap().take()) { let _ = server.join(); }
        }
        _ => {}
    });
}
