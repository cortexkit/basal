use super::*;
use std::sync::mpsc;

#[derive(Default)]
struct Wire {
    calls: Arc<Mutex<Vec<&'static str>>>,
    page: Mutex<Value>,
    fail_ack: AtomicBool,
}

impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("unexpected catalog")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unexpected tool")
    }
    fn management(&self, _: &str, op: &str, _: Value) -> Result<Value, WireError> {
        match op {
            "elicitation.answers" => {
                self.calls.lock().unwrap().push("answers");
                Ok(self.page.lock().unwrap().clone())
            }
            "elicitation.ack" => {
                self.calls.lock().unwrap().push("ack");
                Ok(json!({"ok": !self.fail_ack.swap(false, Ordering::AcqRel)}))
            }
            "elicitation.request" => Ok(json!({"elicitation_id":"raised"})),
            _ => panic!("unexpected management call {op}"),
        }
    }
}

struct Sink {
    open: AtomicBool,
    calls: Arc<Mutex<Vec<&'static str>>>,
    inventory: mpsc::Sender<()>,
}

impl DecisionSink for Sink {
    fn has_open_cards(&self) -> Result<bool, crate::SinkError> {
        if !self.open.load(Ordering::Acquire) {
            let _ = self.inventory.send(());
        }
        Ok(self.open.load(Ordering::Acquire))
    }
    fn decide(&self, _: &DecisionEvent) -> Result<(), crate::SinkError> {
        self.calls.lock().unwrap().push("apply");
        self.open.store(false, Ordering::Release);
        Ok(())
    }
    fn answer(&self, _: &DecisionAnswer) -> Result<(), crate::SinkError> {
        panic!("unexpected answer")
    }
}

fn setup(open: bool, answered: bool) -> (Arc<Wire>, Arc<Sink>, mpsc::Receiver<()>) {
    let wire = Arc::new(Wire::default());
    *wire.page.lock().unwrap() = if answered {
        json!({"cursor":1,"records":[{"elicitation_id":"old", "state":"answered", "answered_choice_id":"approve", "flow_install":{"flow_id":"flow","version":1,"code_hash":"a".repeat(64)}}]})
    } else {
        json!({"cursor":0,"records":[]})
    };
    let (tx, rx) = mpsc::channel();
    let sink = Arc::new(Sink {
        open: AtomicBool::new(open),
        calls: wire.calls.clone(),
        inventory: tx,
    });
    (wire, sink, rx)
}

#[test]
fn idle_consent_makes_zero_rpcs() {
    let (wire, sink, _) = setup(false, false);
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink);
    // Five former polling opportunities, with no sleeps or elapsed-time claim.
    for _ in 0..5 {
        consent.poll_once().unwrap();
    }
    assert_eq!(wire.calls.lock().unwrap().len(), 0);
}

#[test]
fn startup_reconciles_closed_unacked_cards_then_stays_idle() {
    let (wire, sink, inventory) = setup(false, true);
    let consent = CoreConsent::new(wire.clone()).with_polling();
    consent.attach(sink);
    // Inventory is consulted only after the startup page is applied and acked.
    inventory.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(*wire.calls.lock().unwrap(), ["answers", "apply", "ack"]);
    for _ in 0..5 {
        consent.poll_once().unwrap();
    }
    assert_eq!(*wire.calls.lock().unwrap(), ["answers", "apply", "ack"]);
}

#[test]
fn failed_ack_is_retried_after_the_last_card_closes() {
    let (wire, sink, _) = setup(true, true);
    wire.fail_ack.store(true, Ordering::Release);
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink);
    assert!(consent.poll_once().is_err());
    consent.poll_once().unwrap();
    consent.poll_once().unwrap();
    assert_eq!(
        *wire.calls.lock().unwrap(),
        ["answers", "apply", "ack", "answers", "apply", "ack"]
    );
}

#[test]
fn open_cards_back_off_to_five_seconds_and_a_raise_resets_fast_polling() {
    let mut delay = POLL_MIN;
    let mut delays = vec![delay.as_millis()];
    for _ in 0..6 {
        delay = next_poll_delay(delay, false);
        delays.push(delay.as_millis());
    }
    assert_eq!(delays, [250, 500, 1000, 2000, 4000, 5000, 5000]);
    assert_eq!(next_poll_delay(delay, true), POLL_MIN);
    let (wire, sink, _) = setup(true, false);
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink);
    consent.poll_once().unwrap();
    assert_eq!(*wire.calls.lock().unwrap(), ["answers"]);
    let seen = consent.state.wake.lock().unwrap().0;
    consent.raise(&InstallCard {
        card_id: "card".into(), flow_id: "flow".into(), version: 1,
        fields: json!({"code_hash":"a".repeat(64), "code":{"script":"return 1;", "manifest":"{}"}, "purpose":"Polling test", "wire_author":{"operator":true}}),
    }).unwrap();
    assert_ne!(consent.state.wake.lock().unwrap().0, seen);
}
