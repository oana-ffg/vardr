use crate::{
    model::{Job, ServiceState},
    process::STOP,
    runner::{self, Workers},
    store::{Store, read_json},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Bind before opening an app window, so the private URL is ready synchronously.
pub struct Dashboard {
    server: Server,
    port: u16,
    token: String,
    activity: Arc<AtomicUsize>,
}
impl Dashboard {
    pub fn bind(port: u16) -> Result<Self> {
        let server = Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!("{e}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .context("no local address")?
            .port();
        Ok(Self {
            server,
            port,
            token: uuid::Uuid::new_v4().to_string(),
            activity: Arc::default(),
        })
    }
    pub fn entry_url(&self) -> String {
        format!("http://127.0.0.1:{}/?token={}", self.port, self.token)
    }
    pub fn activity(&self) -> Arc<AtomicUsize> {
        self.activity.clone()
    }
    pub fn run(self, store: &Store) -> Result<()> {
        let Self {
            server,
            port,
            token,
            activity,
        } = self;
        let mut workers = Workers::default();
        while !STOP.load(Ordering::Relaxed) {
            for (id, result) in workers.reap() {
                if let Err(e) = result {
                    eprintln!("{id}: {e:#}");
                }
            }
            activity.store(workers.tasks.len(), Ordering::Relaxed);
            let Some(mut request) = server.recv_timeout(Duration::from_millis(250))? else {
                continue;
            };
            let valid_host = header(&request, "Host").is_some_and(|h| {
                h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
            });
            if !valid_host {
                respond(request, 403, "text/plain", "Invalid host", None);
                continue;
            }
            let url = request.url().to_owned();
            let (path, query) = url.split_once('?').unwrap_or((&url, ""));
            if request.method() == &Method::Get && matches!(path, "/" | "/assets/vardr.png") {
                let query_token = query.split('&').find_map(|p| p.strip_prefix("token="));
                let cookie_ok = header(&request, "Cookie").is_some_and(|c| {
                    c.split(';')
                        .any(|part| part.trim() == format!("vardr={token}"))
                });
                if query_token != Some(token.as_str()) && !cookie_ok {
                    respond(
                        request,
                        403,
                        "text/plain",
                        "Open the private dashboard URL printed by `vardr ui`.",
                        None,
                    );
                    continue;
                }
                if path == "/assets/vardr.png" {
                    respond(
                        request,
                        200,
                        "image/png",
                        include_bytes!("../assets/vardr.png").as_slice(),
                        None,
                    );
                    continue;
                }
                let html = include_str!("ui.html").replace("__VARDR_TOKEN__", &token);
                respond(request, 200, "text/html; charset=utf-8", html, Some(&token));
                continue;
            }
            if header(&request, "X-Vardr-Token") != Some(token.as_str()) {
                respond(
                    request,
                    403,
                    "application/json",
                    json!({"error":"Missing dashboard token"}).to_string(),
                    None,
                );
                continue;
            }
            let result = handle(store, &mut request, &mut workers);
            activity.store(workers.tasks.len(), Ordering::Relaxed);
            match result {
                Ok(value) => respond(request, 200, "application/json", value.to_string(), None),
                Err(e) => respond(
                    request,
                    400,
                    "application/json",
                    json!({"error":format!("{e:#}")}).to_string(),
                    None,
                ),
            }
        }
        for (id, result) in workers.finish() {
            if let Err(e) = result {
                eprintln!("{id}: {e:#}");
            }
        }
        activity.store(0, Ordering::Relaxed);
        Ok(())
    }
}
pub fn serve(store: &Store, port: u16) -> Result<()> {
    let dashboard = Dashboard::bind(port)?;
    println!("Vardr dashboard: {}", dashboard.entry_url());
    dashboard.run(store)
}
fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.to_string().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}
pub fn snapshot(store: &Store) -> Result<Value> {
    let inventory = store.jobs()?;
    let mut errors = inventory.errors;
    let mut jobs = Vec::new();
    for job in inventory.jobs {
        let latest = match store.runs(&job.id) {
            Ok(runs) => runs.into_iter().next(),
            Err(e) => {
                errors.insert(format!("history:{}", job.id), format!("{e:#}"));
                None
            }
        };
        jobs.push(json!({"job":job,"latest":latest}));
    }
    let service_path = store.root.join("service.json");
    let service: Option<ServiceState> = if service_path.exists() {
        Some(read_json(&service_path)?)
    } else {
        None
    };
    let desktop_updates_path = store.root.join("desktop-updates.json");
    let desktop_updates: Option<Value> = if desktop_updates_path.exists() {
        Some(read_json(&desktop_updates_path)?)
    } else {
        None
    };
    Ok(
        json!({"desktop_updates":desktop_updates,"jobs":jobs,"errors":errors,"service_running":store.is_locked("service")?,"service":service,"version":env!("CARGO_PKG_VERSION")}),
    )
}
fn handle(store: &Store, request: &mut Request, workers: &mut Workers) -> Result<Value> {
    let path = request.url().to_string();
    let host = header(request, "X-Vardr-Host")
        .unwrap_or("local")
        .to_string();
    let host = host.as_str();
    if request.method() == &Method::Get && path == "/api/hosts" {
        return Ok(serde_json::to_value(crate::hosts::list(store)?)?);
    }
    if request.method() == &Method::Get && path.starts_with("/api/discover/") {
        return Ok(serde_json::to_value(crate::hosts::discover(
            store,
            path.trim_start_matches("/api/discover/"),
        )?)?);
    }
    if request.method() == &Method::Get && path == "/api/jobs" {
        return if host == "local" {
            snapshot(store)
        } else {
            crate::hosts::request(store, host, crate::bridge::Operation::Snapshot)
        };
    }
    if request.method() == &Method::Get && path.starts_with("/api/logs/") {
        let id = path.trim_start_matches("/api/logs/");
        return if host == "local" {
            Ok(json!({"text":runner::logs(store,id,false)?}))
        } else {
            crate::hosts::request(
                store,
                host,
                crate::bridge::Operation::Logs { id: id.into() },
            )
        };
    }
    ensure!(request.method() == &Method::Post, "unknown endpoint");
    ensure!(
        header(request, "Content-Type").is_some_and(|v| v.starts_with("application/json")),
        "JSON content type required"
    );
    let mut bytes = Vec::new();
    request.as_reader().take(524_289).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 524_288, "request too large");
    if path == "/api/job" {
        let job: Job = serde_json::from_slice(&bytes)?;
        return if host == "local" {
            Ok(serde_json::to_value(store.save(job)?)?)
        } else {
            crate::hosts::request(
                store,
                host,
                crate::bridge::Operation::Save { job: Box::new(job) },
            )
        };
    }
    if path == "/api/run" {
        ensure!(
            host == "local",
            "Remote Run now requires the CLI on that Mac; no remote run was started"
        );
        let body: Value = serde_json::from_slice(&bytes)?;
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .context("job ID required")?
            .to_string();
        let job = store.job(&id)?;
        job.validate()?;
        let lock = store.lock(&format!("run-{id}"))?;
        ensure!(
            !workers.tasks.contains_key(&id),
            "this job's previous invocation is still completing"
        );
        let local = store.clone();
        workers.tasks.insert(
            id,
            std::thread::spawn(move || runner::run_job(&local, job, lock)),
        );
        return Ok(json!({"accepted":true}));
    }
    anyhow::bail!("unknown endpoint")
}
fn respond(
    request: Request,
    status: u16,
    content_type: &str,
    body: impl AsRef<[u8]>,
    cookie: Option<&str>,
) {
    let mut response = Response::from_data(body.as_ref()).with_status_code(StatusCode(status));
    for (key, value) in [
        ("Content-Type", content_type),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        (
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; connect-src 'self'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response.add_header(Header::from_bytes(key, value).unwrap());
    }
    if let Some(token) = cookie {
        response.add_header(
            Header::from_bytes(
                "Set-Cookie",
                format!("vardr={token}; HttpOnly; SameSite=Strict; Path=/"),
            )
            .unwrap(),
        );
    }
    let _ = request.respond(response);
}
