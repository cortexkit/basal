//! Deterministic payload fuzzing: untrusted values through JSON, strings and
//! regular expressions inside the worker, under the JS time budget.
//!
//! The generator is seeded, so a failure reproduces from its seed and index.
//! Payloads mix well-formed JSON (deep nesting, every awkward string escape,
//! `__proto__` and friends as keys, extreme numbers) with corrupted text, and
//! regex inputs include patterns known to backtrack catastrophically.

use serde_json::{Value, json};

use basal_proto::{ActivationResult, BudgetKind, Failure, JsonText};

use crate::parent::{Ending, TestParent};

/// A small deterministic generator (xorshift64*).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const KEYS: &[&str] = &[
    "__proto__",
    "constructor",
    "prototype",
    "then",
    "toString",
    "valueOf",
    "a",
    "",
    "\u{2028}",
    "\u{0}",
    "length",
    "hasOwnProperty",
];

const STRING_ATOMS: &[&str] = &[
    "a",
    "\\u2028",
    "\\u2029",
    "\\ud800",
    "\\udfff",
    "\\ud83d\\ude00",
    "\\u0000",
    "\\n",
    "\\\"",
    "\\\\",
    "\u{2028}",
    "\u{feff}",
    "é",
    "e\u{301}",
    "😀",
    "</script>",
    "${x}",
    "\\/",
    "%s",
];

fn string(rng: &mut Rng, max_len: u64) -> String {
    let mut s = String::from("\"");
    for _ in 0..rng.below(max_len) {
        s.push_str(rng.pick(STRING_ATOMS));
    }
    s.push('"');
    s
}

fn number(rng: &mut Rng) -> String {
    rng.pick(&[
        "0",
        "-0",
        "1e308",
        "-1e308",
        "1e-400",
        "123456789012345678901234567890",
        "0.1",
        "-1",
        "5e-324",
        "1.7976931348623157e308",
        "9007199254740993",
    ])
    .to_owned()
}

