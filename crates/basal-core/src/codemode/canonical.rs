//! Canonical JSON as defined by RFC 8785, the JSON Canonicalization Scheme
//! (JCS).
//!
//! The same JSON value always serializes to the same bytes: object members
//! are sorted by their names compared as UTF-16 code units, there is no
//! whitespace, strings use the shortest escapes, and every number is written
//! the way ECMAScript's `Number.prototype.toString` writes the IEEE-754
//! double it denotes. That makes a hash of the output a stable identity for
//! the value, whatever key order or number spelling the sender used.

use serde_json::Value;

/// The RFC 8785 canonical form of `value`.
pub fn to_canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            // JCS treats every JSON number as an IEEE-754 double, so an
            // integer beyond 2^53 is rounded exactly as JavaScript would.
            let double = n.as_f64().unwrap_or(f64::NAN);
            write_number(out, double);
        }
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut members: Vec<(&String, &Value)> = map.iter().collect();
            members.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (i, (name, member)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, name);
                out.push(':');
                write_value(out, member);
            }
            out.push('}');
        }
    }
}

/// JSON string escaping as RFC 8785 section 3.2.2.2 requires: the two-
/// character escapes for `"`, `\` and the five named controls, `\u00xx` with
/// lowercase hex for every other control below U+0020, and every other
/// character, non-ASCII included, written as itself.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// ECMAScript `Number::toString(x)` for a finite double (ECMA-262,
/// section 6.1.6.1.20), which RFC 8785 section 3.2.2.3 adopts.
///
/// The algorithm needs the shortest digit string `s` (of `k` digits) and the
/// exponent `n` with `x = s × 10^(n−k)`. Rust's `{:e}` formatting prints
/// exactly that shortest round-trip digit string, so it is parsed back and
/// laid out by the ECMAScript rules.
fn write_number(out: &mut String, x: f64) {
    if !x.is_finite() {
        // A JSON document cannot carry NaN or an infinity; serde_json never
        // produces one. Write what JavaScript's JSON.stringify writes.
        out.push_str("null");
        return;
    }
    if x == 0.0 {
        // Both zeros are "0".
        out.push('0');
        return;
    }
    if x < 0.0 {
        out.push('-');
    }
    let (digits, n) = shortest_closest_digits(x.abs());
    let k = digits.len() as i64;
    if k <= n && n <= 21 {
        // An integer: the digits followed by n−k zeros.
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        // A decimal point inside the digits.
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        // A small fraction: "0." then −n zeros, then the digits.
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        // Exponent form: one digit, the rest after a point when there is a
        // rest, then "e", an explicit sign and n−1.
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n - 1 < 0 { '-' } else { '+' });
        out.push_str(&(n - 1).abs().to_string());
    }
}

/// The digits `s` and exponent `n` ECMAScript chooses for a positive finite
/// double: the fewest digits that read back as `x`, and among the digit
/// strings of that length that do, the one closest to `x`, the even one on
/// a tie.
///
/// Rust's `{:e}` gives a shortest string, which fixes the digit count `k`,
/// but it need not be the closest one: for 0x43143ff3c1cb0959, exactly
/// 1424953923781206.25, it prints ...206.3 where ECMAScript requires ...206.2.
/// Formatting with `k` significant digits rounds the exact value to the
/// nearest, ties to even; that string is used whenever it reads back as `x`.
fn shortest_closest_digits(x: f64) -> (String, i64) {
    let shortest = format!("{x:e}");
    let k = mantissa_digits(&shortest).0.len();
    let closest = format!("{:.*e}", k.saturating_sub(1), x);
    let chosen = if closest.parse::<f64>() == Ok(x) {
        closest
    } else {
        shortest
    };
    let (mut digits, n) = mantissa_digits(&chosen);
    // A trailing zero would mean fewer digits also read back as `x`.
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    (digits, n)
}

