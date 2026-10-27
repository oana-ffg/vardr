#![cfg(target_os = "macos")]
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
use vardr::{model::Settings, store::Store};

const LEGACY_PATH: &str = "/legacy tools/bin:/usr/bin";
const LEGACY: &str = "dev.vardr.runner";
const CURRENT: &str = "com.firefungames.vardr.runner";

struct Mac {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}
impl Mac {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        // Records loaded labels like launchd; bootout stops the scheduler with SIGTERM.
        let launchctl = root.join("bin/launchctl");
        fs::write(
            &launchctl,
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$VARDR_TEST_ROOT/launchctl.log"
loaded="$VARDR_TEST_ROOT/loaded"
touch "$loaded"
case "$1" in
  print) grep -qx "${2##*/}" "$loaded" ;;
  bootout)
    grep -vx "${2##*/}" "$loaded" > "$loaded.next"; mv "$loaded.next" "$loaded"
    if [ -n "$VARDR_TEST_SERVE_PID" ]; then kill "$VARDR_TEST_SERVE_PID"; fi ;;
  bootstrap)
    [ -z "$VARDR_TEST_BOOTSTRAP_FAILS" ] || exit 1
    basename "$3" .plist >> "$loaded" ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&launchctl, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temp: temp,
            root,
            home,
        }
    }
    fn support(&self, name: &str) -> PathBuf {
        self.home.join("Library/Application Support").join(name)
    }
    fn legacy(&self) -> PathBuf {
        self.support("dev.vardr.vardr")
    }
    fn current(&self) -> PathBuf {
        self.support("com.firefungames.vardr")
    }
    fn agent(&self, label: &str) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"))
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vardr"));
        command
            .env("HOME", &self.home)
            .env_remove("VARDR_HOME")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("VARDR_TEST_ROOT", &self.root);
        command
    }
    fn vardr(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn with_home(&self, home: &Path, args: &[&str]) -> Output {
        self.command()
            .arg("--home")
            .arg(home)
            .args(args)
            .output()
            .unwrap()
    }
    fn launchctl_calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("launchctl.log"))
            .unwrap_or_default()
            .lines()
            .map(|call| call.split_whitespace().next().unwrap().to_owned())
            .collect()
    }
    /// Data, helper, LaunchAgent and receipt as written by Vardr 0.4, at `data`.
    fn legacy_install(&self, data: &Path) -> Store {
        self.legacy_install_with(data, json!({"type": "manual"}))
    }
    fn legacy_install_with(&self, data: &Path, schedule: Value) -> Store {
        let staging = self.root.join("staging");
        let job = self.root.join("job.json");
        fs::write(&job, serde_json::to_vec(&json!({"id":"kept","name":"Kept","purpose":"Survive the identity migration","cwd":self.root,"target":{"type":"command","argv":["/bin/echo","kept"]},"schedule":schedule})).unwrap()).unwrap();
        let put = self.with_home(&staging, &["put", job.to_str().unwrap()]);
        assert!(put.status.success(), "{put:?}");
        fs::create_dir_all(data.parent().unwrap()).unwrap();
        fs::rename(&staging, data).unwrap();
        let store = Store::open(data.to_owned()).unwrap();
        let mut settings = Settings::default();
        settings.updates.automatic = false;
        vardr::store::write_json(&data.join("config.json"), &settings).unwrap();
        let helper = data.join("bin/vardr");
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        fs::write(&helper, "#!/bin/sh\necho 'vardr 0.4.3'\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let plist = self.agent(LEGACY);
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LEGACY}</string>\n<key>ProgramArguments</key><array><string>{}</string>\n<string>--home</string>\n<string>{}</string>\n<string>serve</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{LEGACY_PATH}</string></dict>\n</dict></plist>\n",
            helper.display(),
            data.display()
        );
        fs::write(&plist, &content).unwrap();
        vardr::store::write_json(
            &data.join("service-install.json"),
            &json!({"path": plist, "content": content}),
        )
        .unwrap();
        fs::write(self.root.join("loaded"), format!("{LEGACY}\n")).unwrap();
        store
    }
    fn assert_replaced(&self, data: &Path) {
        let plist = self.agent(CURRENT);
        let content = fs::read_to_string(&plist).unwrap();
        assert!(content.contains(&format!("<string>{CURRENT}</string>")));
        assert!(content.contains(&format!("<string>{}</string>", data.display())));
        assert!(content.contains(&format!("<string>{LEGACY_PATH}</string>")));
        assert!(!self.agent(LEGACY).exists());
        let receipt: Value = vardr::store::read_json(&data.join("service-install.json")).unwrap();
        assert_eq!(receipt["path"], json!(plist));
        assert!(!data.join("migration.json").exists());
        let helper = Command::new(data.join("bin/vardr"))
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(helper.stdout).unwrap().trim(),
            concat!("vardr ", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            fs::read_to_string(self.root.join("loaded")).unwrap(),
            format!("{CURRENT}\n")
        );
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn listed(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["jobs"][0]["id"].clone()
}

#[test]
fn fresh_installations_use_the_firefungames_data_directory() {
    let mac = Mac::new();
    let init = mac.vardr(&["--json", "init"]);
    assert!(init.status.success(), "{init:?}");
    let home = serde_json::from_slice::<Value>(&init.stdout).unwrap()["home"].clone();
    assert_eq!(home, json!(mac.current()));
    assert!(!mac.legacy().exists());
    assert!(stderr(&mac.vardr(&["migrate"])).contains("nothing to migrate"));
}

#[test]
fn migration_moves_data_and_replaces_the_running_scheduler() {
    let mac = Mac::new();
    let legacy = mac.legacy_install(&mac.legacy());

    assert!(stderr(&mac.vardr(&["list"])).contains("run `vardr migrate`"));
    assert!(
        !mac.current().exists(),
        "no empty installation beside legacy data"
    );
    let spelled = mac.support("./dev.vardr.vardr");
    assert!(stderr(&mac.with_home(&spelled, &["list"])).contains("from before 0.5.0"));
    let other = mac.with_home(&mac.root.join("other"), &["service", "install"]);
    assert!(stderr(&other).contains("run `vardr migrate`"));
    assert!(!mac.agent(CURRENT).exists());

    // A real scheduler on the old data stands in for the pre-0.5.0 helper. Its own HOME
    // differs, so this build does not recognize the path as the old default.
    let mut scheduler = Command::new(env!("CARGO_BIN_EXE_vardr"))
        .env("HOME", mac.root.join("unrelated-home"))
        .arg("--home")
        .arg(&legacy.root)
        .arg("serve")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !legacy.is_locked("service").unwrap() {
        assert!(Instant::now() < deadline, "scheduler did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let migrated = mac
        .command()
        .env("VARDR_TEST_SERVE_PID", scheduler.id().to_string())
        .args(["--json", "migrate"])
        .output()
        .unwrap();
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert_eq!(
        serde_json::from_slice::<Value>(&migrated.stdout).unwrap(),
        json!({"home": mac.current(), "scheduler": true, "reported": []})
    );
    assert!(scheduler.wait().unwrap().success());

    assert_eq!(listed(&mac.vardr(&["--json", "list"])), "kept");
    mac.assert_replaced(&mac.current());
    assert_eq!(
        mac.launchctl_calls(),
        // Stop the old agent; on finishing, confirm it was not reloaded, then start the new one.
        ["print", "bootout", "print", "print", "bootstrap"]
    );
    // Pre-0.5.0 processes still holding the old path cannot recreate it.
    assert!(
        fs::read_to_string(mac.legacy())
            .unwrap()
            .contains("com.firefungames.vardr")
    );
    assert!(fs::create_dir_all(mac.legacy().join("jobs")).is_err());
    assert!(stderr(&mac.with_home(&mac.legacy(), &["list"])).contains("from before 0.5.0"));
    assert!(stderr(&mac.vardr(&["migrate"])).contains("nothing to migrate"));

    fs::remove_file(mac.legacy()).unwrap();
    fs::create_dir(mac.legacy()).unwrap();
    assert!(stderr(&mac.vardr(&["list"])).contains("keep only one"));
}

#[test]
fn migration_refuses_running_work_and_changes_nothing() {
    let mac = Mac::new();
    let legacy = mac.legacy_install(&mac.legacy());
    let _running = legacy.lock("run-kept").unwrap();
    let refused = mac.vardr(&["migrate"]);
    assert!(
        stderr(&refused).contains("kept is running"),
        "{}",
        stderr(&refused)
    );
    assert!(legacy.root.join("jobs/kept.json").exists());
    assert!(!legacy.root.join("migration.json").exists());
    assert!(mac.agent(LEGACY).exists());
    assert!(!mac.current().exists());
    assert!(mac.launchctl_calls().is_empty());
}

#[test]
fn migration_without_a_scheduler_only_moves_data() {
    let mac = Mac::new();
    let legacy = mac.legacy_install(&mac.legacy());
    fs::remove_file(mac.agent(LEGACY)).unwrap();
    fs::remove_file(legacy.root.join("service-install.json")).unwrap();
    let migrated = mac.vardr(&["--json", "migrate"]);
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert_eq!(
        serde_json::from_slice::<Value>(&migrated.stdout).unwrap()["scheduler"],
        false
    );
    assert_eq!(listed(&mac.vardr(&["--json", "list"])), "kept");
    assert!(!mac.current().join("migration.json").exists());
    assert!(!mac.agent(CURRENT).exists());
    assert!(mac.launchctl_calls().is_empty());
}

#[test]
fn failed_scheduler_replacement_is_finished_by_running_migrate_again() {
    let mac = Mac::new();
    mac.legacy_install(&mac.legacy());
    let failed = mac
        .command()
        .env("VARDR_TEST_BOOTSTRAP_FAILS", "1")
        .arg("migrate")
        .output()
        .unwrap();
    assert!(
        stderr(&failed).contains("run `vardr migrate` again"),
        "{}",
        stderr(&failed)
    );
    assert!(mac.current().join("migration.json").exists());
    assert!(
        mac.agent(LEGACY).exists(),
        "kept until its replacement runs"
    );
    assert!(stderr(&mac.vardr(&["list"])).contains("unfinished"));

    let finished = mac.vardr(&["migrate"]);
    assert!(finished.status.success(), "{}", stderr(&finished));
    mac.assert_replaced(&mac.current());
    assert_eq!(listed(&mac.vardr(&["--json", "list"])), "kept");
}

#[test]
fn custom_homes_keep_their_data_and_replace_the_scheduler() {
    let mac = Mac::new();
    let custom = mac.root.join("custom");
    mac.legacy_install(&custom);
    assert!(
        stderr(&mac.with_home(&custom, &["service", "install"])).contains("run `vardr migrate`")
    );
    assert!(
        stderr(&mac.with_home(&custom, &["service", "remove"]))
            .contains("`vardr migrate` replaces")
    );
    let migrated = mac.with_home(&custom, &["--json", "migrate"]);
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert_eq!(
        serde_json::from_slice::<Value>(&migrated.stdout).unwrap(),
        json!({"home": custom, "scheduler": true, "reported": []})
    );
    mac.assert_replaced(&custom);
    assert_eq!(listed(&mac.with_home(&custom, &["--json", "list"])), "kept");
    assert!(!mac.current().exists() && !mac.legacy().exists());
}

#[test]
fn migration_refuses_while_a_job_is_due_before_the_scheduler_stops() {
    let mac = Mac::new();
    let legacy = mac.legacy_install_with(&mac.legacy(), json!({"type": "interval", "seconds": 30}));
    let refused = mac.vardr(&["migrate"]);
    assert!(
        stderr(&refused).contains("kept is due at"),
        "{}",
        stderr(&refused)
    );
    assert!(legacy.root.join("jobs/kept.json").exists());
    assert!(mac.agent(LEGACY).exists());
    assert!(mac.launchctl_calls().is_empty());
}

#[test]
fn migration_interrupted_before_its_swap_resumes() {
    let mac = Mac::new();
    let legacy = mac.legacy_install(&mac.legacy());
    fs::write(
        legacy.root.join("migration.json"),
        r#"{"scheduler":null,"reported":[]}"#,
    )
    .unwrap();
    fs::write(
        mac.current(),
        format!(
            "Vardr 0.5.0 moved this data directory to {}\n",
            mac.current().display()
        ),
    )
    .unwrap();
    assert!(stderr(&mac.vardr(&["list"])).contains("run `vardr migrate`"));
    let migrated = mac.vardr(&["--json", "migrate"]);
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    mac.assert_replaced(&mac.current());
    assert_eq!(listed(&mac.vardr(&["--json", "list"])), "kept");
    assert!(fs::symlink_metadata(mac.legacy()).unwrap().is_file());
}

#[test]
fn job_errors_reported_by_the_stopped_scheduler_are_kept() {
    let mac = Mac::new();
    let legacy = mac.legacy_install(&mac.legacy());
    // A reserved run whose worker failed before recording it, as a 0.4.x scheduler
    // reports on exit, beside a scheduler-level error.
    vardr::store::write_json(
        &legacy.root.join("service.json"),
        &json!({"updated_at": null, "pid": 0, "running": [], "errors": {"kept": "timed out acquiring registry", "updater": "offline"}}),
    )
    .unwrap();
    let migrated = mac.vardr(&["--json", "migrate"]);
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert!(stderr(&migrated).contains("may not have happened"));
    let expected = json!([{"job": "kept", "error": "timed out acquiring registry"}]);
    assert_eq!(
        serde_json::from_slice::<Value>(&migrated.stdout).unwrap()["reported"],
        expected
    );
    let kept: Value =
        vardr::store::read_json(&mac.current().join("migration-failures.json")).unwrap();
    assert_eq!(kept, expected);
    let history = mac.vardr(&["--json", "history", "kept"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&history.stdout).unwrap(),
        json!([])
    );
}

#[test]
fn installation_interrupted_between_plist_and_receipt_resumes() {
    let mac = Mac::new();
    mac.legacy_install(&mac.legacy());
    let failed = mac
        .command()
        .env("VARDR_TEST_BOOTSTRAP_FAILS", "1")
        .arg("migrate")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    // Reconstruct the moment after the new plist was written but before its receipt.
    let receipt = mac.current().join("service-install.json");
    let mut recorded: Value = vardr::store::read_json(&receipt).unwrap();
    recorded["path"] = json!(mac.agent(LEGACY));
    recorded["content"] = json!(fs::read_to_string(mac.agent(LEGACY)).unwrap());
    vardr::store::write_json(&receipt, &recorded).unwrap();
    let finished = mac.vardr(&["migrate"]);
    assert!(finished.status.success(), "{}", stderr(&finished));
    mac.assert_replaced(&mac.current());
}

#[test]
fn resume_stops_an_old_agent_restarted_before_migration_finished() {
    let mac = Mac::new();
    let custom = mac.root.join("custom");
    let store = mac.legacy_install(&custom);
    let failed = mac
        .command()
        .env("VARDR_TEST_BOOTSTRAP_FAILS", "1")
        .arg("--home")
        .arg(&custom)
        .arg("migrate")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    // A login reloads the old plist, whose scheduler runs on this data and reports.
    fs::write(mac.root.join("loaded"), format!("{LEGACY}\n")).unwrap();
    vardr::store::write_json(
        &custom.join("service.json"),
        &json!({"updated_at": null, "pid": 0, "running": [], "errors": {"kept": "late failure"}}),
    )
    .unwrap();
    let mut scheduler = Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import fcntl, sys, time\nf = open(sys.argv[1], 'a')\nfcntl.flock(f, fcntl.LOCK_EX)\nprint(flush=True)\ntime.sleep(60)",
        ])
        .arg(custom.join("locks/service.lock"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Read::read(&mut scheduler.stdout.take().unwrap(), &mut [0]).unwrap();
    assert!(store.is_locked("service").unwrap());

    let finished = mac
        .command()
        .env("VARDR_TEST_SERVE_PID", scheduler.id().to_string())
        .arg("--home")
        .arg(&custom)
        .args(["--json", "migrate"])
        .output()
        .unwrap();
    assert!(finished.status.success(), "{}", stderr(&finished));
    scheduler.wait().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&finished.stdout).unwrap()["reported"],
        json!([{"job": "kept", "error": "late failure"}])
    );
    mac.assert_replaced(&custom);
}
