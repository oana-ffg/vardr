//! One bounded SSH request. The transport is authenticated by the user's SSH keys.
use crate::{model::Job, runner, store::Store, ui};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol: u32,
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Snapshot,
    Save { job: Box<Job> },
    Logs { id: String },
}
pub fn respond(store: &Store, request: Request) -> Value {
    let result = (|| -> Result<Value> {
        ensure!(request.protocol == 1, "unsupported bridge protocol");
        match request.operation {
            Operation::Snapshot => ui::snapshot(store),
            Operation::Save { job } => Ok(serde_json::to_value(store.save(*job)?)?),
            Operation::Logs { id } => Ok(json!({"text":runner::logs(store, &id, false)?})),
        }
    })();
    match result {
        Ok(result) => json!({"protocol":1,"result":result}),
        Err(error) => json!({"protocol":1,"error":format!("{error:#}")}),
    }
}
