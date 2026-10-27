//! Reproducible ordinary response bodies, generated from real words and fixed
//! counters. The committed bytes are also checked against the generator.
use super::*;
use base64::Engine;
use sha2::{Digest, Sha256};

const WORDS: &str = "
account activity adapter address archive balance browser catalog channel clarity
cluster command company compare connect context counter current customer database
default deliver details display document durable dynamic endpoint engine example
execute feature feedback gateway history identity include inspect install journal
latency machine message method monitor network notice observe operator ordinary
outcome package payload pending perform policy process project provide publish
quality reader record recover refresh region registry release remote request require
resolve resource restore result review runtime sample schedule section secure segment
service session signal source stable status storage stream summary support system
target timeout update validate version warning window worker information operation
application configuration protection representation reliability component important
successful discovery processing reporting connection delivery analysis planning
management capacity critical performance response collection complete progress
";

struct Words {
    counter: u64,
    words: Vec<&'static str>,
}
impl Words {
    fn new(seed: u64) -> Self {
        Self {
            counter: seed,
            words: WORDS.split_whitespace().collect(),
        }
    }
    fn word(&mut self) -> &'static str {
        self.counter = self.counter.wrapping_add(0x9e3779b97f4a7c15);
        let mut x = self.counter;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
        x ^= x >> 31;
        self.words[(x % self.words.len() as u64) as usize]
    }
    fn phrase(&mut self, count: usize) -> String {
        (0..count)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn sentence(&mut self, count: usize) -> String {
        let mut value = self.phrase(count);
        value.get_mut(..1).unwrap().make_ascii_uppercase();
        value.push('.');
        value
    }
}

fn empty_key_hmac(input: &[u8]) -> Vec<u8> {
    // Public empty signing key: the JWTs are fixture data, not credentials.
    let inner = Sha256::new()
        .chain_update([0x36; 64])
        .chain_update(input)
        .finalize();
    Sha256::new()
        .chain_update([0x5c; 64])
        .chain_update(inner)
        .finalize()
        .to_vec()
}

pub(super) fn generate(kind: &str, seed: u64) -> String {
    let mut words = Words::new(seed);
    let mut body = match kind {
        "prose" => (0..16)
            .map(|_| words.sentence(10))
            .collect::<Vec<_>>()
            .join(" "),
        "text" => {
            let mut body = String::new();
            while body.len() < 5000 {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&words.sentence(10));
            }
            body
        }
        "descriptions" => serde_json::to_string(
            &(0..20)
                .map(|i| serde_json::json!({"id":i,"description":words.phrase(12)}))
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        "html" => {
            let mut body = "<!doctype html><html lang=\"en\"><head><title>Service report</title></head><body><main>".to_owned();
            let mut i = 0;
            while body.len() < 8800 {
                let block = match i % 4 {
                    0 => format!(
                        "<section data-index=\"{i}\" class=\"{}\"><h2>{}</h2><p>{}</p></section>",
                        words.word(),
                        words.sentence(6),
                        words.sentence(12)
                    ),
                    1 => format!(
                        "<article id=\"{}-{i}\"><header>{}</header><p>{}</p><footer>{}</footer></article>",
                        words.word(),
                        words.sentence(8),
                        words.sentence(14),
                        words.sentence(6)
                    ),
                    2 => format!(
                        "<ul><li data-type=\"{}\">{}</li><li>{}</li></ul>",
                        words.word(),
                        words.sentence(12),
                        words.sentence(12)
                    ),
                    _ => format!(
                        "<p title=\"{}\">{}</p>",
                        words.sentence(8),
                        words.sentence(16)
                    ),
                };
                body.push_str(&block);
                i += 1;
            }
            body.push_str("</main></body></html>");
            body
        }
        "jwts" => {
            let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            serde_json::to_string(&(0..20).map(|i| {
                let header = engine.encode(serde_json::json!({"alg":"HS256","typ":"JWT","cty":"application/json","kid":words.phrase(4).replace(' ',"-")}).to_string());
                let payload = engine.encode(serde_json::json!({"sub":format!("user-{i}-{}",words.phrase(2).replace(' ',"-")),"aud":"response-fixture","iat":1700000000_u64+i,"description":words.phrase(12)}).to_string());
                let input = format!("{header}.{payload}");
                format!("{input}.{}",engine.encode(empty_key_hmac(input.as_bytes())))
            }).collect::<Vec<_>>()).unwrap()
        }
        "large" => {
            let mut records = Vec::new();
            let mut body = String::new();
            while body.len() < 90000 {
                records
                    .push(serde_json::json!({"id":records.len(),"description":words.phrase(12)}));
                body = serde_json::to_string(&records).unwrap();
            }
            body
        }
        _ => panic!("unknown response fixture"),
    };
    body.push('\n');
    body
}

