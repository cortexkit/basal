use basal_core::run_log::LogRecord;
use basal_core::run_records::{Body,Deferred,Record};

#[test]
fn pre_refund_deferral_record_decodes_with_refund_enabled() {
    // Body::Deferred had enum ordinal 11 and ended with a bool. This tuple
    // encodes that pre-change field order, independently of today's Record.
    let old=postcard::to_allocvec(&("run-old",2u64,11u32,7u64,19i64,"old readiness refusal",17i64,true)).unwrap();
    let record=Record::decode(14,&old).unwrap();
    let Body::Deferred(deferred)=&record.body else{panic!("wrong record kind")};
    assert_eq!(record.run_id,"run-old");assert_eq!(record.generation,2);
    assert_eq!(deferred.position,7);assert_eq!(deferred.retry_not_before,19);
    assert!(deferred.proven_unsent);assert!(deferred.refund_dispatch);assert_eq!(deferred.attempt,None);
    assert_eq!(postcard::to_allocvec(&record).unwrap(),old,"old flag bytes keep their exact meaning");
}

#[test]
fn transport_budget_flags_and_attempt_ordinal_round_trip_without_becoming_refunds() {
    for proven_unsent in [false,true] {
        let record=Record{run_id:"run-new".into(),generation:1,body:Body::Deferred(Deferred{position:0,retry_not_before:20,cause:"transport outcome".into(),audit_at:10,proven_unsent,refund_dispatch:false,attempt:Some(3)})};
        let bytes=postcard::to_allocvec(&record).unwrap();assert_eq!(Record::decode(14,&bytes).unwrap(),record);
    }
    let invalid=postcard::to_allocvec(&("run-bad",1u64,11u32,0u64,19i64,"bad flags",17i64,8u8)).unwrap();
    assert!(Record::decode(14,&invalid).is_err());
}

#[test]
fn older_human_readable_call_detail_defaults_to_refunding() {
    let old=serde_json::json!({"position":0,"retry_not_before":19,"cause":"ready later","audit_at":17,"proven_unsent":true});
    let deferred:Deferred=serde_json::from_value(old).unwrap();
    assert!(deferred.refund_dispatch);assert_eq!(deferred.attempt,None);
    let value=serde_json::to_value(&deferred).unwrap();assert_eq!(value["proven_unsent"],true);assert_eq!(value["refund_dispatch"],true);
}
