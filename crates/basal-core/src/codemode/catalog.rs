//! A codemode catalog's digest and its tool input schemas.
//!
//! The catalog is the list of tools a codemode program may call, each entry
//! `{name, input_schema, module, op}`. Its digest identifies the catalog a
//! run was admitted with, independently of how the sender ordered entries,
//! keys or spelled numbers. Each entry's `input_schema` is compiled once at
//! admission and checks every input the program sends to that tool.

use std::fmt;

use jsonschema::{Retrieve, Uri, Validator};
use serde_json::Value;

use super::canonical::to_canonical_json;

/// A catalog that cannot be digested: it is not an array, or an entry has
/// no string `name` to sort by. Admission refuses such a catalog before it
/// asks for a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestError;

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a catalog is an array of entries that each have a string name")
    }
}

impl std::error::Error for DigestError {}

/// The catalog's digest: lowercase hex BLAKE3-256 of the RFC 8785 canonical
/// JSON of the catalog array, with its entries sorted by `name`.
///
/// Names sort by UTF-16 code units, the order canonical JSON uses for
/// member names. Admission only accepts ASCII names, for which this is plain
/// byte order.
pub fn catalog_digest(catalog: &Value) -> Result<String, DigestError> {
    let entries = catalog.as_array().ok_or(DigestError)?;
    let mut named = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or(DigestError)?;
        named.push((name, entry));
    }
    named.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let sorted = Value::Array(named.into_iter().map(|(_, e)| e.clone()).collect());
    Ok(blake3::hash(to_canonical_json(&sorted).as_bytes())
        .to_hex()
        .to_string())
}

/// Why a tool's input schema was not admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// A `$ref` or `$dynamicRef` that does not point into the schema's own
    /// document. Carries the reference as written.
    ExternalRef(String),
    /// A `$schema` naming a dialect other than draft 2020-12. The compiler
    /// would honour it and check inputs by that draft's rules instead.
    OtherDraft(String),
    /// The schema is not valid draft 2020-12, or a reference in it does not
    /// resolve within the document.
    Invalid(String),
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExternalRef(r) => write!(
                f,
                "the schema reference {r:?} is not a same-document fragment (\"#\" or \"#/...\")"
            ),
            Self::OtherDraft(s) => write!(
                f,
                "the schema declares {s:?}; tool input schemas are JSON Schema draft 2020-12"
            ),
            Self::Invalid(e) => write!(f, "the schema does not compile: {e}"),
        }
    }
}

impl std::error::Error for SchemaError {}

/// Compiles a tool's input schema as JSON Schema draft 2020-12.
///
/// A schema may refer only to itself: every `$ref` and `$dynamicRef` must be
/// `#` or start with `#/`. That rule is checked on the schema text before
/// compiling, so it holds even for a reference the compiler could resolve on
/// its own (an absolute URI naming an `$id` inside the document, or an
/// `$anchor`). A `$schema` may only name draft 2020-12, because the compiler
/// would otherwise switch that (sub)schema to the draft it names. Both
/// checks look at every such member with a string value anywhere in the
/// document, including inside `const`, `enum` or examples; refusing those
/// rare harmless cases keeps the rules simple to audit.
///
/// The compiler never fetches anything: the crate is built without its HTTP
/// and file loaders, and the retriever refuses every URI. Format keywords
/// are annotations only, never assertions.
pub fn compile_input_schema(schema: &Value) -> Result<Validator, SchemaError> {
    check_members(schema)?;
    options()
        .build(schema)
        .map_err(|e| SchemaError::Invalid(e.to_string()))
}

fn options() -> jsonschema::ValidationOptions<'static> {
    jsonschema::draft202012::options()
        .should_validate_formats(false)
        .with_retriever(NoRetrieval)
}

