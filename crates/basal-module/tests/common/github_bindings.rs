use std::sync::Mutex;

use basal_host::HostOutcome;
use basal_proto::JsonText;
use serde_json::{Value, json};

pub const UNBOUND: &[(&str, &str)] = &[
    ("cortexkit/subconscious-design", "agent_2bfd6927602f7977"),
    ("cortexkit/cingulate", "agent_2bfd6927602f7977"),
    ("cortexkit/broca-private", "agent_65e9fe998e14b47d"),
    ("cortexkit/motor-private", "agent_ef98bc82ef005a63"),
];

// Synthetic bindings, not a snapshot of Plexus's deployed repository inventory.
pub fn snapshot() -> Value {
    serde_json::from_str(include_str!("../data/github-read-bindings.json")).unwrap()
}

pub fn routing() -> Vec<(String, String, String)> {
    let snapshot = snapshot();
    let bindings = snapshot["bindings"].as_array().unwrap();
    let mut routing: Vec<_> = bindings
        .iter()
        .map(|binding| {
            (
                binding["repository"].as_str().unwrap().to_owned(),
                binding["agent_id"].as_str().unwrap().to_owned(),
                binding["app_handle_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for &(repo, owner) in UNBOUND {
        let bot = bindings
            .iter()
            .find(|binding| binding["agent_id"] == owner)
            .unwrap()["app_handle_id"]
            .as_str()
            .unwrap();
        routing.push((repo.to_owned(), owner.to_owned(), bot.to_owned()));
    }
    routing
}

pub fn reply(snapshot: Value) -> HostOutcome {
    HostOutcome::fulfilled(JsonText::new(json!({"result": snapshot}).to_string()).unwrap())
}

pub fn unavailable() -> HostOutcome {
    HostOutcome::fulfilled(
        JsonText::new(json!({"error":{"code":"store_failure","retryable":true}}).to_string())
            .unwrap(),
    )
}

#[derive(Default)]
pub struct MockBindings {
    pub reads: Mutex<Vec<String>>,
    pub outcome: Mutex<Option<HostOutcome>>,
}

impl MockBindings {
    pub fn set(&self, outcome: HostOutcome) {
        *self.outcome.lock().unwrap() = Some(outcome);
    }

    pub fn answer(&self, args: &str) -> HostOutcome {
        assert_eq!(args, r#"{"action":"github.bindings.list"}"#);
        self.reads.lock().unwrap().push(args.to_owned());
        self.outcome
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| reply(snapshot()))
    }
}

pub fn cache(f: &super::Fixture, flow: &str) -> Value {
    let stored: String = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT value FROM kv WHERE flow_id=?1 AND key='github-bindings'",
                [flow],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    serde_json::from_str(&stored).unwrap()
}

#[test]
fn fixture_has_the_documented_bindings_shape() {
    let snapshot = snapshot();
    assert_eq!(snapshot["generation"].as_u64(), Some(38));
    let bindings = snapshot["bindings"].as_array().unwrap();
    assert!(!bindings.is_empty());
    let mut previous = "";
    for binding in bindings {
        assert_eq!(binding.as_object().unwrap().len(), 3);
        assert!(binding["agent_id"].as_str().unwrap().starts_with("agent_"));
        assert!(!binding["app_handle_id"].as_str().unwrap().is_empty());
        let repo = binding["repository"].as_str().unwrap();
        assert_eq!(repo, repo.to_ascii_lowercase());
        assert!(repo.starts_with("cortexkit/"));
        assert!(previous < repo);
        assert!(!UNBOUND.iter().any(|&(unbound, _)| unbound == repo));
        previous = repo;
    }
}