const CASES: &[(&str, u64, &str)] = &[
    (
        "prose",
        0,
        include_str!("fixtures/ordinary-responses/prose.txt"),
    ),
    (
        "text",
        0,
        include_str!("fixtures/ordinary-responses/text.txt"),
    ),
    (
        "descriptions",
        0,
        include_str!("fixtures/ordinary-responses/descriptions.json"),
    ),
    (
        "html",
        0,
        include_str!("fixtures/ordinary-responses/page.html"),
    ),
    (
        "jwts",
        0,
        include_str!("fixtures/ordinary-responses/jwts.json"),
    ),
    (
        "large",
        0,
        include_str!("fixtures/ordinary-responses/large.json"),
    ),
];

fn check(kind: &str, key_len: usize) {
    let (_, seed, body) = CASES.iter().find(|(name, _, _)| *name == kind).unwrap();
    assert_eq!(*body, generate(kind, *seed));
    let short = "ghp_R4nd0mT0k3nV4lu3";
    let key = if key_len == 20 {
        short.to_owned()
    } else {
        format!("{short}{}", "0123456789".repeat(3))
    };
    assert_eq!(key.len(), key_len);
    match kind {
        "prose" => {
            assert!((1150..1550).contains(&body.len()));
            assert_eq!(
                body.split('.')
                    .filter(|s| !s.trim().is_empty())
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                16
            );
        }
        "text" => assert!((5000..5200).contains(&body.len())),
        "descriptions" => {
            let rows: Vec<serde_json::Value> = serde_json::from_str(body).unwrap();
            assert_eq!(rows.len(), 20);
            let unique: std::collections::HashSet<_> = rows
                .iter()
                .map(|r| r["description"].as_str().unwrap())
                .collect();
            assert_eq!(unique.len(), 20);
            assert!(unique.iter().all(|s| s.split_whitespace().count() == 12));
            assert!((2100..2900).contains(&body.len()));
        }
        "html" => assert!((8800..9500).contains(&body.len())),
        "large" => assert!((90000..90400).contains(&body.len())),
        "jwts" => {
            let rows: Vec<String> = serde_json::from_str(body).unwrap();
            assert_eq!(rows.len(), 20);
            let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let mut headers = std::collections::HashSet::new();
            let mut payloads = std::collections::HashSet::new();
            for row in rows {
                let parts: Vec<_> = row.split('.').collect();
                assert_eq!(parts.len(), 3);
                let header = engine.decode(parts[0]).unwrap();
                let payload = engine.decode(parts[1]).unwrap();
                serde_json::from_slice::<serde_json::Value>(&header).unwrap();
                serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
                headers.insert(header);
                payloads.insert(payload);
                assert_eq!(
                    engine.decode(parts[2]).unwrap(),
                    empty_key_hmac(format!("{}.{}", parts[0], parts[1]).as_bytes())
                );
            }
            assert_eq!(headers.len(), 20);
            assert_eq!(payloads.len(), 20);
        }
        _ => unreachable!(),
    }
    let value = finish_secret(
        Response {
            status: 200,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            body_error: None,
            effect_possible: false,
        },
        key.as_bytes(),
        "key",
    )
    .unwrap();
    assert_eq!(
        value["body"].as_str(),
        Some(*body),
        "{kind}, {key_len}-byte key"
    );
    assert!(value.get("body_error").is_none());
}

macro_rules! case {
    ($name:ident,$kind:literal,$size:literal) => {
        #[test]
        fn $name() {
            check($kind, $size);
        }
    };
}
case!(second_review_corpus_prose_key20, "prose", 20);
case!(second_review_corpus_prose_key50, "prose", 50);
case!(second_review_corpus_text_key20, "text", 20);
case!(second_review_corpus_text_key50, "text", 50);
case!(second_review_corpus_descriptions_key20, "descriptions", 20);
case!(second_review_corpus_descriptions_key50, "descriptions", 50);
case!(second_review_corpus_html_key20, "html", 20);
case!(second_review_corpus_html_key50, "html", 50);
case!(second_review_corpus_jwts_key20, "jwts", 20);
case!(second_review_corpus_jwts_key50, "jwts", 50);
case!(second_review_corpus_large_key20, "large", 20);
case!(second_review_corpus_large_key50, "large", 50);