/// Rejects the first reference that leaves the document and the first
/// `$schema` naming another draft. Walks with an explicit stack, so a deeply
/// nested schema cannot exhaust the thread's stack.
fn check_members(schema: &Value) -> Result<(), SchemaError> {
    let mut stack = vec![schema];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(map) => {
                for (key, member) in map {
                    if let Value::String(s) = member {
                        if (key == "$ref" || key == "$dynamicRef") && !is_same_document(s) {
                            return Err(SchemaError::ExternalRef(s.clone()));
                        }
                        if key == "$schema" && !is_draft_2020_12(s) {
                            return Err(SchemaError::OtherDraft(s.clone()));
                        }
                    }
                    stack.push(member);
                }
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    Ok(())
}

fn is_same_document(reference: &str) -> bool {
    reference == "#" || reference.starts_with("#/")
}

fn is_draft_2020_12(dialect: &str) -> bool {
    dialect.strip_suffix('#').unwrap_or(dialect) == "https://json-schema.org/draft/2020-12/schema"
}

/// Refuses every external resource, whatever its scheme.
struct NoRetrieval;

impl Retrieve for NoRetrieval {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("codemode schemas cannot load {}", uri.as_str()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The oracle: canonical text written out by hand, hashed with BLAKE3
    // directly, never through `catalog_digest` or the canonicalizer.
    fn oracle(canonical: &str) -> String {
        blake3::hash(canonical.as_bytes()).to_hex().to_string()
    }

    const CANONICAL: &str = concat!(
        r#"[{"input_schema":{"properties":{"q":{"type":"string"}},"type":"object"},"#,
        r#""module":"search","name":"find","op":"search.find"},"#,
        r#"{"input_schema":{"maximum":1e+21,"minimum":1,"type":"number"},"#,
        r#""module":"notes","name":"read","op":"notes.read"}]"#
    );

    #[test]
    fn the_digest_is_blake3_of_canonical_json_with_entries_sorted_by_name() {
        let catalog = json!([
            {"name": "find", "module": "search", "op": "search.find",
             "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}},
            {"name": "read", "module": "notes", "op": "notes.read",
             "input_schema": {"type": "number", "minimum": 1.0, "maximum": 1e21}}
        ]);
        let digest = catalog_digest(&catalog).unwrap();
        assert_eq!(digest, oracle(CANONICAL));
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn reordered_entries_keys_and_number_spellings_share_one_digest() {
        let reordered: Value = serde_json::from_str(
            r#"[
                {"op": "notes.read", "input_schema": {"minimum": 1, "maximum": 1000000000000000000000, "type": "number"},
                 "name": "read", "module": "notes"},
                {"module": "search", "input_schema": {"properties": {"q": {"type": "string"}}, "type": "object"},
                 "op": "search.find", "name": "find"}
            ]"#,
        )
        .unwrap();
        assert_eq!(catalog_digest(&reordered).unwrap(), oracle(CANONICAL));
    }

    #[test]
    fn a_number_and_its_string_spelling_differ() {
        let number = json!([{"name": "a", "module": "m", "op": "o", "input_schema": {"const": 1}}]);
        let string =
            json!([{"name": "a", "module": "m", "op": "o", "input_schema": {"const": "1"}}]);
        assert_eq!(
            catalog_digest(&number).unwrap(),
            oracle(r#"[{"input_schema":{"const":1},"module":"m","name":"a","op":"o"}]"#)
        );
        assert_eq!(
            catalog_digest(&string).unwrap(),
            oracle(r#"[{"input_schema":{"const":"1"},"module":"m","name":"a","op":"o"}]"#)
        );
    }

    #[test]
    fn non_ascii_text_and_control_characters_are_canonical() {
        let catalog = json!([{"name": "a", "module": "m", "op": "o", "input_schema": {"description": "caf\u{e9}\u{1}\n"}}]);
        assert_eq!(
            catalog_digest(&catalog).unwrap(),
            oracle(
                "[{\"input_schema\":{\"description\":\"caf\u{e9}\\u0001\\n\"},\"module\":\"m\",\"name\":\"a\",\"op\":\"o\"}]"
            )
        );
    }

    #[test]
    fn an_empty_catalog_has_a_digest() {
        assert_eq!(catalog_digest(&json!([])).unwrap(), oracle("[]"));
    }

    #[test]
    fn a_catalog_without_names_cannot_be_digested() {
        assert_eq!(catalog_digest(&json!({})), Err(DigestError));
        assert_eq!(catalog_digest(&json!([{"name": 1}])), Err(DigestError));
        assert_eq!(catalog_digest(&json!([{}])), Err(DigestError));
    }

    #[test]
    fn same_document_references_compile() {
        let schema = json!({
            "$defs": {"id": {"type": "string", "minLength": 1}},
            "type": "object",
            "properties": {"id": {"$ref": "#/$defs/id"}, "self": {"$ref": "#"}}
        });
        let validator = compile_input_schema(&schema).unwrap();
        assert!(validator.is_valid(&json!({"id": "x"})));
        assert!(!validator.is_valid(&json!({"id": ""})));
    }

    #[test]
    fn references_outside_the_document_are_refused_even_when_they_would_compile() {
        let cases = [
            json!({"$ref": "https://example.com/schema.json"}),
            json!({"$ref": "file:///etc/passwd"}),
            json!({"properties": {"a": {"$ref": "other.json#/x"}}}),
            // Each of these resolves inside the document, so the compiler
            // alone would accept it; the rule still refuses it.
            json!({"$id": "https://example.com/s", "$defs": {"a": {"type": "string"}},
                   "$ref": "https://example.com/s#/$defs/a"}),
            json!({"$defs": {"a": {"$anchor": "a", "type": "string"}}, "$ref": "#a"}),
            json!({"$dynamicRef": "https://example.com/meta"}),
        ];
        for schema in cases {
            assert!(
                matches!(
                    compile_input_schema(&schema),
                    Err(SchemaError::ExternalRef(_))
                ),
                "{schema}"
            );
        }
    }

    #[test]
    fn the_compiler_alone_resolves_in_document_absolute_references() {
        // Shows the case the reference rule exists for: the compiler itself
        // admits an absolute reference that names an `$id` in the document.
        let schema = json!({"$id": "https://example.com/s", "$defs": {"a": {"type": "string"}},
                            "$ref": "https://example.com/s#/$defs/a"});
        assert!(options().build(&schema).is_ok());
    }

    #[test]
    fn the_compiler_never_retrieves_an_external_schema() {
        // Bypasses the reference rule to show the compiler's own loader
        // refuses remote and file URIs.
        for uri in [
            "https://example.com/schema.json",
            "http://127.0.0.1:9/schema.json",
            "file:///etc/hosts",
        ] {
            assert!(options().build(&json!({"$ref": uri})).is_err(), "{uri}");
        }
    }

    #[test]
    fn formats_are_annotations_not_assertions() {
        let validator =
            compile_input_schema(&json!({"type": "string", "format": "email"})).unwrap();
        assert!(validator.is_valid(&json!("not an email address")));
    }

    #[test]
    fn schemas_are_draft_2020_12() {
        // `prefixItems` exists only in draft 2020-12; an earlier draft would
        // ignore it and accept the second instance.
        let validator =
            compile_input_schema(&json!({"prefixItems": [{"type": "integer"}]})).unwrap();
        assert!(validator.is_valid(&json!([1, "x"])));
        assert!(!validator.is_valid(&json!(["x"])));
        // Naming 2020-12 is fine; naming another draft, at the root or in a
        // subschema, is refused, since the compiler would honour it.
        let validator = compile_input_schema(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "prefixItems": [{"type": "integer"}]
        }))
        .unwrap();
        assert!(!validator.is_valid(&json!(["x"])));
        for schema in [
            json!({"$schema": "http://json-schema.org/draft-07/schema#"}),
            json!({"$defs": {"a": {"$id": "a", "$schema": "http://json-schema.org/draft-04/schema#"}}}),
        ] {
            assert!(
                matches!(
                    compile_input_schema(&schema),
                    Err(SchemaError::OtherDraft(_))
                ),
                "{schema}"
            );
        }
    }

    #[test]
    fn the_compiler_alone_honours_a_declared_draft() {
        // Shows the case the draft rule exists for: under draft-07 rules
        // `prefixItems` is ignored and the wrong input passes.
        let validator = options()
            .build(&json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "prefixItems": [{"type": "integer"}]
            }))
            .unwrap();
        assert!(validator.is_valid(&json!(["x"])));
    }

    #[test]
    fn a_schema_that_does_not_compile_is_invalid() {
        for schema in [
            json!({"type": "no-such-type"}),
            json!({"minimum": "one"}),
            json!({"pattern": "("}),
            json!(17),
        ] {
            assert!(
                matches!(compile_input_schema(&schema), Err(SchemaError::Invalid(_))),
                "{schema}"
            );
        }
    }
}
