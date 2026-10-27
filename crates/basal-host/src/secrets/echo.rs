//! Bounded detection views supplement exact substitution. Unscanned data is
//! withheld, not assumed safe when the normalization budget is exhausted.
use base64::Engine;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};

#[path = "html_ascii.rs"]
mod html_ascii;

const MAX_DEPTH: usize = 8;
const MAX_VIEWS: usize = 64;
const MAX_WORK: usize = super::REDACTION_WORK;

pub(crate) struct EchoGuard {
    key: Vec<u8>,
    prefix: Vec<usize>,
    remaining: usize,
}

impl EchoGuard {
    pub(crate) fn redact(
        &mut self,
        input: &[u8],
        key: &[u8],
        name: &str,
    ) -> Result<Vec<u8>, super::RedactionLimit> {
        super::redact_with_budget(input, key, name, &mut self.remaining)
    }
    pub(crate) fn new(key: &[u8]) -> Self {
        let key = key.to_ascii_lowercase();
        let mut prefix = vec![0; key.len()];
        for i in 1..key.len() {
            let mut j = prefix[i - 1];
            while j > 0 && key[i] != key[j] {
                j = prefix[j - 1];
            }
            if key[i] == key[j] {
                j += 1;
            }
            prefix[i] = j;
        }
        Self {
            key,
            prefix,
            remaining: MAX_WORK,
        }
    }

    fn spend(&mut self, work: usize) -> bool {
        if work > self.remaining {
            self.remaining = 0;
            return false;
        }
        self.remaining -= work;
        true
    }

    fn contains(&self, input: &[u8]) -> bool {
        let mut j = 0;
        for b in input {
            let b = b.to_ascii_lowercase();
            while j > 0 && b != self.key[j] {
                j = self.prefix[j - 1];
            }
            if b == self.key[j] {
                j += 1;
            }
            if j == self.key.len() {
                return true;
            }
        }
        false
    }

    fn queue(
        &mut self,
        bytes: Vec<u8>,
        depth: usize,
        queue: &mut VecDeque<(Vec<u8>, usize)>,
        seen: &mut HashSet<[u8; 32]>,
    ) -> bool {
        if bytes.len() < self.key.len() {
            return false;
        }
        if !self.spend(bytes.len()) {
            return true;
        }
        let digest = Sha256::digest(&bytes).into();
        if !seen.insert(digest) {
            return false;
        }
        if depth > MAX_DEPTH || seen.len() > MAX_VIEWS {
            return true;
        }
        queue.push_back((bytes, depth));
        false
    }

