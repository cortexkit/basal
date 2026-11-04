//! Postcard adapters for the worker's protocol types, without changing that protocol.

use basal_proto::{ArgsDigest, CallKind, JsonText, Primitive, Settlement};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

use crate::model::StoredClass;

pub mod json {
    use super::*;
    pub fn serialize<S: Serializer>(value: &JsonText, serializer: S) -> Result<S::Ok, S::Error> {
        value.as_str().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<JsonText, D::Error> {
        JsonText::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub mod digest {
    use super::*;
    pub fn serialize<S: Serializer>(value: &ArgsDigest, serializer: S) -> Result<S::Ok, S::Error> {
        value.0.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ArgsDigest, D::Error> {
        Ok(ArgsDigest(<[u8; 32]>::deserialize(deserializer)?))
    }
}

pub mod kind {
    use super::*;
    // Keep OpFor and Op as distinct serialized variants so replay can distinguish them.
    #[derive(Serialize, Deserialize)]
    enum Kind {
        Op(String, String),
        OpFor(String, String),
        Primitive(u8),
        Tool(String),
    }
    pub fn serialize<S: Serializer>(value: &CallKind, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            CallKind::Op { module, op } => Kind::Op(module.clone(), op.clone()),
            CallKind::OpFor { module, op } => Kind::OpFor(module.clone(), op.clone()),
            CallKind::Primitive(p) => Kind::Primitive(p.code()),
            CallKind::Tool { name } => Kind::Tool(name.clone()),
        }
        .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<CallKind, D::Error> {
        Ok(match Kind::deserialize(deserializer)? {
            Kind::Op(module, op) => CallKind::Op { module, op },
            Kind::OpFor(module, op) => CallKind::OpFor { module, op },
            Kind::Primitive(code) => CallKind::Primitive(
                Primitive::from_code(code).ok_or_else(|| D::Error::custom("unknown primitive"))?,
            ),
            Kind::Tool(name) => CallKind::Tool { name },
        })
    }
}

pub mod class {
    use super::*;
    pub fn serialize<S: Serializer>(value: &StoredClass, serializer: S) -> Result<S::Ok, S::Error> {
        value.as_str().serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<StoredClass, D::Error> {
        StoredClass::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub mod settlement {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Settlement, serializer: S) -> Result<S::Ok, S::Error> {
        matches!(value, Settlement::Fulfilled).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Settlement, D::Error> {
        Ok(if bool::deserialize(deserializer)? {
            Settlement::Fulfilled
        } else {
            Settlement::Rejected
        })
    }
}
