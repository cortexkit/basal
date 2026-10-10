//! The catalog digest vectors published in `docs/ops.md`, reproduced by an
//! oracle written here from the specifications alone.
//!
//! A codemode run's `catalog_digest` is the lowercase hex BLAKE3-256 of the
//! RFC 8785 (JCS) canonical JSON of its catalog, entries sorted by `name`.
//! Core computes the same digest independently, so the published vectors are
//! the contract between the two. The oracle below never calls basal's
//! canonicalizer or digest helper: it has its own canonical JSON writer and
//! its own BLAKE3, so a bug shared by the production code and its unit tests
//! cannot also make the published vectors agree with it. A separate test then
//! checks the production digest against the same vectors.

use serde_json::Value;

const DOC: &str = include_str!("../../../docs/ops.md");

struct Vector {
    name: String,
    catalog: String,
    canonical: String,
    digest: String,
}

/// Text between the first `start` and the next `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let rest = &text[text
        .find(start)
        .unwrap_or_else(|| panic!("missing {start:?}"))
        + start.len()..];
    &rest[..rest
        .find(end)
        .unwrap_or_else(|| panic!("unterminated {start:?}"))]
}

/// Each `### Vector `name`` block of the doc's digest section: its catalog
/// as written, its canonical JSON and its digest.
fn published() -> Vec<Vector> {
    let section = between(DOC, "\n## Catalog digest vectors\n", "\n## ");
    let vectors: Vec<Vector> = section
        .split("\n### Vector `")
        .skip(1)
        .map(|block| Vector {
            name: block[..block.find('`').unwrap()].to_owned(),
            catalog: between(block, "```json\n", "\n```").to_owned(),
            canonical: between(block, "```text\n", "\n```").to_owned(),
            digest: between(block, "Digest: `", "`").to_owned(),
        })
        .collect();
    assert!(!vectors.is_empty(), "no digest vectors published");
    vectors
}

fn vector<'a>(vectors: &'a [Vector], name: &str) -> &'a Vector {
    vectors
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("vector {name} is not published"))
}

// ---- Oracle: RFC 8785 canonical JSON ----

/// The catalog's canonical text: entries sorted by `name` in UTF-16 code
/// unit order, then written as RFC 8785 canonical JSON.
fn oracle_canonical_catalog(catalog: &Value) -> String {
    let mut entries = catalog.as_array().expect("a catalog is an array").clone();
    entries.sort_by_key(|entry| utf16(entry["name"].as_str().expect("string name")));
    let mut out = String::new();
    jcs(&mut out, &Value::Array(entries));
    out
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// RFC 8785 section 3.2: no whitespace, members sorted by name as UTF-16
/// code units, strings and numbers in their canonical spellings.
fn jcs(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&es_number(n.as_f64().expect("finite number"))),
        Value::String(s) => jcs_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut names: Vec<&String> = map.keys().collect();
            names.sort_by_key(|name| utf16(name));
            out.push('{');
            for (i, name) in names.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs_string(out, name);
                out.push(':');
                jcs(out, &map[name]);
            }
            out.push('}');
        }
    }
}

