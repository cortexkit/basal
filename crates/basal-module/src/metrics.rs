//! The module's own counters, for the operator's view of basal: worker kills and
//! respawns, and what replay costs. They live in memory and start from zero
//! at each process start; the durable figures (run ages, token windows,
//! flow health) are read from the store when asked for.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

#[derive(Debug, Default)]
pub struct Metrics {
    pub workers_spawned: AtomicU64,
    pub spawn_failures: AtomicU64,
    /// Workers the parent killed: a broken protocol, a budget or deadline
    /// breach, an activation that lost its run to another activation, or
    /// any other activation that did not end cleanly.
    pub workers_killed: AtomicU64,
    /// Workers found dead that nobody killed.
    pub workers_crashed: AtomicU64,
    /// Workers ended on purpose after their idle period or activation count.
    pub workers_retired: AtomicU64,
    /// Spawns that replaced a killed or crashed worker.
    pub workers_respawned: AtomicU64,
    /// Activations that had to wait for a worker to be spawned because no
    /// warm one was ready.
    pub spawned_on_demand: AtomicU64,
    pub activations: AtomicU64,
    /// Journal rows already recorded when an activation started: the calls
    /// it replayed (or skipped) before doing anything new.
    pub replayed_calls: AtomicU64,
    /// Wall time inside activations, replay included.
    pub activation_micros: AtomicU64,
    pub activation_micros_max: AtomicU64,
    pub dry_runs: AtomicU64,
}

fn load(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

impl Metrics {
    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one finished activation.
    pub fn activation(&self, replayed: u64, micros: u64) {
        Self::bump(&self.activations);
        self.replayed_calls.fetch_add(replayed, Ordering::Relaxed);
        self.activation_micros.fetch_add(micros, Ordering::Relaxed);
        self.activation_micros_max
            .fetch_max(micros, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> Value {
        let activations = load(&self.activations);
        let micros = load(&self.activation_micros);
        json!({
            "workers": {
                "spawned": load(&self.workers_spawned),
                "spawn_failures": load(&self.spawn_failures),
                "killed": load(&self.workers_killed),
                "crashed": load(&self.workers_crashed),
                "retired": load(&self.workers_retired),
                "respawned": load(&self.workers_respawned),
                "spawned_on_demand": load(&self.spawned_on_demand),
            },
            "replay": {
                "activations": activations,
                "replayed_calls": load(&self.replayed_calls),
                "activation_micros_total": micros,
                "activation_micros_mean": micros.checked_div(activations).unwrap_or(0),
                "activation_micros_max": load(&self.activation_micros_max),
            },
            "dry_runs": load(&self.dry_runs),
        })
    }
}