/// Splits Rust's `d.ddde±x` form into its digits and ECMAScript's `n`
/// (the exponent plus one).
fn mantissa_digits(sci: &str) -> (String, i64) {
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci, "0"));
    let digits = mantissa.chars().filter(char::is_ascii_digit).collect();
    (digits, exponent.parse::<i64>().unwrap_or(0) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn number(bits: u64) -> String {
        let mut out = String::new();
        write_number(&mut out, f64::from_bits(bits));
        out
    }

    // RFC 8785 Appendix B: IEEE-754 doubles, by bit pattern, and the text
    // each must serialize to.
    #[test]
    fn numbers_match_the_rfc_8785_appendix_b_table() {
        let table: &[(u64, &str)] = &[
            (0x0000000000000000, "0"),
            (0x8000000000000000, "0"),
            (0x0000000000000001, "5e-324"),
            (0x8000000000000001, "-5e-324"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0xffefffffffffffff, "-1.7976931348623157e+308"),
            (0x4340000000000000, "9007199254740992"),
            (0xc340000000000000, "-9007199254740992"),
            (0x4430000000000000, "295147905179352830000"),
            (0x44b52d02c7e14af5, "9.999999999999997e+22"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
            (0x444b1ae4d6e2ef4e, "999999999999999700000"),
            (0x444b1ae4d6e2ef4f, "999999999999999900000"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x41b3de4355555553, "333333333.3333332"),
            (0x41b3de4355555554, "333333333.33333325"),
            (0x41b3de4355555555, "333333333.3333333"),
            (0x41b3de4355555556, "333333333.3333334"),
            (0x41b3de4355555557, "333333333.33333343"),
            (0xbecbf647612f3696, "-0.0000033333333333333333"),
            (0x43143ff3c1cb0959, "1424953923781206.2"),
        ];
        for &(bits, expected) in table {
            assert_eq!(number(bits), expected, "bits {bits:#018x}");
        }
    }

    #[test]
    fn json_text_parses_to_the_nearest_double() {
        // 333333333.33333329 lies nearest 0x41b3de4355555555; an inexact
        // parser lands on its neighbour, which serializes differently.
        let value: Value = serde_json::from_str("333333333.33333329").unwrap();
        assert_eq!(value.as_f64().unwrap().to_bits(), 0x41b3de4355555555);
        assert_eq!(to_canonical_json(&value), "333333333.3333333");
    }

    #[test]
    fn json_number_spellings_collapse_to_one_form() {
        let value: Value = serde_json::from_str(
            r#"[1.0, 1, 1e0, 10.50, 1E2, 2e-3, 1e21, 1e-7, 9007199254740993]"#,
        )
        .unwrap();
        assert_eq!(
            to_canonical_json(&value),
            "[1,1,1,10.5,100,0.002,1e+21,1e-7,9007199254740992]"
        );
    }

    // The worked example of RFC 8785 section 3.2.4: numbers, escaped
    // strings and literals in one object, with its published canonical form.
    #[test]
    fn the_rfc_8785_example_canonicalizes_exactly() {
        let input = r#"{
            "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 0.000000000000000000000000001],
            "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
            "literals": [null, true, false]
        }"#;
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            to_canonical_json(&value),
            "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"\u{20ac}$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}"
        );
    }

    // RFC 8785 section 3.2.3: names sort by UTF-16 code units, so the
    // emoji (a surrogate pair starting 0xD83D) sorts before U+FB33 even
    // though its code point is larger.
    #[test]
    fn member_names_sort_by_utf16_code_units() {
        let value = json!({
            "\u{20ac}": "Euro Sign",
            "\r": "Carriage Return",
            "\u{fb33}": "Hebrew Letter Dalet With Dagesh",
            "1": "One",
            "\u{1f600}": "Emoji: Grinning Face",
            "\u{80}": "Control",
            "\u{f6}": "Latin Small Letter O With Diaeresis"
        });
        let expected = concat!(
            "{",
            "\"\\r\":\"Carriage Return\",",
            "\"1\":\"One\",",
            "\"\u{80}\":\"Control\",",
            "\"\u{f6}\":\"Latin Small Letter O With Diaeresis\",",
            "\"\u{20ac}\":\"Euro Sign\",",
            "\"\u{1f600}\":\"Emoji: Grinning Face\",",
            "\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"",
            "}"
        );
        assert_eq!(to_canonical_json(&value), expected);
    }

    #[test]
    fn strings_use_the_shortest_escapes_and_lowercase_hex() {
        let value = json!("q\"b\\s\u{8}f\u{c}n\nr\rt\tc\u{1}\u{1f}d\u{7f}/\u{2028}\u{e9}");
        assert_eq!(
            to_canonical_json(&value),
            "\"q\\\"b\\\\s\\bf\\fn\\nr\\rt\\tc\\u0001\\u001fd\u{7f}/\u{2028}\u{e9}\""
        );
    }

    #[test]
    fn nested_values_have_no_whitespace() {
        let value = json!({"b": [1, {"d": null, "c": true}], "a": {}});
        assert_eq!(
            to_canonical_json(&value),
            r#"{"a":{},"b":[1,{"c":true,"d":null}]}"#
        );
    }
}
