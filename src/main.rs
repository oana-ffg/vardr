use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{fs, path::PathBuf};
use vardr::{
    hosts,
    model::{Job, Schedule},
    process, runner, scheduler, service,
    store::{Store, read_json, write_json},
    ui, updates,
};

#[derive(Parser)]
#[command(version, about = "Lightweight automations for humans and agents")]
struct Cli {
    /// Local data directory; use separate directories for isolated testing.
    #[arg(long, global = true, env = "VARDR_HOME")]
    home: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Create an empty local installation. Does not install a service or any jobs.
    Init,
    List,
    Show {
        id: String,
    },
    /// Import or update a job definition. Existing jobs require their current revision.
    Put {
        file: PathBuf,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    /// Run now, including when paused. A manual run does not move the schedule.
    Run {
        id: String,
    },
    History {
        id: String,
    },
    Logs {
        id: String,
        #[arg(long)]
        investigation: bool,
    },
    Schedule {
        id: String,
        #[arg(long, conflicts_with_all=["every","manual"])]
        daily: Option<String>,
        #[arg(long, requires = "daily")]
        timezone: Option<String>,
        #[arg(long, conflicts_with_all=["daily","manual"])]
        every: Option<u64>,
        #[arg(long, conflicts_with_all=["daily","every"])]
        manual: bool,
    },
    /// Toggle a configured investigator. Configure its target in the job definition first.
    Investigate {
        id: String,
        #[arg(long, action=clap::ArgAction::Set)]
        enabled: bool,
    },
    /// Run the scheduler. --once dispatches due jobs and exits after they finish.
    Serve {
        #[arg(long)]
        once: bool,
    },
    /// Start the optional local dashboard. Closing it never stops the scheduler.
    Ui {
        #[arg(long, default_value_t = 7818)]
        port: u16,
    },
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    Update {
        #[arg(long)]
        install: bool,
    },
    /// Verify a downloaded release binary using Vardr's compiled signing key.
    Verify {
        binary: PathBuf,
        signature: PathBuf,
    },
    Status,
    /// Read-only launchd definitions and runtime snapshot (macOS).
    Discover {
        #[arg(long, default_value = "local")]
        host: String,
    },
    /// Private SSH connections for read-only launchd discovery.
    Hosts {
        #[command(subcommand)]
        action: HostAction,
    },
}
#[derive(Subcommand)]
enum HostAction {
    List,
    Add {
        id: String,
        #[arg(long)]
        ssh: String,
        #[arg(long, default_value = "/Applications/Vardr.app/Contents/MacOS/vardr")]
        cli: String,
    },
    Remove {
        id: String,
    },
}
#[derive(Subcommand)]
enum ServiceAction {
    Plan,
    Install,
    Remove,
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    if let Err(e) = entry(cli) {
        if json {
            eprintln!("{}", serde_json::json!({"error":format!("{e:#}")}));
        } else {
            eprintln!("vardr: {e:#}");
        }
        std::process::exit(1);
    }
}
fn output(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn entry(cli: Cli) -> Result<()> {
    if let Commands::Verify { binary, signature } = &cli.command {
        ensure!(
            fs::metadata(binary)?.len() <= 64 * 1024 * 1024,
            "binary exceeds update size limit"
        );
        updates::verify(
            updates::PUBLIC_KEY,
            &fs::read(binary)?,
            &fs::read_to_string(signature)?,
        )?;
        return output(&json!({"verified":true}));
    }
    let root = cli.home.unwrap_or_else(|| {
        directories::ProjectDirs::from("dev", "vardr", "vardr")
            .expect("home directory required")
            .data_local_dir()
            .to_path_buf()
    });
    fs::create_dir_all(&root)?;
    let store = Store::open(root.canonicalize()?)?;
    process::handle_signals();
    match cli.command {
        Commands::Init => {
            let p = store.root.join("config.json");
            if !p.exists() {
                write_json(&p, &vardr::model::Settings::default())?;
            }
            output(
                &json!({"home":store.root,"service_installed":false,"settings":store.settings()?}),
            )?;
        }
        Commands::List => {
            let jobs = store.jobs()?;
            if cli.json {
                output(&jobs)?;
            } else {
                if jobs.jobs.is_empty() {
                    println!("No automations yet. Import one with `vardr put job.json`.");
                }
                for (file, error) in &jobs.errors {
                    eprintln!("{file}: {error}");
                }
                for job in jobs.jobs {
                    println!(
                        "{}  {}  {}\n  {}\n  {}",
                        if job.enabled { "enabled" } else { "paused " },
                        job.id,
                        job.name,
                        job.purpose,
                        serde_json::to_string(&job.schedule)?
                    );
                }
            }
        }
        Commands::Show { id } => output(&store.job(&id)?)?,
        Commands::Put { file } => {
            let job: Job = read_json(&file.canonicalize()?)?;
            output(&store.save(job)?)?;
        }
        Commands::Pause { id } => {
            let mut job = store.job(&id)?;
            job.enabled = false;
            output(&store.save(job)?)?;
        }
        Commands::Resume { id } => {
            let mut job = store.job(&id)?;
            job.enabled = true;
            output(&store.save(job)?)?;
        }
        Commands::Run { id } => {
            let run = runner::run(&store, &id)?;
            output(&run)?;
            ensure!(
                matches!(run.status.as_str(), "succeeded" | "dispatched"),
                "automation {}",
                run.status
            );
        }
        Commands::History { id } => output(&store.runs(&id)?)?,
        Commands::Logs { id, investigation } => {
            let text = runner::logs(&store, &id, investigation)?;
            if cli.json {
                output(&json!({"text":text}))?;
            } else {
                print!("{text}");
            }
        }
        Commands::Schedule {
            id,
            daily,
            timezone,
            every,
            manual,
        } => {
            let mut job = store.job(&id)?;
            job.schedule = if let Some(time) = daily {
                Schedule::Daily {
                    time,
                    timezone: timezone.context("--daily requires --timezone")?,
                    weekdays: Vec::new(),
                }
            } else if let Some(seconds) = every {
                Schedule::Interval { seconds }
            } else {
                ensure!(manual, "choose --daily, --every, or --manual");
                Schedule::Manual
            };
            output(&store.save(job)?)?;
        }
        Commands::Investigate { id, enabled } => {
            let mut job = store.job(&id)?;
            job.investigation
                .as_mut()
                .context("configure investigation.target with `put` first")?
                .enabled = enabled;
            output(&store.save(job)?)?;
        }
        Commands::Serve { once } => {
            let executable = std::env::current_exe()?;
            if scheduler::serve(&store, once)? {
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    let e = std::process::Command::new(executable)
                        .args(std::env::args_os().skip(1))
                        .exec();
                    return Err(e.into());
                }
            }
        }
        Commands::Ui { port } => ui::serve(&store, port)?,
        Commands::Service { action } => match action {
            ServiceAction::Plan => {
                let plan = service::plan(&store)?;
                if cli.json {
                    output(&json!({"plist":plan}))?;
                } else {
                    print!("{plan}");
                }
            }
            ServiceAction::Install => service::install(&store)?,
            ServiceAction::Remove => service::remove(&store)?,
        },
        Commands::Update { install } => {
            let _lock = store.lock("update")?;
            // A manual replacement must not race a service update. The running
            // scheduler owns this lock for its full lifetime.
            let _service = if install {
                Some(store.lock("service").context("stop the service before a manual update; it also updates automatically while idle")?)
            } else {
                None
            };
            let release = updates::check()?;
            if install && let Some(r) = &release {
                updates::install(r)?;
            }
            output(&json!({"available":release,"installed":install && release.is_some()}))?;
        }
        Commands::Status => {
            let heartbeat = store.root.join("service.json");
            let state: Option<serde_json::Value> = if heartbeat.exists() {
                Some(read_json(&heartbeat)?)
            } else {
                None
            };
            let running = store.is_locked("service")?;
            let updater = store.root.join("updates.json");
            let updates: Option<serde_json::Value> = if updater.exists() {
                Some(read_json(&updater)?)
            } else {
                None
            };
            output(
                &json!({"home":store.root,"service_running":running,"service":state,"updates":updates,"settings":store.settings()?}),
            )?;
        }
        Commands::Discover { host } => {
            let inventory = hosts::discover(&store, &host)?;
            if cli.json {
                output(&inventory)?;
            } else {
                println!(
                    "{} · {} · read-only launchd snapshot",
                    inventory.machine, inventory.local_time
                );
                for job in &inventory.jobs {
                    let state = if job.disabled == Some(true) {
                        "disabled"
                    } else {
                        job.state
                            .as_deref()
                            .unwrap_or(if job.loaded == Some(false) {
                                "not loaded"
                            } else {
                                "unknown"
                            })
                    };
                    println!("{}  [{}]  {}", job.label, state, job.schedule.join("; "));
                }
                for (path, error) in &inventory.errors {
                    eprintln!("{path}: {error}");
                }
            }
        }
        Commands::Hosts { action } => match action {
            HostAction::List => output(&hosts::list(&store)?)?,
            HostAction::Add { id, ssh, cli } => {
                hosts::add(&store, hosts::Host { id, ssh, cli })?;
                output(&hosts::list(&store)?)?;
            }
            HostAction::Remove { id } => {
                hosts::remove(&store, &id)?;
                output(&hosts::list(&store)?)?;
            }
        },
        Commands::Verify { .. } => unreachable!(),
    }
    Ok(())
}
