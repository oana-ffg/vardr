//! Private SSH connections for on-demand read-only discovery. No remote daemon.
use crate::{
    discovery::Inventory,
    model::validate_id,
    process,
    store::{Store, read_json, write_json},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub id: String,
    pub ssh: String,
    pub cli: String,
}
impl Host {
    fn validate(&self) -> Result<()> {
        validate_id(&self.id)?;
        ensure!(self.id != "local", "local is reserved");
        ensure!(
            self.ssh.len() <= 200
                && self.ssh.starts_with(|c: char| c.is_ascii_alphanumeric())
                && self
                    .ssh
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
            "ssh must be an SSH config alias or hostname"
        );
        ensure!(
            self.cli.starts_with('/')
                && self.cli.len() <= 4096
                && !self.cli.chars().any(char::is_control),
            "remote CLI must be an absolute path"
        );
        Ok(())
    }
    fn arguments(&self) -> Result<Vec<String>> {
        self.validate()?;
        let command = format!("{} discover --json", quote(&self.cli));
        Ok([
            "/usr/bin/ssh",
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oConnectTimeout=5",
            "-oConnectionAttempts=1",
            "-oServerAliveInterval=5",
            "-oServerAliveCountMax=1",
            "--",
            &self.ssh,
            &command,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect())
    }
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
pub fn list(store: &Store) -> Result<Vec<Host>> {
    let path = store.root.join("hosts.json");
    let hosts: Vec<Host> = if path.exists() {
        read_json(&path)?
    } else {
        Vec::new()
    };
    ensure!(hosts.len() <= 16, "at most 16 remote hosts supported");
    for (index, host) in hosts.iter().enumerate() {
        host.validate()?;
        ensure!(
            !hosts[..index].iter().any(|other| other.id == host.id),
            "duplicate host ID"
        );
    }
    Ok(hosts)
}
pub fn add(store: &Store, host: Host) -> Result<()> {
    host.validate()?;
    let _lock = store.transaction("hosts")?;
    let mut hosts = list(store)?;
    ensure!(
        !hosts.iter().any(|other| other.id == host.id),
        "host already exists; remove it before replacing"
    );
    ensure!(hosts.len() < 16, "at most 16 remote hosts supported");
    hosts.push(host);
    write_json(&store.root.join("hosts.json"), &hosts)
}
pub fn remove(store: &Store, id: &str) -> Result<()> {
    let _lock = store.transaction("hosts")?;
    let mut hosts = list(store)?;
    let index = hosts
        .iter()
        .position(|host| host.id == id)
        .context("unknown host")?;
    hosts.remove(index);
    write_json(&store.root.join("hosts.json"), &hosts)
}
pub fn discover(store: &Store, id: &str) -> Result<Inventory> {
    if id == "local" {
        return crate::discovery::local();
    }
    let hosts = list(store)?;
    let host = hosts
        .iter()
        .find(|host| host.id == id)
        .context("unknown host; use `vardr hosts add`")?;
    let bytes=process::capture(&host.arguments()?, Path::new("/"), 4 * 1024 * 1024, Duration::from_secs(25))
        .with_context(|| format!("Cannot read {id}. Check SSH access and the installed Vardr CLI; no remote changes were made"))?;
    serde_json::from_slice(&bytes)
        .context("Remote discovery response is incompatible; update Vardr on that Mac")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ssh_arguments_reject_options_and_quote_the_only_remote_command() {
        let mut host = Host {
            id: "test-mac".into(),
            ssh: "test-mac".into(),
            cli: "/Applications/Vardr's app/Contents/MacOS/vardr".into(),
        };
        let args = host.arguments().unwrap();
        assert_eq!(
            args.last().unwrap(),
            "'/Applications/Vardr'\\''s app/Contents/MacOS/vardr' discover --json"
        );
        assert!(args.contains(&"-oBatchMode=yes".into()));
        assert!(args.contains(&"-oStrictHostKeyChecking=yes".into()));
        for bad in ["-oProxyCommand=bad", "host;touch bad", "host\nnext"] {
            host.ssh = bad.into();
            assert!(host.arguments().is_err());
        }
    }
}
