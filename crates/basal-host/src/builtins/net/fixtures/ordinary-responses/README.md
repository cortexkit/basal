# Ordinary HTTP response corpus

These committed response bodies protect authenticated fetches from false
`secret_echoed` results. They use distinct text rather than repeated copies of
one sentence, so normalization cannot hide excessive work by deduplicating every
candidate to the same bytes.

`response_corpus.rs` contains the fixed English word list, deterministic counter
mixer and generators. Every case uses seed **0**. Tests reconstruct the bytes,
check the structure and size, then require an unchanged response for both a
20-byte and a 50-byte printable key. No run-time entropy is used.

| File | Shape | Bytes |
|---|---|---:|
| `prose.txt` | 16 distinct sentences, joined with spaces | 1,398 |
| `text.txt` | Varied line-separated text | 5,081 |
| `descriptions.json` | 20 distinct twelve-word descriptions | 2,588 |
| `page.html` | Varied elements, attributes and prose | 8,932 |
| `jwts.json` | 20 distinct, correctly encoded JWT headers and payloads | 8,670 |
| `large.json` | Larger ordinary description list | 90,061 |

The JWTs use a public empty HMAC-SHA256 fixture signing key. They are synthetic
test data, not credentials or production tokens.

These are pass-through regression inputs. The tests compare the complete body,
not a hash or a summary, so withholding or altering an ordinary response fails.
Their varied content exercises distinct decoding candidates; tests for actual
secret echoes are kept separately in the network built-in's test module.
