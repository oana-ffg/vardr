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

struct Mac {
    _temp: tempfile::TempDir,
    home: PathBuf,
    fake_bin: PathBuf,
    log: PathBuf,
}
impl Mac {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let fake_bin = root.join("bin");
        fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        // launchd stops a booted-out agent with SIGTERM.
        let launchctl = fake_bin.join("launchctl");
        fs::write(
            &launchctl,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$VARDR_TEST_LAUNCHCTL_LOG\"\ncase \"$1\" in\n  print) [ -n \"$VARDR_TEST_SERVE_PID\" ] ;;\n  bootout) kill \"$VARDR_TEST_SERVE_PID\" ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&launchctl, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            log: root.join("launchctl.log"),
            _temp: temp,
            home,
            fake_bin,
        }
    }
    fn support(&self, name: &str) -> PathBuf {
        self.home.join("Library/Application Support").join(name)
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
            .env("PATH", format!("{}:/usr/bin:/bin", self.fake_bin.display()))
            .env("VARDR_TEST_LAUNCHCTL_LOG", &self.log);
        command
    }
    fn vardr(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn launchctl_calls(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
    /// Data, helper, LaunchAgent and receipt as written by Vardr 0.4.
    fn legacy_install(&self) -> Store {
        let legacy = self.support("dev.vardr.vardr");
        let job = self.home.join("job.json");
        fs::write(&job, serde_json::to_vec(&json!({"id":"kept","name":"Kept","purpose":"Survive the identity migration","cwd":self.home,"target":{"type":"command","argv":["/bin/echo","kept"]},"schedule":{"type":"manual"}})).unwrap()).unwrap();
        let put = self
            .command()
            .arg("--home")
            .arg(&legacy)
            .args(["put", job.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(put.status.success(), "{put:?}");
        let store = Store::open(legacy.clone()).unwrap();
        let mut settings = Settings::default();
        settings.updates.automatic = false;
        vardr::store::write_json(&legacy.join("config.json"), &settings).unwrap();
        let helper = legacy.join("bin/vardr");
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        fs::write(&helper, "#!/bin/sh\necho 'vardr 0.4.3'\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        let plist = self.agent("dev.vardr.runner");
        let content = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>dev.vardr.runner</string>\n<key>ProgramArguments</key><array><string>{}</string>\n<string>--home</string>\n<string>{}</string>\n<string>serve</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>EnvironmentVariables</key><dict><key>PATH</key><string>{LEGACY_PATH}</string></dict>\n</dict></plist>\n",
            helper.display(),
            legacy.display()
        );
        fs::write(&plist, &content).unwrap();
        vardr::store::write_json(
            &legacy.join("service-install.json"),
            &json!({"path": plist, "content": content}),
        )
        .unwrap();
        store
    }
}

fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn fresh_installations_use_the_firefungames_data_directory() {
    let mac = Mac::new();
    let init = mac.vardr(&["--json", "init"]);
    assert!(init.status.success(), "{init:?}");
    let home = serde_json::from_slice::<Value>(&init.stdout).unwrap()["home"].clone();
    assert_eq!(home, json!(mac.support("com.firefungames.vardr")));
    assert!(!mac.support("dev.vardr.vardr").exists());
    let nothing = mac.vardr(&["migrate"]);
    assert!(stderr(&nothing).contains("nothing to migrate"));
}

#[test]
fn migration_moves_data_and_replaces_the_running_scheduler() {
    let mac = Mac::new();
    let legacy = mac.legacy_install();
    let new_home = mac.support("com.firefungames.vardr");

    let blocked = mac.vardr(&["list"]);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("run `vardr migrate`"));
    assert!(
        !new_home.exists(),
        "no empty installation beside legacy data"
    );
    let other = mac.home.join("other");
    let second = mac
        .command()
        .arg("--home")
        .arg(&other)
        .args(["service", "install"])
        .output()
        .unwrap();
    assert!(stderr(&second).contains("run `vardr migrate`"));
    assert!(!mac.agent("com.firefungames.vardr.runner").exists());

    let mut scheduler = mac
        .command()
        .arg("--home")
        .arg(&legacy.root)
        .arg("serve")
        .spawn()
        .unwrap();
    wait_for(|| legacy.is_locked("service").unwrap());
    let migrated = mac
        .command()
        .env("VARDR_TEST_SERVE_PID", scheduler.id().to_string())
        .args(["--json", "migrate"])
        .output()
        .unwrap();
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert_eq!(
        serde_json::from_slice::<Value>(&migrated.stdout).unwrap(),
        json!({"home": new_home, "scheduler": true})
    );
    assert!(scheduler.wait().unwrap().success());

    assert!(!legacy.root.exists());
    assert!(!mac.agent("dev.vardr.runner").exists());
    let listed = mac.vardr(&["--json", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(
        serde_json::from_slice::<Value>(&listed.stdout).unwrap()["jobs"][0]["id"],
        "kept"
    );
    let plist = mac.agent("com.firefungames.vardr.runner");
    let content = fs::read_to_string(&plist).unwrap();
    assert!(content.contains("<string>com.firefungames.vardr.runner</string>"));
    assert!(content.contains(&format!("<string>{}</string>", new_home.display())));
    assert!(content.contains(&format!("<string>{LEGACY_PATH}</string>")));
    let receipt: Value = vardr::store::read_json(&new_home.join("service-install.json")).unwrap();
    assert_eq!(receipt["path"], json!(plist));
    let helper = Command::new(new_home.join("bin/vardr"))
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(helper.stdout).unwrap().trim(),
        concat!("vardr ", env!("CARGO_PKG_VERSION"))
    );
    let calls: Vec<String> = mac.launchctl_calls().lines().map(Into::into).collect();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(calls[0].starts_with("print gui/") && calls[0].ends_with("/dev.vardr.runner"));
    assert!(calls[1].starts_with("bootout gui/") && calls[1].ends_with("/dev.vardr.runner"));
    assert!(calls[2].starts_with("bootstrap gui/") && calls[2].ends_with(plist.to_str().unwrap()));

    let again = mac.vardr(&["migrate"]);
    assert!(stderr(&again).contains("nothing to migrate"));
    fs::create_dir_all(&legacy.root).unwrap();
    let ambiguous = mac.vardr(&["list"]);
    assert!(stderr(&ambiguous).contains("keep only one"));
}

#[test]
fn migration_refuses_running_work_and_changes_nothing() {
    let mac = Mac::new();
    let legacy = mac.legacy_install();
    let _scheduler = legacy.lock("service").unwrap();
    vardr::store::write_json(
        &legacy.root.join("service.json"),
        &json!({"updated_at": null, "pid": 1, "running": ["kept"], "errors": {}}),
    )
    .unwrap();
    let refused = mac.vardr(&["migrate"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("scheduled work is running (kept)"));
    assert!(legacy.root.join("jobs/kept.json").exists());
    assert!(mac.agent("dev.vardr.runner").exists());
    assert!(!mac.support("com.firefungames.vardr").exists());
    assert_eq!(mac.launchctl_calls(), "");
}

#[test]
fn migration_without_a_scheduler_only_moves_data() {
    let mac = Mac::new();
    let legacy = mac.legacy_install();
    fs::remove_file(mac.agent("dev.vardr.runner")).unwrap();
    fs::remove_file(legacy.root.join("service-install.json")).unwrap();
    let migrated = mac.vardr(&["--json", "migrate"]);
    assert!(migrated.status.success(), "{}", stderr(&migrated));
    assert_eq!(
        serde_json::from_slice::<Value>(&migrated.stdout).unwrap()["scheduler"],
        false
    );
    assert!(
        mac.support("com.firefungames.vardr")
            .join("jobs/kept.json")
            .exists()
    );
    assert!(!Path::new(&mac.agent("com.firefungames.vardr.runner")).exists());
    assert_eq!(mac.launchctl_calls(), "");
}