/// RFC 8785 section 3.2.2.2: `\"`, `\\` and the five short control escapes,
/// `\u00xx` in lowercase hex for any other control character, and every
/// other character, non-ASCII included, as itself.
fn jcs_string(out: &mut String, s: &str) {
    const SHORT: [(char, &str); 7] = [
        ('"', "\\\""),
        ('\\', "\\\\"),
        ('\u{8}', "\\b"),
        ('\u{9}', "\\t"),
        ('\u{a}', "\\n"),
        ('\u{c}', "\\f"),
        ('\u{d}', "\\r"),
    ];
    out.push('"');
    for c in s.chars() {
        if let Some((_, escape)) = SHORT.iter().find(|(short, _)| *short == c) {
            out.push_str(escape);
        } else if c < ' ' {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out.push('"');
}

/// ECMAScript's `Number::toString` for a finite double, which RFC 8785
/// section 3.2.2.3 adopts. Rust's `Display` writes a double's shortest
/// round-trip digits in plain positional notation; those digits are read
/// back as `s` (k digits) and `n`, where the value is `0.s × 10^n`, and laid
/// out by the ECMAScript rules.
fn es_number(x: f64) -> String {
    assert!(x.is_finite());
    if x == 0.0 {
        return "0".into();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let positional = format!("{}", x.abs());
    let (int, frac) = positional.split_once('.').unwrap_or((&positional, ""));
    let mut digits = format!("{int}{frac}");
    let mut n = int.len() as i64;
    while digits.starts_with('0') {
        digits.remove(0);
        n -= 1;
    }
    while digits.ends_with('0') {
        digits.pop();
    }
    let k = digits.len() as i64;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let rest = if k > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let exponent = n - 1;
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{}{rest}e{exponent_sign}{}", &digits[..1], exponent.abs())
    };
    format!("{sign}{body}")
}

// ---- Oracle: BLAKE3 (hash mode, 32-byte output) ----

const IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
const PERMUTATION: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;
const BLOCK_LEN: usize = 64;
const CHUNK_LEN: usize = 1024;

fn g(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    s[a] = s[a].wrapping_add(s[b]).wrapping_add(x);
    s[d] = (s[d] ^ s[a]).rotate_right(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(12);
    s[a] = s[a].wrapping_add(s[b]).wrapping_add(y);
    s[d] = (s[d] ^ s[a]).rotate_right(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_right(7);
}

/// The BLAKE3 compression function; returns the first eight output words,
/// which is all a 32-byte hash and every chaining value need.
fn compress(cv: &[u32; 8], block: &[u8], counter: u64, flags: u32) -> [u32; 8] {
    let mut padded = [0u8; BLOCK_LEN];
    padded[..block.len()].copy_from_slice(block);
    let mut m = [0u32; 16];
    for (i, word) in m.iter_mut().enumerate() {
        *word = u32::from_le_bytes(padded[4 * i..4 * i + 4].try_into().unwrap());
    }
    let mut s = [
        cv[0],
        cv[1],
        cv[2],
        cv[3],
        cv[4],
        cv[5],
        cv[6],
        cv[7],
        IV[0],
        IV[1],
        IV[2],
        IV[3],
        counter as u32,
        (counter >> 32) as u32,
        block.len() as u32,
        flags,
    ];
    for round in 0..7 {
        g(&mut s, 0, 4, 8, 12, m[0], m[1]);
        g(&mut s, 1, 5, 9, 13, m[2], m[3]);
        g(&mut s, 2, 6, 10, 14, m[4], m[5]);
        g(&mut s, 3, 7, 11, 15, m[6], m[7]);
        g(&mut s, 0, 5, 10, 15, m[8], m[9]);
        g(&mut s, 1, 6, 11, 12, m[10], m[11]);
        g(&mut s, 2, 7, 8, 13, m[12], m[13]);
        g(&mut s, 3, 4, 9, 14, m[14], m[15]);
        if round < 6 {
            m = PERMUTATION.map(|i| m[i]);
        }
    }
    std::array::from_fn(|i| s[i] ^ s[i + 8])
}

/// One chunk of at most 1,024 bytes, as blocks chained from the key. An
/// empty input is one empty block.
fn chunk(input: &[u8], index: u64, root: bool) -> [u32; 8] {
    let blocks: Vec<&[u8]> = if input.is_empty() {
        vec![&[][..]]
    } else {
        input.chunks(BLOCK_LEN).collect()
    };
    let mut cv = IV;
    for (i, block) in blocks.iter().enumerate() {
        let mut flags = 0;
        if i == 0 {
            flags |= CHUNK_START;
        }
        if i == blocks.len() - 1 {
            flags |= CHUNK_END | if root { ROOT } else { 0 };
        }
        cv = compress(&cv, block, index, flags);
    }
    cv
}

/// The subtree over `input`, whose first chunk has index `first`: its left
/// half holds the largest power-of-two number of chunks that leaves at least
/// one chunk on the right.
fn subtree(input: &[u8], first: u64, root: bool) -> [u32; 8] {
    if input.len() <= CHUNK_LEN {
        return chunk(input, first, root);
    }
    let chunks = input.len().div_ceil(CHUNK_LEN);
    let left_chunks = 1usize << (usize::BITS - 1 - (chunks - 1).leading_zeros());
    let (left, right) = input.split_at(left_chunks * CHUNK_LEN);
    let mut block = [0u8; BLOCK_LEN];
    let halves = subtree(left, first, false).into_iter().chain(subtree(
        right,
        first + left_chunks as u64,
        false,
    ));
    for (i, word) in halves.enumerate() {
        block[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    compress(&IV, &block, 0, PARENT | if root { ROOT } else { 0 })
}

fn oracle_blake3_hex(input: &[u8]) -> String {
    subtree(input, 0, true)
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---- Tests ----

#[test]
fn the_oracle_blake3_matches_published_known_answers() {
    // The BLAKE3 hashes of "" and "abc", as published with the algorithm.
    assert_eq!(
        oracle_blake3_hex(b""),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
    assert_eq!(
        oracle_blake3_hex(b"abc"),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
    // Block, chunk and tree boundaries, compared with the blake3 crate
    // itself (not basal's digest helper).
    let input: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
    for len in [
        1, 63, 64, 65, 1023, 1024, 1025, 2048, 2049, 3072, 3073, 4097, 8193, 9000,
    ] {
        assert_eq!(
            oracle_blake3_hex(&input[..len]),
            blake3::hash(&input[..len]).to_hex().as_str(),
            "length {len}"
        );
    }
}

#[test]
fn the_oracle_canonical_json_matches_the_rfc_8785_example() {
    // RFC 8785 section 3.2.4, with its published canonical form.
    let input: Value = serde_json::from_str(
        r#"{
            "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
            "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
            "literals": [null, true, false]
        }"#,
    )
    .unwrap();
    let mut out = String::new();
    jcs(&mut out, &input);
    assert_eq!(
        out,
        "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"\u{20ac}$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}"
    );
}

#[test]
fn published_digest_vectors_are_reproduced_by_the_oracle() {
    // Reports every mismatching vector at once, with the oracle's values.
    let mut mismatches = Vec::new();
    for v in published() {
        let catalog: Value = serde_json::from_str(&v.catalog)
            .unwrap_or_else(|e| panic!("vector {}: catalog is not JSON: {e}", v.name));
        let canonical = oracle_canonical_catalog(&catalog);
        let digest = oracle_blake3_hex(canonical.as_bytes());
        if canonical != v.canonical || digest != v.digest {
            mismatches.push(format!("{}: {canonical} {digest}", v.name));
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[test]
fn the_production_digest_matches_every_published_vector() {
    for v in published() {
        let catalog: Value = serde_json::from_str(&v.catalog).unwrap();
        assert_eq!(
            basal_core::codemode::catalog::catalog_digest(&catalog).unwrap(),
            v.digest,
            "vector {}",
            v.name
        );
    }
}

#[test]
fn published_vectors_cover_reordering_numbers_and_strings() {
    let vectors = published();
    let base = vector(&vectors, "base");
    // Reordering keys or entries changes the text but not the digest.
    for name in ["keys-reordered", "entries-reordered"] {
        let v = vector(&vectors, name);
        assert_ne!(v.catalog, base.catalog, "{name} must differ as written");
        assert_eq!(v.canonical, base.canonical, "{name}");
        assert_eq!(v.digest, base.digest, "{name}");
    }
    let entries = |v: &Vector| -> Vec<String> {
        let catalog: Value = serde_json::from_str(&v.catalog).unwrap();
        catalog
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_ne!(
        entries(vector(&vectors, "entries-reordered")),
        entries(base),
        "entries-reordered must list its entries in another order"
    );
    // `1.0` keeps no trailing zero; an exponent takes ECMAScript's form.
    let trailing = vector(&vectors, "trailing-zero");
    assert!(trailing.catalog.contains(":1.0"));
    assert!(!trailing.canonical.contains("1.0"));
    let exponent = vector(&vectors, "exponent");
    assert!(exponent.catalog.contains("e21") || exponent.catalog.contains("E21"));
    assert!(exponent.canonical.contains("e+21") && exponent.canonical.contains("e-7"));
    // Non-ASCII text is written as itself; a control character is escaped.
    let string = vector(&vectors, "string");
    assert!(!string.canonical.is_ascii());
    assert!(string.canonical.contains("\\u0007"));
    let catalog: Value = serde_json::from_str(&string.catalog).unwrap();
    assert!(catalog.to_string().contains("\\u0007"));
    // Every digest is distinct unless its catalog is the same up to order.
    for a in &vectors {
        for b in &vectors {
            if a.canonical != b.canonical {
                assert_ne!(a.digest, b.digest, "{} and {}", a.name, b.name);
            }
        }
    }
}
