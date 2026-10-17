//! Read-only snapshots. Never bootstrap, enable, unload, or execute discovered jobs.
use crate::process;
use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value as Plist};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Inventory {
    pub machine: String,
    pub observed_at: String,
    pub local_time: String,
    pub jobs: Vec<DiscoveredJob>,
    pub errors: BTreeMap<String, String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoveredJob {
    pub label: String,
    pub path: String,
    pub domain: String,
    pub executable: Option<String>,
    pub working_directory: Option<String>,
    pub triggers: BTreeMap<String, Value>,
    pub schedule: Vec<String>,
    pub disabled: Option<bool>,
    pub loaded: Option<bool>,
    pub state: Option<String>,
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    pub error: Option<String>,
}
fn text(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(Plist::as_string).map(str::to_owned)
}
fn definition(path: &Path, domain: &str, data: &[u8]) -> Result<DiscoveredJob> {
    let value = Plist::from_reader(std::io::Cursor::new(data))?;
    let dict = value
        .as_dictionary()
        .context("plist must be a dictionary")?;
    let label = text(dict, "Label").context("missing Label")?;
    ensure!(
        !label.is_empty() && !label.contains(['/', '\n', '\r']),
        "invalid Label"
    );
    let executable = text(dict, "Program").or_else(|| {
        dict.get("ProgramArguments")?
            .as_array()?
            .first()?
            .as_string()
            .map(str::to_owned)
    });
    let mut triggers = BTreeMap::new();
    for key in [
        "StartCalendarInterval",
        "StartInterval",
        "RunAtLoad",
        "KeepAlive",
        "WatchPaths",
        "QueueDirectories",
        "StartOnMount",
        "Sockets",
        "MachServices",
        "LaunchEvents",
    ] {
        if let Some(value) = dict.get(key) {
            // Expose trigger definitions, never EnvironmentVariables or command arguments.
            triggers.insert(key.to_owned(), serde_json::to_value(value)?);
        }
    }
    let schedule = describe_triggers(&triggers);
    Ok(DiscoveredJob {
        label,
        path: path.display().to_string(),
        domain: domain.into(),
        executable,
        working_directory: text(dict, "WorkingDirectory"),
        triggers,
        schedule,
        disabled: dict.get("Disabled").and_then(Plist::as_boolean),
        loaded: None,
        state: None,
        pid: None,
        last_exit_code: None,
        error: None,
    })
}
fn calendar(value: &Value) -> String {
    let Some(fields) = value.as_object() else {
        return format!("Calendar: {value}");
    };
    let number = |key| fields.get(key).and_then(Value::as_u64);
    let hour = number("Hour");
    let minute = number("Minute");
    let time = match (hour, minute) {
        (Some(h), Some(m)) => format!("{h:02}:{m:02}"),
        (None, Some(m)) => format!("hourly at :{m:02}"),
        (Some(h), None) => format!("every minute during hour {h:02}"),
        _ => "every minute".into(),
    };
    let mut parts = vec![if hour.is_some() && minute.is_some() && fields.len() == 2 {
        format!("Daily at {time}")
    } else {
        time
    }];
    for key in ["Weekday", "Day", "Month"] {
        if let Some(value) = fields.get(key) {
            parts.push(format!("{key} {value}"));
        }
    }
    if fields
        .keys()
        .any(|k| !["Hour", "Minute", "Weekday", "Day", "Month"].contains(&k.as_str()))
    {
        return format!("Calendar: {value}");
    }
    parts.join(" · ")
}
fn describe_triggers(triggers: &BTreeMap<String, Value>) -> Vec<String> {
    let mut result = Vec::new();
    if let Some(value) = triggers.get("StartCalendarInterval") {
        match value.as_array() {
            Some(values) => result.extend(values.iter().map(calendar)),
            None => result.push(calendar(value)),
        }
    }
    if let Some(seconds) = triggers.get("StartInterval").and_then(Value::as_u64) {
        result.push(format!("Every {seconds} seconds"));
    }
    if triggers.get("RunAtLoad") == Some(&json!(true)) {
        result.push("When loaded".into());
    }
    if let Some(value) = triggers.get("KeepAlive") {
        if value == &json!(true) {
            result.push("Keep running".into());
        } else if value.is_object() {
            result.push(format!("Keep alive when: {value}"));
        }
    }
    for key in [
        "WatchPaths",
        "QueueDirectories",
        "StartOnMount",
        "Sockets",
        "MachServices",
        "LaunchEvents",
    ] {
        if let Some(value) = triggers.get(key) {
            result.push(format!("{key}: {value}"));
        }
    }
    if result.is_empty() {
        result.push("On demand / no timed trigger".into());
    }
    result
}
fn launchctl(args: &[&str]) -> Result<(process::CaptureResult, String)> {
    let argv: Vec<_> = std::iter::once("/bin/launchctl")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    let mut result =
        process::capture_result(&argv, Path::new("/"), 512 * 1024, Duration::from_secs(2))?;
    let output = String::from_utf8(std::mem::take(&mut result.stdout))?;
    Ok((result, output))
}
fn disabled_overrides(output: &str) -> Result<BTreeMap<String, bool>> {
    ensure!(
        output.trim_start().starts_with("disabled services = {")
            && output.trim_end().ends_with('}'),
        "unrecognized disabled-services output"
    );
    let mut map = BTreeMap::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .skip(1)
    {
        if line == "}" {
            continue;
        }
        let (label, state) = line
            .rsplit_once(" => ")
            .context("unrecognized disabled-service entry")?;
        let label = label
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .context("unrecognized service label")?;
        let disabled = match state {
            "disabled" => true,
            "enabled" => false,
            _ => anyhow::bail!("unrecognized disabled-service state"),
        };
        map.insert(label.to_owned(), disabled);
    }
    Ok(map)
}
fn runtime(job: &mut DiscoveredJob, output: &str) -> Result<()> {
    // launchctl print is diagnostic text, not a stable API. Parse only top-level
    // fields; reject unknown formats instead of inventing a healthy status.
    ensure!(
        output
            .lines()
            .next()
            .is_some_and(|line| line.starts_with(&format!("{}/{} = {{", job.domain, job.label))),
        "unrecognized launchctl print header"
    );
    for line in output
        .lines()
        .filter_map(|line| line.strip_prefix('\t'))
        .filter(|line| !line.starts_with(['\t', ' ']))
    {
        if let Some(state) = line.strip_prefix("state = ") {
            job.state = Some(state.to_owned());
        }
        if let Some(pid) = line.strip_prefix("pid = ") {
            job.pid = Some(pid.parse()?);
        }
        if let Some(code) = line.strip_prefix("last exit code = ")
            && code != "(never exited)"
        {
            job.last_exit_code = Some(code.split(':').next().unwrap().trim().parse()?);
        }
    }
    ensure!(job.state.is_some(), "launchctl did not report a state");
    job.loaded = Some(true);
    Ok(())
}
pub fn local() -> Result<Inventory> {
    ensure!(cfg!(target_os = "macos"), "launchd discovery is macOS-only");
    let home = directories::BaseDirs::new().context("home directory required")?;
    let user = format!("gui/{}", unsafe { libc::getuid() });
    let mut inventory = Inventory {
        machine: String::from_utf8(process::capture(
            &["/bin/hostname".into()],
            Path::new("/"),
            1024,
            Duration::from_secs(2),
        )?)?
        .trim()
        .into(),
        observed_at: chrono::Utc::now().to_rfc3339(),
        local_time: chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        jobs: Vec::new(),
        errors: BTreeMap::new(),
    };
    let mut overrides = BTreeMap::new();
    for domain in [&user, "system"] {
        match launchctl(&["print-disabled", domain]).and_then(|(result, output)| {
            ensure!(
                result.outcome.status == "succeeded",
                "cannot inspect disabled services"
            );
            disabled_overrides(&output)
        }) {
            Ok(map) => {
                overrides.insert(domain.to_owned(), map);
            }
            Err(e) => {
                inventory.errors.insert(domain.to_owned(), format!("{e:#}"));
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    for (root, domain) in [
        (home.home_dir().join("Library/LaunchAgents"), user.as_str()),
        ("/Library/LaunchAgents".into(), user.as_str()),
        ("/Library/LaunchDaemons".into(), "system"),
    ] {
        if !root.exists() {
            continue;
        }
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) => {
                inventory
                    .errors
                    .insert(root.display().to_string(), e.to_string());
                continue;
            }
        };
        for (index, entry) in entries.enumerate() {
            if index >= 1024 {
                inventory.errors.insert(
                    root.display().to_string(),
                    "Directory entry limit reached".into(),
                );
                break;
            }
            let result = (|| -> Result<Option<DiscoveredJob>> {
                let path = entry?.path();
                if path.extension().is_none_or(|x| x != "plist") {
                    return Ok(None);
                }
                let mut data = Vec::new();
                fs::File::open(&path)?
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut data)?;
                ensure!(
                    data.len() <= 1024 * 1024,
                    "plist exceeds size limit: {}",
                    path.display()
                );
                let mut job =
                    definition(&path, domain, &data).with_context(|| path.display().to_string())?;
                if job.label.starts_with("com.apple.") {
                    return Ok(None);
                }
                job.disabled = overrides.get(domain).map(|map| {
                    map.get(&job.label)
                        .copied()
                        .unwrap_or(job.disabled.unwrap_or(false))
                });
                if Instant::now() < deadline {
                    let target = format!("{domain}/{}", job.label);
                    let observation =
                        launchctl(&["print", &target]).and_then(|(result, output)| {
                            if result.exit_code == Some(113) {
                                job.loaded = Some(false);
                                return Ok(());
                            }
                            ensure!(
                                result.outcome.status == "succeeded",
                                "cannot inspect service runtime"
                            );
                            runtime(&mut job, &output)
                        });
                    if let Err(e) = observation {
                        job.error = Some(format!("{e:#}"));
                    }
                } else {
                    job.error =
                        Some("Runtime snapshot time limit reached; refresh to try again".into());
                }
                Ok(Some(job))
            })();
            match result {
                Ok(Some(job)) => inventory.jobs.push(job),
                Ok(None) => {}
                Err(e) => {
                    inventory
                        .errors
                        .insert(format!("{}:{index}", root.display()), format!("{e:#}"));
                }
            }
        }
    }
    inventory
        .jobs
        .sort_by(|a, b| (&a.label, &a.path).cmp(&(&b.label, &b.path)));
    Ok(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_plist_retains_combined_triggers_without_exposing_environment() {
        let value = Plist::Dictionary(Dictionary::from_iter([
            ("Label", "org.example.capture".into()),
            ("Program", "/usr/bin/true".into()),
            (
                "StartCalendarInterval",
                Plist::Dictionary(Dictionary::from_iter([("Minute", Plist::from(30))])),
            ),
            ("StartInterval", 900.into()),
            ("RunAtLoad", true.into()),
            (
                "EnvironmentVariables",
                Plist::Dictionary(Dictionary::from_iter([(
                    "TOKEN",
                    Plist::from("secret-fixture"),
                )])),
            ),
        ]));
        let mut bytes = Vec::new();
        value.to_writer_binary(&mut bytes).unwrap();
        let job = definition(Path::new("/fixture.plist"), "gui/501", &bytes).unwrap();
        assert_eq!(
            job.schedule,
            ["hourly at :30", "Every 900 seconds", "When loaded"]
        );
        assert!(
            !serde_json::to_string(&job)
                .unwrap()
                .contains("secret-fixture")
        );
        assert!(job.loaded.is_none());
    }
    #[test]
    fn disabled_overrides_and_runtime_do_not_confuse_nested_fields() {
        assert!(disabled_overrides("unknown format").is_err());
        let map =
            disabled_overrides("disabled services = {\n\"org.example.capture\" => disabled\n}")
                .unwrap();
        assert!(map["org.example.capture"]);
        let mut job=definition(Path::new("/fixture.plist"), "gui/501", br#"<?xml version="1.0"?><plist version="1.0"><dict><key>Label</key><string>org.example.capture</string></dict></plist>"#).unwrap();
        runtime(&mut job, "gui/501/org.example.capture = {\n\tstate = not running\n\tenvironment = {\n\t\tpid = 999\n\t}\n\tlast exit code = 1\n}").unwrap();
        assert_eq!(job.loaded, Some(true));
        assert_eq!(job.pid, None);
        assert_eq!(job.last_exit_code, Some(1));
        job.last_exit_code = None;
        runtime(&mut job, "gui/501/org.example.capture = {\n\tstate = running\n\tpid = 42\n\tlast exit code = (never exited)\n}").unwrap();
        assert_eq!(job.pid, Some(42));
        assert!(job.last_exit_code.is_none());
        runtime(&mut job, "gui/501/org.example.capture = {\n\tstate = spawn scheduled\n\tlast exit code = 78: EX_CONFIG\n}").unwrap();
        assert_eq!(job.last_exit_code, Some(78));
    }
}
