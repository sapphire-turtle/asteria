//! What the rating queue's tests share: recording the calls a stub got, and `ops.apply` answers.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::StubRequest;

/// The RPC calls a stub received: method and params, in order.
#[derive(Clone, Default)]
pub struct Calls(Arc<Mutex<Vec<(String, Value)>>>);

impl Calls {
    pub fn record(&self, req: &StubRequest) -> (u64, String, Value) {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let (id, method) = req.rpc().unwrap();
        let params = body["params"].clone();
        self.0
            .lock()
            .unwrap()
            .push((method.clone(), params.clone()));
        (id, method, params)
    }

    pub fn all(&self) -> Vec<(String, Value)> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self, method: &str) -> usize {
        self.all().iter().filter(|(m, _)| m == method).count()
    }

    /// The ops of each `ops.apply` call.
    pub fn batches(&self) -> Vec<Vec<Value>> {
        self.all()
            .into_iter()
            .filter(|(m, _)| m == "ops.apply")
            .map(|(_, p)| p["ops"].as_array().unwrap().clone())
            .collect()
    }
}

pub fn media_item(id: &str, rating: i64) -> Value {
    json!({
        "id": id,
        "path": format!("{id}.jpg"),
        "kind": "image",
        "size": 1,
        "modified": "2026-11-16T00:00:00Z",
        "rating": rating,
        "subjects": [],
        "hierarchical_subjects": [],
        "description": null,
        "label": null,
        "content_hash": null,
        "read_only_reason": null,
    })
}

/// `applied` for every op of an `ops.apply`, the item rated as asked.
pub fn all_applied(params: &Value) -> Value {
    let results: Vec<Value> = params["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| {
            json!({
                "id": op["id"],
                "status": "applied",
                "item": media_item(op["item"].as_str().unwrap(), op["value"].as_i64().unwrap()),
            })
        })
        .collect();
    json!({ "results": results })
}
