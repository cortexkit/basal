//! Call-state snapshots keep their original binary field order. The reservation
//! bool's byte also carries the per-call scope-cut bit, so older 0/1 snapshots
//! decode without consuming a field from the next call or snapshot counter.
use super::{state::CallState, types::*};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Deserialize)]
#[serde(bound(deserialize = "F: Deserialize<'de> + Default"))]
struct Wire<F> {
    identity: CallIdentity,
    #[serde(default)]
    pre_accounted_attempts: u64,
    #[serde(default)]
    pre_accounted_reservation: F,
    intent: Option<Intent>,
    prior_intents: Vec<Intent>,
    sync: Option<Sync>,
    accepted: Option<String>,
    arrival: Option<Arrival>,
    delivery_order: Option<u64>,
    deferred: Option<Deferred>,
    prior_deferrals: Vec<Deferred>,
    unknown: Option<String>,
    resolutions: Vec<Resolution>,
}
impl<F> Wire<F> {
    fn state(
        self,
        reserved: bool,
        scope_cut: bool,
        usage_report: Option<UsageReport>,
    ) -> CallState {
        CallState {
            identity: self.identity,
            pre_accounted_attempts: self.pre_accounted_attempts,
            pre_accounted_reservation: reserved,
            scope_cut,
            usage_report,
            intent: self.intent,
            prior_intents: self.prior_intents,
            sync: self.sync,
            accepted: self.accepted,
            arrival: self.arrival,
            delivery_order: self.delivery_order,
            deferred: self.deferred,
            prior_deferrals: self.prior_deferrals,
            unknown: self.unknown,
            resolutions: self.resolutions,
        }
    }
}
impl Serialize for CallState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let human = serializer.is_human_readable();
        let mut value = serializer.serialize_struct("CallState", if human { 15 } else { 13 })?;
        value.serialize_field("identity", &self.identity)?;
        value.serialize_field("pre_accounted_attempts", &self.pre_accounted_attempts)?;
        if human {
            value.serialize_field("pre_accounted_reservation", &self.pre_accounted_reservation)?;
        } else {
            let flags = BinaryFlags {
                bits: u8::from(self.pre_accounted_reservation) | (u8::from(self.scope_cut) << 1),
                report: self.usage_report,
            };
            value.serialize_field("pre_accounted_reservation", &flags)?;
        }
        value.serialize_field("intent", &self.intent)?;
        value.serialize_field("prior_intents", &self.prior_intents)?;
        value.serialize_field("sync", &self.sync)?;
        value.serialize_field("accepted", &self.accepted)?;
        value.serialize_field("arrival", &self.arrival)?;
        value.serialize_field("delivery_order", &self.delivery_order)?;
        value.serialize_field("deferred", &self.deferred)?;
        value.serialize_field("prior_deferrals", &self.prior_deferrals)?;
        value.serialize_field("unknown", &self.unknown)?;
        value.serialize_field("resolutions", &self.resolutions)?;
        if human {
            value.serialize_field("scope_cut", &self.scope_cut)?;
            value.serialize_field("usage_report", &self.usage_report)?;
        }
        value.end()
    }
}
impl<'de> Deserialize<'de> for CallState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            #[derive(Deserialize)]
            struct Human {
                #[serde(flatten)]
                state: Wire<bool>,
                #[serde(default)]
                scope_cut: bool,
                #[serde(default)]
                usage_report: Option<UsageReport>,
            }
            let value = Human::deserialize(deserializer)?;
            let reserved = value.state.pre_accounted_reservation;
            Ok(value
                .state
                .state(reserved, value.scope_cut, value.usage_report))
        } else {
            let value = Wire::<BinaryFlags>::deserialize(deserializer)?;
            let flags = value.pre_accounted_reservation;
            Ok(value.state(flags.bits & 1 != 0, flags.bits & 2 != 0, flags.report))
        }
    }
}

// A pre-change reservation bool occupied exactly one byte. Bits 0/1 retain its
// reservation/scope meanings; bit 2 says a full measurement follows that byte.
// No report byte is consumed for an older call or for a call without a report.
#[derive(Default, Clone, Copy)]
struct BinaryFlags {
    bits: u8,
    report: Option<UsageReport>,
}
impl Serialize for BinaryFlags {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut value = serializer.serialize_tuple(if self.report.is_some() { 2 } else { 1 })?;
        value.serialize_element(&(self.bits | if self.report.is_some() { 4 } else { 0 }))?;
        if let Some(report) = self.report {
            value.serialize_element(&report)?;
        }
        value.end()
    }
}
impl<'de> Deserialize<'de> for BinaryFlags {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Flags;
        impl<'de> serde::de::Visitor<'de> for Flags {
            type Value = BinaryFlags;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("binary call flags and optional measurement")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<BinaryFlags, A::Error> {
                use serde::de::Error;
                let bits: u8 = seq
                    .next_element()?
                    .ok_or_else(|| Error::missing_field("call flags"))?;
                if bits > 7 {
                    return Err(Error::custom("invalid call-state flags"));
                }
                let report = if bits & 4 != 0 {
                    Some(
                        seq.next_element()?
                            .ok_or_else(|| Error::missing_field("measurement"))?,
                    )
                } else {
                    None
                };
                Ok(BinaryFlags {
                    bits: bits & 3,
                    report,
                })
            }
        }
        deserializer.deserialize_tuple(2, Flags)
    }
}
