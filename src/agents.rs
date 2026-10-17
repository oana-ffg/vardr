use crate::{
    model::{Outcome, Target},
    process,
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    time::Duration,
};

pub fn execute(
    target: &Target,
    prompt: &str,
    cwd: &Path,
    log: &Path,
    limit: u64,
    timeout: Duration,
) -> Result<Outcome> {
    match target {
        Target::Command { argv, env } => {
            process::execute_with_env(argv, cwd, None, env, log, limit, timeout)
        }
        Target::Codex { model } => {
            let mut argv = vec![
                "codex".into(),
                "exec".into(),
                "--sandbox".into(),
                "read-only".into(),
            ];
            if let Some(model) = model {
                argv.extend(["--model".into(), model.clone()]);
            }
            argv.push("-".into());
            process::execute(&argv, cwd, Some(prompt), log, limit, timeout)
        }
        Target::Claude { model } => {
            let mut argv = vec![
                "claude".into(),
                "-p".into(),
                "--permission-mode".into(),
                "plan".into(),
                "--no-chrome".into(),
            ];
            if let Some(model) = model {
                argv.extend(["--model".into(), model.clone()]);
            }
            process::execute(&argv, cwd, Some(prompt), log, limit, timeout)
        }
        Target::T3 {
            endpoint,
            authorization_env,
            authorization_gopass,
            project_id,
            model_selection,
        } => {
            let authorization = if let Some(name) = authorization_env {
                std::env::var(name).with_context(|| {
                    format!("set {name} in the runner's environment; T3 authorization is missing")
                })?
            } else {
                let key = authorization_gopass
                    .as_ref()
                    .context("T3 authorization reference is missing")?;
                let bytes = process::capture(
                    &["gopass".into(), "show".into(), "-o".into(), key.clone()],
                    Path::new("/"),
                    8192,
                    Duration::from_secs(10),
                )
                .context("could not resolve T3 authorization from gopass")?;
                String::from_utf8(bytes)?.trim().to_string()
            };
            let mut mcp = Mcp::connect(endpoint, &authorization)?;
            let mut args = json!({"title":"Vardr automation", "projectId":project_id, "message":prompt, "runtimeMode":"approval-required", "interactionMode":"default", "workspaceStrategy":{"type":"root"}});
            if let Some(selection) = model_selection {
                args["modelSelection"] = selection.clone();
            }
            // Launch is deliberately not retried: this tool has no idempotency key.
            // Lost responses must be reconciled in T3 instead of creating duplicates.
            let receipt=match mcp.call("t3_thread_launch",args) {
                Ok(receipt) if receipt.get("threadId").and_then(Value::as_str).is_some()=>receipt,
                _=>return Ok(Outcome {status:"dispatch_unknown".into(),error:Some("T3 launch outcome is unknown; inspect T3 before retrying to avoid a duplicate thread".into()),remote:None}),
            };
            if fs::write(log, serde_json::to_vec_pretty(&receipt)?).is_err() {
                return Ok(Outcome {status:"dispatch_unknown".into(),error:Some("T3 accepted the launch but its receipt log could not be saved; inspect the recorded remote thread before retrying".into()),remote:Some(receipt)});
            }
            Ok(Outcome {
                status: "dispatched".into(),
                error: None,
                remote: Some(receipt),
            })
        }
    }
}

/// Streamable HTTP MCP. Credentials are resolved from the environment for each
/// connection; saved job definitions contain only their variable names.
struct Mcp {
    endpoint: String,
    client: Client,
    headers: HeaderMap,
    next: u64,
}
impl Mcp {
    fn connect(endpoint: &str, authorization: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut header =
            HeaderValue::from_str(authorization).context("invalid T3 authorization header")?;
        header.set_sensitive(true);
        headers.insert("authorization", header);
        headers.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        let mut this = Self {
            endpoint: endpoint.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(45))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            headers,
            next: 1,
        };
        let result = this.request("initialize", json!({"protocolVersion":"2025-06-18", "capabilities":{}, "clientInfo":{"name":"vardr","version":env!("CARGO_PKG_VERSION")}}), true)?;
        if let Some(protocol) = result.get("protocolVersion").and_then(Value::as_str) {
            this.headers
                .insert("mcp-protocol-version", HeaderValue::from_str(protocol)?);
        }
        this.request("notifications/initialized", json!({}), false)?;
        Ok(this)
    }
    fn request(&mut self, method: &str, params: Value, response_expected: bool) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let mut message = json!({"jsonrpc":"2.0", "method":method, "params":params});
        if response_expected {
            message["id"] = json!(id);
        }
        let response = self
            .client
            .post(&self.endpoint)
            .headers(self.headers.clone())
            .json(&message)
            .send()
            .context("T3 MCP connection failed")?;
        ensure!(
            response.status().is_success(),
            "T3 MCP returned HTTP {}",
            response.status()
        );
        if let Some(session) = response.headers().get("mcp-session-id") {
            self.headers.insert("mcp-session-id", session.clone());
        }
        if !response_expected {
            return Ok(Value::Null);
        }
        let sse = response
            .headers()
            .get("content-type")
            .is_some_and(|v| v.to_str().unwrap_or("").contains("text/event-stream"));
        if sse {
            let mut reader = BufReader::new(response.take(2_097_153));
            let mut event = String::new();
            let mut total = 0;
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line)?;
                total += n;
                ensure!(total <= 2_097_152, "T3 response exceeds 2 MiB");
                if n == 0 || line.trim_end().is_empty() {
                    if !event.is_empty() {
                        if let Some(value) =
                            matching_result(serde_json::from_str(&event)?, id, method)?
                        {
                            return Ok(value);
                        }
                        event.clear();
                    }
                    if n == 0 {
                        break;
                    }
                } else if let Some(data) = line.strip_prefix("data:") {
                    event.push_str(data.trim_start().trim_end_matches(['\r', '\n']));
                    event.push('\n');
                }
            }
        } else {
            let mut bytes = Vec::new();
            response.take(2_097_153).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 2_097_152, "T3 response exceeds 2 MiB");
            if let Some(value) = matching_result(serde_json::from_slice(&bytes)?, id, method)? {
                return Ok(value);
            }
        }
        bail!("T3 MCP response has no matching request ID")
    }
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self.request(
            "tools/call",
            json!({"name":name,"arguments":arguments}),
            true,
        )?;
        ensure!(
            result.get("isError") != Some(&Value::Bool(true)),
            "T3 {name} failed; inspect T3 before retrying"
        );
        if let Some(value) = result.get("structuredContent") {
            return Ok(value.clone());
        }
        for content in result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) = content.get("text").and_then(Value::as_str)
                && let Ok(value) = serde_json::from_str(text)
            {
                return Ok(value);
            }
        }
        bail!("T3 returned no structured result for {name}")
    }
}

fn matching_result(value: Value, id: u64, method: &str) -> Result<Option<Value>> {
    let values = if let Value::Array(values) = value {
        values
    } else {
        vec![value]
    };
    for envelope in values {
        if envelope.get("id") != Some(&json!(id)) {
            continue;
        }
        ensure!(
            envelope.get("error").is_none(),
            "T3 MCP rejected {method}; inspect the server for details"
        );
        return Ok(Some(
            envelope
                .get("result")
                .cloned()
                .context("T3 MCP response has no result")?,
        ));
    }
    Ok(None)
}
