use anyhow::{Result, ensure};
use chrono::{DateTime, Datelike, Days, LocalResult, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    Manual,
    Interval {
        seconds: u64,
    },
    Daily {
        time: String,
        timezone: String,
        /// ISO weekdays: Monday=1, Sunday=7; an empty list means every day.
        #[serde(default)]
        weekdays: Vec<u32>,
    },
}

impl Schedule {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Manual => (),
            Self::Interval { seconds } => ensure!(
                (10..=31_536_000).contains(seconds),
                "interval must be 10 seconds to one year"
            ),
            Self::Daily {
                time,
                timezone,
                weekdays,
            } => {
                parse_time(time)?;
                timezone.parse::<Tz>()?;
                ensure!(
                    weekdays.iter().all(|d| (1..=7).contains(d)),
                    "weekdays must be 1 (Monday) to 7 (Sunday)"
                );
            }
        }
        Ok(())
    }

    /// Latest occurrence since the watermark. Coalesces missed runs instead of replaying a backlog.
    pub fn due(
        &self,
        watermark: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>> {
        if now <= watermark {
            return Ok(None);
        }
        Ok(match self {
            Self::Manual => None,
            Self::Interval { seconds } => {
                let elapsed = (now - watermark).num_seconds();
                let n = elapsed / *seconds as i64;
                (n > 0).then(|| watermark + chrono::Duration::seconds(n * *seconds as i64))
            }
            Self::Daily {
                time,
                timezone,
                weekdays,
            } => {
                let tz: Tz = timezone.parse()?;
                let day = now.with_timezone(&tz).date_naive();
                let clock = parse_time(time)?;
                let mut latest = None;
                for ago in 0..=7 {
                    let date = day.checked_sub_days(Days::new(ago)).unwrap();
                    if !weekdays.is_empty()
                        && !weekdays.contains(&date.weekday().number_from_monday())
                    {
                        continue;
                    }
                    let slot = match tz.from_local_datetime(&date.and_time(clock)) {
                        LocalResult::Single(x) => x,
                        // Run once at the earlier occurrence during a fall-back hour.
                        LocalResult::Ambiguous(a, b) => a.min(b),
                        LocalResult::None => continue,
                    }
                    .with_timezone(&Utc);
                    if slot <= now && slot > watermark {
                        latest = Some(slot);
                        break;
                    }
                }
                latest
            }
        })
    }
}