/// A random JSON text of bounded size, nested up to `depth` levels.
fn value(rng: &mut Rng, depth: u32) -> String {
    let choice = if depth == 0 {
        rng.below(4)
    } else {
        rng.below(6)
    };
    match choice {
        0 => string(rng, 12),
        1 => number(rng),
        2 => rng.pick(&["true", "false", "null"]).to_owned(),
        3 => format!("\"{}\"", "x".repeat(rng.below(4096) as usize)),
        4 => {
            let n = rng.below(5);
            let items: Vec<String> = (0..n).map(|_| value(rng, depth - 1)).collect();
            format!("[{}]", items.join(","))
        }
        _ => {
            let n = rng.below(5);
            let items: Vec<String> = (0..n)
                .map(|_| {
                    let key =
                        serde_json::to_string(rng.pick(KEYS)).unwrap_or_else(|_| "\"k\"".into());
                    format!("{key}:{}", value(rng, depth - 1))
                })
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

/// The `index`-th payload for `seed`.
pub fn payload(seed: u64, index: u64) -> String {
    let mut rng = Rng::new(seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    match rng.below(10) {
        // Deep nesting, past what the engine's parser can recurse into.
        0 => {
            let depth = 100 + rng.below(20_000) as usize;
            format!("{}{}", "[".repeat(depth), "]".repeat(depth))
        }
        // Corrupted text: truncated, or with a byte replaced.
        1 => {
            let mut text = value(&mut rng, 4);
            let cut = rng.below(text.len() as u64) as usize;
            while !text.is_char_boundary(cut.min(text.len())) {
                text.pop();
            }
            text.truncate(cut.min(text.len()));
            text
        }
        2 => {
            let text = value(&mut rng, 4);
            let junk = rng.pick(&["{", "]", "\\", "\"", ",", "NaN", "undefined", "'"]);
            let at = rng.below(text.len() as u64 + 1) as usize;
            let at = (0..=at)
                .rev()
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(0);
            format!("{}{}{}", &text[..at], junk, &text[at..])
        }
        // A long string near the size the flows' events allow.
        3 => format!("\"{}\"", "\\u2028ab".repeat(rng.below(2048) as usize)),
        _ => value(&mut rng, 6),
    }
}

/// Regex inputs: (pattern, subject). Several backtrack exponentially.
pub fn regex_cases() -> Vec<(String, String)> {
    let evil = format!("{}!", "a".repeat(40));
    vec![
        ("(a+)+$".into(), evil.clone()),
        ("(a|aa)+$".into(), evil.clone()),
        ("(a|a?)+$".into(), evil.clone()),
        ("(.*a){25}".into(), "a".repeat(30)),
        (
            "^(([a-z])+.)+[A-Z]([a-z])+$".into(),
            format!("{}!", "aaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ),
        ("\\u2028|\\u2029".into(), "x\u{2028}y".into()),
        ("(?<x>a)\\k<x>".into(), "aa".into()),
        ("[".into(), "x".into()),
    ]
}

/// The script every fuzz activation runs.
pub const SCRIPT: &str = r#"
    const v = await ops.call('mock', 'raw', { raw: trigger.raw });
    const s = JSON.stringify(v);
    const back = JSON.parse(s);
    const str = typeof v === 'string' ? v : s;
    let re;
    try { re = new RegExp(trigger.pattern ?? str.slice(0, 32), 'u'); } catch (e) { re = /a|b/; }
    const subject = trigger.subject ?? str;
    const m = subject.match(re);
    return {
        roundTrip: JSON.stringify(back) === s,
        parts: str.split(/[,:{}\[\]]/).length,
        match: m ? m.length : 0,
        upper: str.toUpperCase().length,
        normalized: str.normalize('NFC').length,
        replaced: str.replace(/a/g, '$&$&').length,
    };
"#;

/// Counts of how fuzz activations ended.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub completed: u64,
    pub invalid_value: u64,
    pub js_time: u64,
    pub memory: u64,
    pub stack: u64,
    pub script_error: u64,
    /// Anything else: a hang, a broken channel, an engine failure, or a
    /// completed result whose round trip failed. Each is a finding.
    pub unexpected: Vec<String>,
}

/// Runs one fuzz case through `parent` and tallies how it ended.
pub fn run_case(parent: &mut TestParent, tally: &mut Tally, label: &str, trigger: Value) {
    let Ok(text) = JsonText::new(trigger.to_string()) else {
        return;
    };
    parent.trigger = text;
    parent.journals.clear();
    let report = parent.run(label, SCRIPT);
    match &report.ending {
        Ending::Finished(ActivationResult::Completed { value }) => {
            let parsed: Value = serde_json::from_str(value.as_str()).unwrap_or(Value::Null);
            if parsed["roundTrip"] == true {
                tally.completed += 1;
            } else {
                tally
                    .unexpected
                    .push(format!("{label}: round trip failed: {value:?}"));
            }
        }
        Ending::Finished(ActivationResult::Failed(Failure::InvalidHostValue { .. })) => {
            tally.invalid_value += 1;
        }
        Ending::Finished(ActivationResult::BudgetExhausted(BudgetKind::JsTime)) => {
            tally.js_time += 1
        }
        Ending::Finished(ActivationResult::BudgetExhausted(BudgetKind::Memory)) => {
            tally.memory += 1
        }
        Ending::Finished(ActivationResult::BudgetExhausted(BudgetKind::Stack)) => tally.stack += 1,
        Ending::Finished(ActivationResult::Failed(Failure::Script { .. })) => {
            tally.script_error += 1
        }
        other => tally.unexpected.push(format!("{label}: {other:?}")),
    }
}

/// Runs `count` payloads and every regex case.
pub fn run(parent: &mut TestParent, seed: u64, count: u64) -> Tally {
    let mut tally = Tally::default();
    for index in 0..count {
        let raw = payload(seed, index);
        run_case(
            parent,
            &mut tally,
            &format!("payload-{seed}-{index}"),
            json!({ "raw": raw }),
        );
    }
    for (i, (pattern, subject)) in regex_cases().into_iter().enumerate() {
        run_case(
            parent,
            &mut tally,
            &format!("regex-{i}"),
            json!({ "raw": "\"x\"", "pattern": pattern, "subject": subject }),
        );
    }
    tally
}