    /// True means an echo was found or safety could not be established within
    /// the response-wide work budget. It never means a partial key matched.
    pub(crate) fn unsafe_echo(&mut self, input: &[u8]) -> bool {
        if self.key.is_empty() {
            return true;
        }
        if input.len() < self.key.len() {
            return false;
        }
        if !self.spend(input.len()) {
            return true;
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        if self.queue(input.to_vec(), 0, &mut queue, &mut seen) {
            return true;
        }
        while let Some((bytes, depth)) = queue.pop_front() {
            if !self.spend(bytes.len()) || self.contains(&bytes) || self.selective_echo(&bytes) {
                return true;
            }
            for view in [
                decode_tokens(&bytes, json_token),
                decode_tokens(&bytes, html_token),
                percent(&bytes, false),
                percent(&bytes, true),
            ] {
                if view != bytes && self.queue(view, depth + 1, &mut queue, &mut seen) {
                    return true;
                }
            }
            let alphabet = |b: u8| b.is_ascii_alphanumeric() || b"+/-_".contains(&b);
            let engine = base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new()
                    .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
                    .with_decode_allow_trailing_bits(true),
            );
            let mut offset = 0;
            while offset < bytes.len() {
                if !alphabet(bytes[offset]) {
                    offset += 1;
                    continue;
                }
                let mut run = Vec::new();
                while offset < bytes.len() {
                    let b = bytes[offset];
                    if alphabet(b) {
                        run.push(match b {
                            b'-' => b'+',
                            b'_' => b'/',
                            _ => b,
                        });
                        offset += 1;
                    } else if b.is_ascii_whitespace() {
                        offset += 1;
                    } else if bytes[offset..].starts_with(b"\\n") {
                        offset += 2;
                    } else {
                        break;
                    }
                }
                for phase in 0..4.min(run.len()) {
                    let mut candidate = &run[phase..];
                    if candidate.len() % 4 == 1 {
                        candidate = &candidate[..candidate.len() - 1];
                    }
                    if candidate.len() < 2 || candidate.len() * 3 / 4 < self.key.len() {
                        continue;
                    }
                    if !self.spend(candidate.len()) {
                        return true;
                    }
                    if let Ok(decoded) = engine.decode(candidate) {
                        if !self.spend(decoded.len())
                            || self.contains(&decoded)
                            || self.selective_echo(&decoded)
                        {
                            return true;
                        }
                        if needs_view(&decoded, self.key.len())
                            && self.queue(decoded, depth + 1, &mut queue, &mut seen)
                        {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn selective_echo(&mut self, input: &[u8]) -> bool {
        // Literal-only spellings have already been checked by the linear matcher.
        let mut markers = input
            .iter()
            .enumerate()
            .filter_map(|(i, b)| b"\\&%+".contains(b).then_some(i))
            .peekable();
        for start in 0..input.len() {
            if input.len() - start < self.key.len() {
                break;
            }
            while markers.peek().is_some_and(|p| *p < start) {
                markers.next();
            }
            if !markers.peek().is_some_and(|p| *p < start + self.key.len()) {
                continue;
            }
            let mut states = vec![(start, 0)];
            let mut seen = HashSet::new();
            while let Some((offset, matched)) = states.pop() {
                if !seen.insert((offset, matched)) {
                    continue;
                }
                if !self.spend(1) || states.len() > 512 || seen.len() > 32768 {
                    return true;
                }
                if matched == self.key.len() {
                    return true;
                }
                let Some(&b) = input.get(offset) else {
                    continue;
                };
                let alternatives = if b"\\&%+".contains(&b) {
                    match self.fragment_alternatives(&input[offset..]) {
                        Ok(values) => values,
                        Err(()) => return true,
                    }
                } else {
                    vec![(vec![b], 1)]
                };
                for (decoded, consumed) in alternatives {
                    let remaining = &self.key[matched..];
                    let common = remaining.len().min(decoded.len());
                    if remaining[..common].eq_ignore_ascii_case(&decoded[..common]) {
                        // An entity may append a combining mark after a complete key.
                        if decoded.len() >= remaining.len() {
                            return true;
                        }
                        states.push((offset + consumed, matched + decoded.len()));
                    }
                }
            }
        }
        false
    }

    // Retain original source boundaries while decoding one fragment, so a
    // decoded suffix does not force decoding escape-looking text elsewhere.
    fn fragment_alternatives(&mut self, input: &[u8]) -> Result<Vec<(Vec<u8>, usize)>, ()> {
        let mut width = input.len().min(32);
        'window: loop {
            let truncated = width < input.len();
            let initial: Spanned = input[..width]
                .iter()
                .enumerate()
                .map(|(i, b)| (*b, i + 1))
                .collect();
            let mut seen = HashSet::from([initial.clone()]);
            let mut views = VecDeque::from([(initial, 0)]);
            let mut alternatives = Vec::new();
            while let Some((view, depth)) = views.pop_front() {
                if !self.spend(view.len().saturating_mul(8)) {
                    return Err(());
                }
                let bytes: Vec<u8> = view.iter().map(|(b, _)| *b).collect();
                if truncated && leading_incomplete(&bytes) {
                    if width == 4096 {
                        return Err(());
                    }
                    width = input.len().min(width * 2).min(4096);
                    continue 'window;
                }
                let consumed = view[0].1;
                let count = view.iter().take_while(|(_, end)| *end == consumed).count();
                alternatives.push((bytes[..count].to_vec(), consumed));
                if !b"\\&%+".contains(&bytes[0]) {
                    continue;
                }
                for decoder in [
                    json_token as TokenDecoder,
                    html_token,
                    percent_token,
                    form_token,
                ] {
                    let decoded = decode_spans(&view, decoder);
                    if decoded != view && seen.insert(decoded.clone()) {
                        if depth >= MAX_DEPTH || seen.len() > MAX_VIEWS {
                            return Err(());
                        }
                        views.push_back((decoded, depth + 1));
                    }
                }
            }
            alternatives.sort_unstable();
            alternatives.dedup();
            return Ok(alternatives);
        }
    }
}

fn needs_view(input: &[u8], key_len: usize) -> bool {
    if input
        .iter()
        .any(|b| b"\\%&+".contains(b) || b.is_ascii_whitespace())
    {
        return true;
    }
    let mut run = 0;
    for b in input {
        if b.is_ascii_alphanumeric() || b"+/-_".contains(b) {
            run += 1;
        } else {
            run = 0;
        }
        if run * 3 / 4 >= key_len {
            return true;
        }
    }
    false
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|v| v as u8)
}

fn percent(input: &[u8], form: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%'
            && i + 2 < input.len()
            && let Some((hi, lo)) = hex(input[i + 1]).zip(hex(input[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(if form && input[i] == b'+' {
            b' '
        } else {
            input[i]
        });
        i += 1;
    }
    out
}

type TokenDecoder = fn(&[u8]) -> Option<(Vec<u8>, usize)>;

// Each decoded byte carries the exclusive end offset of its original source.
type Spanned = Vec<(u8, usize)>;

fn decode_spans(input: &Spanned, decoder: TokenDecoder) -> Spanned {
    let bytes: Vec<u8> = input.iter().map(|(b, _)| *b).collect();
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        if let Some((decoded, consumed)) = decoder(&bytes[offset..]) {
            let end = input[offset + consumed - 1].1;
            result.extend(decoded.into_iter().map(|b| (b, end)));
            offset += consumed;
        } else {
            result.push(input[offset]);
            offset += 1;
        }
    }
    result
}

fn percent_token(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    if input.first() != Some(&b'%') {
        return None;
    }
    Some((vec![hex(*input.get(1)?)? * 16 + hex(*input.get(2)?)?], 3))
}

fn form_token(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    if input.first() == Some(&b'+') {
        Some((vec![b' '], 1))
    } else {
        percent_token(input)
    }
}

fn leading_incomplete(input: &[u8]) -> bool {
    match input.first() {
        Some(b'%') => input.len() < 3,
        Some(b'\\') => {
            if input.len() < 2 {
                return true;
            }
            if input[1] != b'u' {
                return false;
            }
            if input.len() < 6 {
                return true;
            }
            let point = input[2..6]
                .iter()
                .try_fold(0_u32, |value, b| Some(value * 16 + u32::from(hex(*b)?)));
            point.is_some_and(|p| (0xd800..=0xdbff).contains(&p)) && input.len() < 12
        }
        Some(b'&') if input.get(1) == Some(&b'#') => {
            let mut i = 2;
            let radix = if input.get(i).is_some_and(|b| matches!(b, b'x' | b'X')) {
                i += 1;
                16
            } else {
                10
            };
            while input.get(i).is_some_and(|b| (*b as char).is_digit(radix)) {
                i += 1;
            }
            i == input.len()
        }
        Some(b'&') => html_ascii::ASCII_ENTITIES
            .iter()
            .any(|(name, _)| name.as_bytes().starts_with(&input[1..])),
        _ => false,
    }
}

fn decode_tokens(input: &[u8], token: TokenDecoder) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if let Some((value, consumed)) = token(&input[i..]) {
            out.extend(value);
            i += consumed;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

fn json_token(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    if input.first() != Some(&b'\\') {
        return None;
    }
    let value = match *input.get(1)? {
        b'"' => b'"',
        b'\\' => b'\\',
        b'/' => b'/',
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'b' => 8,
        b'f' => 12,
        b'u' => {
            let mut point = 0_u32;
            for &b in input.get(2..6)? {
                point = point * 16 + u32::from(hex(b)?);
            }
            let mut consumed = 6;
            if (0xd800..=0xdbff).contains(&point) && input.get(6..8) == Some(b"\\u") {
                let mut low = 0_u32;
                for &b in input.get(8..12)? {
                    low = low * 16 + u32::from(hex(b)?);
                }
                if !(0xdc00..=0xdfff).contains(&low) {
                    return None;
                }
                point = 0x10000 + ((point - 0xd800) << 10) + low - 0xdc00;
                consumed = 12;
            }
            let mut buf = [0; 4];
            return Some((
                char::from_u32(point)?
                    .encode_utf8(&mut buf)
                    .as_bytes()
                    .to_vec(),
                consumed,
            ));
        }
        _ => return None,
    };
    Some((vec![value], 2))
}

fn html_token(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    if input.first() != Some(&b'&') {
        return None;
    }
    if input.get(1) == Some(&b'#') {
        let mut i = 2;
        let radix = if input.get(i).is_some_and(|b| matches!(b, b'x' | b'X')) {
            i += 1;
            16
        } else {
            10
        };
        let start = i;
        let mut point = 0_u32;
        while let Some(digit) = input.get(i).and_then(|b| (*b as char).to_digit(radix)) {
            point = point.checked_mul(radix)?.checked_add(digit)?;
            i += 1;
        }
        if i == start {
            return None;
        }
        if input.get(i) == Some(&b';') {
            i += 1;
        }
        let mut buf = [0; 4];
        return Some((
            char::from_u32(point)?
                .encode_utf8(&mut buf)
                .as_bytes()
                .to_vec(),
            i,
        ));
    }
    html_ascii::ASCII_ENTITIES.iter().find_map(|(name, value)| {
        input[1..]
            .starts_with(name.as_bytes())
            .then(|| (value.as_bytes().to_vec(), name.len() + 1))
    })
}
