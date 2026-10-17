use crate::{
    model::{Job, ServiceState},
    process::STOP,
    runner::{self, Workers},
    store::{Store, read_json},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{io::Read, sync::atomic::Ordering, time::Duration};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub fn serve(store: &Store, port: u16) -> Result<()> {
    let server = Server::http(("127.0.0.1", port)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("no local address")?
        .port();
    let token = uuid::Uuid::new_v4().to_string();
    println!("Vardr dashboard: http://127.0.0.1:{port}/?token={token}");
    let mut workers = Workers::default();
    while !STOP.load(Ordering::Relaxed) {
        for (id, result) in workers.reap() {
            if let Err(e) = result {
                eprintln!("{id}: {e:#}");
            }
        }
        let Some(mut request) = server.recv_timeout(Duration::from_millis(250))? else {
            continue;
        };
        let valid_host = header(&request, "Host")
            .is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
        if !valid_host {
            respond(request, 403, "text/plain", "Invalid host".into(), None);
            continue;
        }
        let url = request.url().to_owned();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        if request.method() == &Method::Get && path == "/" {
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
                    "Open the private dashboard URL printed by `vardr ui`.".into(),
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
    Ok(())
}
fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.to_string().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}
fn handle(store: &Store, request: &mut Request, workers: &mut Workers) -> Result<Value> {
    let path = request.url().to_string();
    if request.method() == &Method::Get && path == "/api/jobs" {
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
        return Ok(
            json!({"jobs":jobs,"errors":errors,"service_running":store.is_locked("service")?,"service":service,"version":env!("CARGO_PKG_VERSION")}),
        );
    }
    if request.method() == &Method::Get && path.starts_with("/api/logs/") {
        return Ok(
            json!({"text":runner::logs(store,path.trim_start_matches("/api/logs/"),false)?}),
        );
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
        return Ok(serde_json::to_value(store.save(job)?)?);
    }
    if path == "/api/run" {
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
fn respond(request: Request, status: u16, content_type: &str, body: String, cookie: Option<&str>) {
    let mut response = Response::from_string(body).with_status_code(StatusCode(status));
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
