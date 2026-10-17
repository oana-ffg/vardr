use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Dashboard(Child);
impl Drop for Dashboard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            let _ = self.0.wait();
        }
    }
}

#[test]
fn dashboard_requires_private_entry_and_rejects_duplicate_runs() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vardr"))
        .arg("--home")
        .arg(temp.path().join("data"))
        .args(["ui", "--port", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("Vardr dashboard: ")
        .unwrap()
        .to_string();
    let token = url.split("token=").nth(1).unwrap().to_string();
    let base = url.split("/?").next().unwrap();
    let mut dashboard = Dashboard(child);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert_eq!(client.get(format!("{base}/")).send().unwrap().status(), 403);
    let page = client.get(&url).send().unwrap();
    assert!(page.status().is_success());
    assert!(
        page.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("HttpOnly; SameSite=Strict")
    );
    assert_eq!(
        client
            .get(format!("{base}/api/jobs"))
            .send()
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(format!("{base}/api/jobs"))
            .header("Host", "attacker.example")
            .header("X-Vardr-Token", &token)
            .send()
            .unwrap()
            .status(),
        403
    );
    for endpoint in ["/api/hosts", "/api/discover/absent"] {
        assert_eq!(
            client
                .get(format!("{base}{endpoint}"))
                .send()
                .unwrap()
                .status(),
            403
        );
    }
    let hosts: Value = client
        .get(format!("{base}/api/hosts"))
        .header("X-Vardr-Token", &token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(hosts, json!([]));
    let unavailable = client
        .get(format!("{base}/api/discover/absent"))
        .header("X-Vardr-Token", &token)
        .send()
        .unwrap();
    assert_eq!(unavailable.status(), 400);
    assert!(
        unavailable.json::<Value>().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("unknown host")
    );
    let job = json!({"id":"ui-test","name":"UI test","purpose":"Test authentication and supervised shutdown","cwd":temp.path(),"target":{"type":"command","argv":["/bin/sleep","30"]},"schedule":{"type":"manual"}});
    let post = |path: &str, body: Value| {
        client
            .post(format!("{base}{path}"))
            .header("X-Vardr-Token", &token)
            .json(&body)
            .send()
            .unwrap()
    };
    let rejected = client
        .post(format!("{base}/api/job"))
        .header("X-Vardr-Token", &token)
        .header("X-Vardr-Host", "absent")
        .json(&job)
        .send()
        .unwrap();
    assert_eq!(rejected.status(), 400);
    let local: Value = client
        .get(format!("{base}/api/jobs"))
        .header("X-Vardr-Token", &token)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(local["jobs"].as_array().unwrap().is_empty());
    assert!(post("/api/job", job).status().is_success());
    assert!(
        post("/api/run", json!({"id":"ui-test"}))
            .status()
            .is_success()
    );
    assert_eq!(post("/api/run", json!({"id":"ui-test"})).status(), 400);
    let start = Instant::now();
    unsafe {
        libc::kill(dashboard.0.id() as i32, libc::SIGHUP);
    }
    assert!(dashboard.0.wait().unwrap().success());
    assert!(start.elapsed() < Duration::from_secs(3));
    let history = Command::new(env!("CARGO_BIN_EXE_vardr"))
        .arg("--home")
        .arg(temp.path().join("data"))
        .args(["history", "ui-test"])
        .output()
        .unwrap();
    let history: Vec<Value> = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(history[0]["status"], "interrupted");
}