pub fn parse_time(value: &str) -> Result<NaiveTime> {
    let time = NaiveTime::parse_from_str(value, "%H:%M")?;
    ensure!(
        time.format("%H:%M").to_string() == value,
        "time must use HH:MM"
    );
    Ok(time)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Command {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Codex {
        #[serde(default)]
        model: Option<String>,
    },
    Claude {
        #[serde(default)]
        model: Option<String>,
    },
    T3 {
        endpoint: String,
        #[serde(default)]
        authorization_env: Option<String>,
        #[serde(default)]
        authorization_gopass: Option<String>,
        project_id: String,
        #[serde(default)]
        model_selection: Option<serde_json::Value>,
    },
}

impl Target {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Command { argv, env } => {
                ensure!(
                    !argv.is_empty() && !argv[0].is_empty(),
                    "command argv needs an executable"
                );
                ensure!(
                    env.iter().all(|(key, value)| !key.is_empty()
                        && !key.contains(['=', '\0'])
                        && !value.contains('\0')),
                    "invalid command environment"
                );
            }
            Self::T3 {
                endpoint,
                authorization_env,
                authorization_gopass,
                project_id,
                ..
            } => {
                let url = reqwest::Url::parse(endpoint)?;
                ensure!(
                    url.username().is_empty() && url.password().is_none(),
                    "T3 credentials belong in a secret reference"
                );
                ensure!(
                    url.scheme() == "https"
                        || (url.scheme() == "http"
                            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
                    "T3 requires HTTPS or loopback HTTP"
                );
                ensure!(
                    authorization_env.is_some() != authorization_gopass.is_some(),
                    "T3 requires exactly one of authorization_env or authorization_gopass"
                );
                ensure!(!project_id.is_empty(), "T3 project ID is required");
                if let Some(name) = authorization_env {
                    ensure!(
                        !name.is_empty() && !name.contains('='),
                        "invalid authorization environment name"
                    );
                }
                if let Some(key) = authorization_gopass {
                    ensure!(
                        !key.is_empty() && !key.starts_with('-'),
                        "invalid gopass reference"
                    );
                }
            }
            _ => (),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Investigation {
    #[serde(default)]
    pub enabled: bool,
    pub target: Target,
    #[serde(default = "cooldown")]
    pub cooldown_seconds: u64,
    #[serde(default = "investigation_timeout")]
    pub timeout_seconds: u64,
}
fn cooldown() -> u64 {
    3600
}
fn investigation_timeout() -> u64 {
    600
}
fn timeout() -> u64 {
    3600
}
fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub purpose: String,
    #[serde(default)]
    pub project: String,
    /// Descriptive machine label. Each installation owns only its local jobs.
    #[serde(default)]
    pub machine: String,
    pub cwd: PathBuf,
    pub target: Target,
    #[serde(default)]
    pub prompt: String,
    pub schedule: Schedule,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub catch_up: bool,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub investigation: Option<Investigation>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 80
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "ID must be 1–80 lowercase letters, digits, or hyphens"
    );
    Ok(())
}

impl Job {
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        ensure!(self.id.len() <= 64, "job IDs must be at most 64 characters");
        ensure!(
            !self.name.trim().is_empty() && !self.purpose.trim().is_empty(),
            "name and purpose are required"
        );
        ensure!(self.cwd.is_absolute(), "working directory must be absolute");
        ensure!(
            (1..=604800).contains(&self.timeout_seconds),
            "timeout must be 1 second to seven days"
        );
        self.schedule.validate()?;
        self.target.validate()?;
        if !matches!(self.target, Target::Command { .. }) {
            ensure!(
                !self.prompt.trim().is_empty(),
                "agent jobs require a prompt"
            );
        }
        if let Some(i) = &self.investigation {
            i.target.validate()?;
            ensure!(
                i.cooldown_seconds <= 31_536_000,
                "investigation cooldown must not exceed one year"
            );
            ensure!(
                (1..=86400).contains(&i.timeout_seconds),
                "investigation timeout must be 1 second to one day"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub max_concurrency: usize,
    pub retained_runs: usize,
    pub log_limit_bytes: u64,
    pub updates: UpdateSettings,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrency: 1,
            retained_runs: 30,
            log_limit_bytes: 1_048_576,
            updates: UpdateSettings::default(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16).contains(&self.max_concurrency),
            "max_concurrency must be 1–16"
        );
        ensure!(
            (1..=1000).contains(&self.retained_runs),
            "retained_runs must be 1–1000"
        );
        ensure!(
            (4096..=16_777_216).contains(&self.log_limit_bytes),
            "log limit must be 4 KiB–16 MiB"
        );
        parse_time(&self.updates.time)?;
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateSettings {
    pub automatic: bool,
    pub time: String,
}
impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            automatic: true,
            time: "00:00".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Run {
    pub id: String,
    pub job_id: String,
    pub job_revision: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub error: Option<String>,
    pub investigation: Option<Box<Outcome>>,
    pub remote: Option<serde_json::Value>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Outcome {
    pub status: String,
    pub error: Option<String>,
    pub remote: Option<serde_json::Value>,
}
impl Outcome {
    pub fn success() -> Self {
        Self {
            status: "succeeded".into(),
            error: None,
            remote: None,
        }
    }
    pub fn failed(error: impl ToString) -> Self {
        Self {
            status: "failed".into(),
            error: Some(error.to_string()),
            remote: None,
        }
    }
}
#[derive(Default, Deserialize, Serialize)]
pub struct RuntimeState {
    pub watermark: Option<DateTime<Utc>>,
    pub last_investigation: Option<DateTime<Utc>>,
}
#[derive(Default, Deserialize, Serialize)]
pub struct ServiceState {
    pub updated_at: Option<DateTime<Utc>>,
    pub pid: u32,
    pub running: Vec<String>,
    pub errors: BTreeMap<String, String>,
}

#[derive(Default, Serialize)]
pub struct JobInventory {
    pub jobs: Vec<Job>,
    pub errors: BTreeMap<String, String>,
}
