//! Private SSH connections for discovery and schedule editing. No remote daemon.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
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
        if let Some(home) = &self.home {
            ensure!(
                home.starts_with('/') && home.len() <= 4096 && !home.chars().any(char::is_control),
                "remote home must be an absolute path"
            );
        }
        Ok(())
    }
    fn arguments(&self, operation: &str) -> Result<Vec<String>> {
        self.validate()?;
        ensure!(
            ["discover --json", "bridge"].contains(&operation),
            "unsupported SSH operation"
        );
        let home = self
            .home
            .as_ref()
            .map(|home| format!(" --home {}", quote(home)))
            .unwrap_or_default();
        let command = format!("{}{home} {operation}", quote(&self.cli));
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
    let bytes=process::capture(&host.arguments("discover --json")?, Path::new("/"), 4 * 1024 * 1024, Duration::from_secs(25))
        .with_context(|| format!("Cannot read {id}. Check SSH access and the installed Vardr CLI; no remote changes were made"))?;
    serde_json::from_slice(&bytes)
        .context("Remote discovery response is incompatible; update Vardr on that Mac")
}
pub fn request(
    store: &Store,
    id: &str,
    operation: crate::bridge::Operation,
) -> Result<serde_json::Value> {
    let hosts = list(store)?;
    let host = hosts
        .iter()
        .find(|host| host.id == id)
        .context("unknown remote host")?;
    let mutating = matches!(operation, crate::bridge::Operation::Save { .. });
    let input = serde_json::to_string(&crate::bridge::Request {
        protocol: 1,
        operation,
    })?;
    let bytes = process::capture_input(&host.arguments("bridge")?, Path::new("/"), Some(&input), 4 * 1024 * 1024, Duration::from_secs(25))
        .with_context(|| if mutating { format!("Update outcome on {id} is unknown; refresh that Mac before retrying. The request was not retried") } else { format!("Cannot read {id}; check SSH access and update Vardr on that Mac") })?;
    let response: serde_json::Value = serde_json::from_slice(&bytes)
        .context("Invalid remote bridge response; refresh before retrying an edit")?;
    ensure!(
        response["protocol"] == 1,
        "incompatible remote bridge protocol"
    );
    if let Some(error) = response.get("error").and_then(serde_json::Value::as_str) {
        anyhow::bail!("{error}");
    }
    response
        .get("result")
        .cloned()
        .context("remote bridge response has no result")
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
            home: None,
        };
        let args = host.arguments("discover --json").unwrap();
        assert_eq!(
            args.last().unwrap(),
            "'/Applications/Vardr'\\''s app/Contents/MacOS/vardr' discover --json"
        );
        assert!(args.contains(&"-oBatchMode=yes".into()));
        assert!(args.contains(&"-oStrictHostKeyChecking=yes".into()));
        for bad in ["-oProxyCommand=bad", "host;touch bad", "host\nnext"] {
            host.ssh = bad.into();
            assert!(host.arguments("discover --json").is_err());
        }
    }
}
