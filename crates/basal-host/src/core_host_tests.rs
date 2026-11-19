use super::*;
use crate::{Sent, TransportError};
use basal_proto::Settlement;

struct SinkTransport(Result<Value, WireError>);

impl Transport for SinkTransport {
    fn catalog(&self) -> Result<Value, WireError> {
        self.0.clone()
    }

    fn management(&self, _: &str, _: &str, _: Value) -> Result<Value, WireError> {
        panic!("the capability check must stop the sink before dispatch")
    }

    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        unreachable!()
    }
}

#[test]
fn a_keyed_sink_retry_stays_unsent_when_the_catalog_read_fails() {
    let request = CallRequest {
        flow_id: "f".into(),
        flow_version: Some(1),
        run_id: "r".into(),
        position: 3,
        kind: CallKind::Primitive(Primitive::SinkRoomPost),
        args: JsonText::new(json!({"room":"rm_1","text":"hello"}).to_string()).unwrap(),
        idempotency_key: "k3".into(),
        attempt: 2,
    };
    for error in [
        WireError::NeverSent("daemon connection is not open".into()),
        WireError::TimedOut("catalog read timed out".into()),
        WireError::Unreadable("malformed catalog reply".into()),
    ] {
        let host = CoreHost::new(Arc::new(SinkTransport(Err(error.clone()))));
        let result = host.dispatch_classified(&request, host.classify(&request.kind));
        assert_eq!(
            result.unwrap_err(),
            TransportError::Unavailable {
                sent: Sent::Never,
                detail: format!("{error:?}"),
            }
        );
    }

    let host = CoreHost::new(Arc::new(SinkTransport(Ok(json!({"modules": []})))));
    let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
        panic!("a readable catalog without the capability must reject the sink");
    };
    assert_eq!(outcome.settlement, Settlement::Rejected);
    let refusal: Value = serde_json::from_str(outcome.value.as_str()).unwrap();
    assert_eq!(refusal["code"], "room_post_unsupported");
}
