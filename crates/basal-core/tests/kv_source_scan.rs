//! Preflight for keeping live kv state in a run's overlay: no other production
//! reader may depend on seeing a run's eager SQLite writes.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Token {
    text: String,
    string: bool,
    group: Option<char>,
    inner: Vec<Token>,
    at: usize,
}

// Keep comments and literal contents out of the Rust token stream. In particular,
// braces in SQL, documentation and character literals must not end a function.
fn lex(source: &str, cursor: &mut usize, end: Option<u8>) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    while *cursor < bytes.len() {
        let at = *cursor;
        let rest = &source[at..];
        let byte = bytes[at];
        if Some(byte) == end {
            *cursor += 1;
            return tokens;
        }
        if byte.is_ascii_whitespace() {
            *cursor += 1;
            continue;
        }
        if rest.starts_with("//") {
            *cursor += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            *cursor += 2;
            let mut depth = 1;
            while depth != 0 {
                assert!(*cursor < bytes.len(), "unterminated Rust comment");
                if source[*cursor..].starts_with("/*") {
                    depth += 1;
                    *cursor += 2;
                } else if source[*cursor..].starts_with("*/") {
                    depth -= 1;
                    *cursor += 2;
                } else {
                    *cursor += source[*cursor..].chars().next().unwrap().len_utf8();
                }
            }
            continue;
        }
        let mut token = Token {
            text: String::new(),
            string: false,
            group: None,
            inner: Vec::new(),
            at,
        };
        if let Some(close) = match byte {
            b'(' => Some(b')'),
            b'[' => Some(b']'),
            b'{' => Some(b'}'),
            _ => None,
        } {
            *cursor += 1;
            token.group = Some(char::from(byte));
            token.inner = lex(source, cursor, Some(close));
        } else if let Some(prefix) = ["r", "br", "cr"].into_iter().find(|prefix| {
            rest.strip_prefix(prefix)
                .is_some_and(|tail| tail.trim_start_matches('#').starts_with('"'))
        }) {
            let hashes = rest[prefix.len()..]
                .bytes()
                .take_while(|b| *b == b'#')
                .count();
            let start = at + prefix.len() + hashes + 1;
            let closing = format!("\"{}", "#".repeat(hashes));
            let length = source[start..].find(&closing).expect("raw string end");
            token.text = source[start..start + length].to_owned();
            token.string = true;
            *cursor = start + length + closing.len();
        } else if byte == b'"' || (byte == b'\'' && char_literal_length(rest).is_some()) {
            let quote = byte;
            *cursor += 1;
            while bytes.get(*cursor).is_some_and(|b| *b != quote) {
                if bytes[*cursor] == b'\\' {
                    *cursor += 1;
                    let escaped = bytes[*cursor];
                    *cursor += 1;
                    match escaped {
                        b'x' | b'u' => {
                            let start = *cursor;
                            let digits = if escaped == b'x' {
                                *cursor += 2;
                                &source[start..*cursor]
                            } else {
                                assert_eq!(bytes[*cursor], b'{');
                                *cursor += 1;
                                let start = *cursor;
                                *cursor += source[start..].find('}').expect("unicode escape end");
                                let digits = &source[start..*cursor];
                                *cursor += 1;
                                digits
                            };
                            let value = u32::from_str_radix(&digits.replace('_', ""), 16).unwrap();
                            token.text.push(char::from_u32(value).unwrap());
                        }
                        b'\n' | b'\r' => {
                            while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
                                *cursor += 1;
                            }
                        }
                        b'n' => token.text.push('\n'),
                        b'r' => token.text.push('\r'),
                        b't' => token.text.push('\t'),
                        b'0' => token.text.push('\0'),
                        other => token.text.push(char::from(other)),
                    }
                } else {
                    let character = source[*cursor..].chars().next().unwrap();
                    token.text.push(character);
                    *cursor += character.len_utf8();
                }
            }
            assert_eq!(bytes.get(*cursor), Some(&quote), "literal end");
            *cursor += 1;
            token.string = quote == b'"';
        } else if byte.is_ascii_alphanumeric() || byte == b'_' {
            *cursor += 1;
            while bytes
                .get(*cursor)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                *cursor += 1;
            }
            token.text = source[at..*cursor].to_owned();
        } else {
            let character = rest.chars().next().unwrap();
            token.text.push(character);
            *cursor += character.len_utf8();
        }
        tokens.push(token);
    }
    assert!(end.is_none(), "unterminated Rust group");
    tokens
}

fn char_literal_length(source: &str) -> Option<usize> {
    let mut chars = source[1..].char_indices();
    let (_, first) = chars.next()?;
    if first == '\\' {
        // Escaped character literals contain no unescaped quote before their end.
        chars.next()?;
        chars.find(|(_, c)| *c == '\'').map(|(i, _)| i + 2)
    } else {
        let (index, next) = chars.next()?;
        (next == '\'').then_some(index + 2)
    }
}

// Unknown platform/feature cfgs remain production-reachable. Only a condition
// known to be false with test and test-support disabled can exempt an item.
fn production_cfg(tokens: &[Token]) -> Option<bool> {
    match tokens {
        [name] if name.text == "test" => Some(false),
        [name, eq, value]
            if name.text == "feature" && eq.text == "=" && value.text == "test-support" =>
        {
            Some(false)
        }
        [name, args] if args.group == Some('(') => {
            let values: Vec<_> = args
                .inner
                .split(|t| t.text == ",")
                .filter(|part| !part.is_empty())
                .map(production_cfg)
                .collect();
            match name.text.as_str() {
                "all" if values.contains(&Some(false)) => Some(false),
                "all" if values.iter().all(|v| *v == Some(true)) => Some(true),
                "any" if values.contains(&Some(true)) => Some(true),
                "any" if values.iter().all(|v| *v == Some(false)) => Some(false),
                "not" if values.len() == 1 => values[0].map(|v| !v),
                _ => None,
            }
        }
        _ => None,
    }
}

#[derive(Default, Debug)]
struct Scan {
    dropped_tables: bool,
    checked_sql: usize,
    visited: BTreeSet<(PathBuf, bool)>,
    violations: Vec<String>,
    sql_reads: usize,
    run_calls: usize,
    run_local_reads: usize,
    terminal_overlay_writes: usize,
}

impl Scan {
    fn file(&mut self, path: &Path, test: bool) {
        let path = path
            .canonicalize()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        if !self.visited.insert((path.clone(), test)) {
            return;
        }
        let source = fs::read_to_string(&path).unwrap();
        if path.extension().is_some_and(|ext| ext == "sql") {
            if test { return; }
            self.sql(
                &path,
                &source,
                &Token {
                    text: source.clone(),
                    string: true,
                    group: None,
                    inner: vec![],
                    at: 0,
                },
                "",
            );
            return;
        }
        let directory = if matches!(
            path.file_name().unwrap().to_str().unwrap(),
            "lib.rs" | "main.rs" | "mod.rs"
        ) {
            path.parent().unwrap().to_owned()
        } else {
            path.with_extension("")
        };
        self.tokens(
            &path,
            &source,
            &lex(&source, &mut 0, None),
            &directory,
            test,
            "",
        );
    }

    fn report(&mut self, path: &Path, source: &str, token: &Token, reason: &str) {
        let line = source[..token.at].bytes().filter(|b| *b == b'\n').count() + 1;
        self.violations
            .push(format!("{}:{line}: {reason}", path.display()));
    }

    fn tokens(
        &mut self,
        path: &Path,
        source: &str,
        tokens: &[Token],
        directory: &Path,
        test: bool,
        function: &str,
    ) {
        let mut index = 0;
        while index < tokens.len() {
            let mut item_test = test;
            let mut module_path = None;
            while tokens.get(index).is_some_and(|t| t.text == "#") {
                let Some(attribute) = tokens.get(index + 1).filter(|t| t.group == Some('[')) else {
                    break;
                };
                match attribute.inner.as_slice() {
                    [name, args] if name.text == "cfg" && args.group == Some('(') => {
                        item_test |= production_cfg(&args.inner) == Some(false);
                    }
                    [name, eq, value] if name.text == "path" && eq.text == "=" && value.string => {
                        module_path = Some(value.text.as_str());
                    }
                    _ => {}
                }
                index += 2;
            }
            let Some(token) = tokens.get(index) else {
                break;
            };
            // Attributes precede visibility and qualifiers, not necessarily `mod`/`fn`.
            let item_end = tokens[index..]
                .iter()
                .position(|t| t.group == Some('{') || t.text == ";")
                .map(|offset| index + offset)
                .unwrap_or(tokens.len());
            let header = &tokens[index..item_end];
            let module = header
                .windows(2)
                .find(|pair| pair[0].text == "mod")
                .map(|pair| pair[1].text.as_str());
            if let Some(name) = module {
                if tokens.get(item_end).is_some_and(|t| t.group == Some('{')) {
                    let nested = directory.join(name);
                    self.tokens(
                        path,
                        source,
                        &tokens[item_end].inner,
                        &nested,
                        item_test,
                        function,
                    );
                } else {
                    let target = if let Some(relative) = module_path {
                        path.parent().unwrap().join(relative)
                    } else {
                        let plain = directory.join(format!("{name}.rs"));
                        if plain.exists() {
                            plain
                        } else {
                            directory.join(name).join("mod.rs")
                        }
                    };
                    self.file(&target, item_test);
                }
                index = item_end + 1;
                continue;
            }
            if item_test && !test {
                index = item_end + 1;
                continue;
            }
            if token.text == "fn" && tokens.get(index + 1).is_some() {
                let name = &tokens[index + 1].text;
                if let Some(body) = tokens.get(item_end).filter(|t| t.group == Some('{')) {
                    self.tokens(path, source, &body.inner, directory, test, name);
                }
                index = item_end + 1;
                continue;
            }
            if !test
                && self.dropped_tables
                && !migration_path(path)
                && token.text == "pragma_update"
                && let Some(args) = tokens.get(index + 1).filter(|t| t.group == Some('('))
            {
                for name in args
                    .inner
                    .iter()
                    .filter(|t| t.string && matches!(t.text.as_str(), "journal_mode" | "page_size"))
                {
                    self.report(
                        path,
                        source,
                        name,
                        "SQLite layout maintenance outside cutover/fresh initialization",
                    );
                }
            }
            if !test && token.string {
                self.sql(path, source, token, function);
            }
            if token.text == "include_str"
                && tokens.get(index + 1).is_some_and(|t| t.text == "!")
                && let Some(args) = tokens.get(index + 2).filter(|t| t.group == Some('('))
                && let Some(file) = args.inner.first().filter(|t| t.string)
            {
                self.file(&path.parent().unwrap().join(&file.text), test);
            }
            if !test && !self.dropped_tables {
                if token.text == "use"
                    && import_contains_kv(header)

                    && !safe_kv_import(header)
                {
                    self.report(
                        path,
                        source,
                        token,
                        "kv reader import outside the audited run driver",
                    );
                }
                // Audit every path to the reader, including imports/aliases. Other
                // kv exports classify calls, construct rejections or define limits.
                if !token.string && token.text == "apply" && path.ends_with("basal-core/src/kv.rs")
                {
                    self.report(
                        path,
                        source,
                        token,
                        "unqualified kv reader reference outside the audited run driver",
                    );
                }
                if token.text == "kv"
                    && tokens.get(index + 1).is_some_and(|t| t.text == ":")
                    && tokens.get(index + 2).is_some_and(|t| t.text == ":")
                {
                    let member = tokens.get(index + 3).map(|t| t.text.as_str());
                    if !matches!(member, Some("KvLimits" | "is_kv" | "rejection")) {
                        self.report(
                            path,
                            source,
                            token,
                            "kv reader path outside the audited log-backed driver",
                        );
                    }
                }
            }
            if token.group.is_some() {
                self.tokens(path, source, &token.inner, directory, test, function);
            }
            index += 1;
        }
    }

    fn sql(&mut self, path: &Path, source: &str, token: &Token, function: &str) {
        if self.dropped_tables {
            if migration_path(path) {
                return;
            }
            let words = sql_words(&token.text);
            let sql = words.first().is_some_and(|w| {
                matches!(
                    w.as_str(),
                    "select"
                        | "from"
                        | "join"
                        | "insert"
                        | "update"
                        | "delete"
                        | "create"
                        | "drop"
                        | "alter"
                        | "with"
                        | "pragma"
                        | "vacuum"
                        | "exists"
                        | "where"
                        | "union"
                        | "order"
                        | "returning"
                )
            });
            if sql {
                self.checked_sql += 1;
                if words.iter().any(|w| w == "vacuum")
                    || words.windows(2).any(|p| {
                        p[0] == "pragma" && matches!(p[1].as_str(), "page_size" | "journal_mode")
                    })
                {
                    self.report(
                        path,
                        source,
                        token,
                        "SQLite layout maintenance outside cutover/fresh initialization",
                    );
                }
                if let Some(table) = dropped_relation(&words) {
                    self.report(
                        path,
                        source,
                        token,
                        &format!("SQL references dropped table or view {table}"),
                    );
                }
            }
            return;
        }
        // Journal labels name operations, not SQL tables.
        if matches!(token.text.as_str(), "kv.get" | "kv.set" | "kv.delete") {
            return;
        }
        let words: Vec<_> = token
            .text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        if !words.iter().any(|word| word == "kv") {
            return;
        }
        let sql = words.iter().any(|word| {
            matches!(
                word.as_str(),
                "select" | "from" | "join" | "insert" | "update" | "delete" | "table"
            )
        });
        if !sql && words != ["kv"] {
            return;
        }

        // The flow's single-run slot excludes other runs while the owner reads its own kv.
        if path.ends_with("basal-core/src/run_driver/local.rs") && function == "prepare" {
            self.run_local_reads += 1;
            return;
        }
        // The run's overlay commits in the same transaction as its summary.
        if path.ends_with("basal-core/src/run_driver/terminal.rs") && function == "apply" {
            self.terminal_overlay_writes += 1;
            return;
        }
        // These exact write-only statements are not readers. Do not exempt their
        // files: a SELECT added to an epoch reset or migration still needs review.
        if (path.ends_with("basal-core/src/state_epoch.rs")
            && function == "reset"
            && words == ["delete", "from", "kv", "where", "flow_id", "1"])
            || (path.ends_with("basal-core/src/run_cutover/schema.rs")
                && token.text.contains("CREATE TABLE kv (")
                && words.iter().filter(|word| word.as_str() == "kv").count() == 1)
        {
            return;
        }
        self.report(
            path,
            source,
            token,
            "kv SQL outside the audited run-local or terminal-overlay functions",
        );
    }
}

fn import_contains_kv(tokens: &[Token]) -> bool {
    tokens
        .iter()
        .any(|token| (!token.string && token.text == "kv") || import_contains_kv(&token.inner))
}

fn safe_kv_import(tokens: &[Token]) -> bool {
    tokens.iter().enumerate().all(|(index, token)| {
        (token.text != "kv"
            || matches!(
                tokens.get(index + 3).map(|t| t.text.as_str()),
                Some("KvLimits" | "rejection" | "is_kv")
            ))
            && safe_kv_import(&token.inner)
    })
}

// The runtime scan covers every production crate. Keep this inventory only
// to assert that the owning execution modules and their included SQL are visited.
const NEW_PATH_ROOTS: &[&str] = &[
    "run_admission.rs",
    "run_decisions.rs",
    "run_driver/mod.rs",
    "run_host.rs",
    "run_log/mod.rs",
    "run_readers.rs",
    "run_records/mod.rs",
    "run_retention.rs",
    "run_store/mod.rs",
    "run_store_sql.rs",
    "staged_runtime.rs",
];
const DROPPED_TABLES: &[&str] = &[
    "runs",
    "journal",
    "journal_payloads",
    "mailbox",
    "call_audit",
    "activations",
    "event_body_journal",
    "journal_with_payloads",
    "runs_with_snapshots",
];

fn dropped_relation(words: &[String]) -> Option<&str> {
    for (i, word) in words.iter().enumerate() {
        let trigger_on = word == "on"
            && words[..i]
                .iter()
                .rposition(|w| w == "trigger")
                .is_some_and(|at| {
                    words[..i]
                        .iter()
                        .rposition(|w| w == "begin")
                        .is_none_or(|begin| begin < at)
                });
        if !trigger_on
            && !matches!(
                word.as_str(),
                "from" | "join" | "into" | "update" | "references" | "table" | "view" | "trigger"
            )
        {
            continue;
        }
        let mut next = i + 1;
        while words.get(next).is_some_and(|w| {
            matches!(
                w.as_str(),
                "if" | "not" | "exists" | "only" | "main" | "temp"
            )
        }) {
            next += 1;
        }
        if let Some(name) = words
            .get(next)
            .filter(|w| DROPPED_TABLES.contains(&w.as_str()))
        {
            return Some(name);
        }
    }
    None
}

fn migration_path(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_str().unwrap();
    (name.starts_with("schema_")
        && name.ends_with(".sql")
        && path
            .parent()
            .is_some_and(|p| p.ends_with("crates/basal-core/src")))
        || path.ends_with("crates/basal-core/src/run_cutover.rs")
        || [
            "audit.rs",
            "convert.rs",
            "legacy.rs",
            "pages.rs",
            "schema.rs",
        ]
        .iter()
        .any(|name| path.ends_with(format!("crates/basal-core/src/run_cutover/{name}")))
}

// SQL string values and comments are not identifiers. Double-quoted, backtick
// and bracketed identifiers still yield words, including schema-qualified names.
fn sql_words(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut words = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let identifier = words.last().is_some_and(|word: &String| {
                matches!(
                    word.as_str(),
                    "from" | "join" | "into" | "update" | "table" | "view" | "trigger"
                )
            }) || sql[..i].trim_end().ends_with('.');
            i += 1;
            let start = i;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    let end = i;
                    i += 1;
                    if bytes.get(i) == Some(&b'\'') {
                        i += 1;
                    } else {
                        if identifier {
                            words.push(sql[start..end].replace("''", "'").to_ascii_lowercase());
                        }
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if bytes[i..].starts_with(b"--") {
            i += bytes[i..]
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(bytes.len() - i);
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            words.push(sql[start..i].to_ascii_lowercase());
        } else {
            i += 1;
        }
    }
    words
}

fn scan_runtime(root: &Path) -> Scan {
    scan_sources(root, true)
}

#[test]
fn dropped_table_guard_covers_all_production_sql() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    for module in NEW_PATH_ROOTS {
        assert!(
            root.join("crates/basal-core/src").join(module).exists(),
            "missing guarded root {module}"
        );
    }
    let scan = scan_runtime(root);
    for name in ["basal-core","basal-host","basal-module","basal-launch","basal-worker","basal-proto"] {
        let directory=root.join("crates").join(name).join("src");
        assert!(scan.visited.iter().any(|(path,test)|!test&&path.starts_with(&directory)),"missing production crate {name}");
    }
    assert!(
        scan.checked_sql > 100,
        "unexpectedly empty SQL scan: {scan:?}"
    );
    assert!(scan.violations.is_empty(), "{}", scan.violations.join("\n"));
    assert!(
        scan.visited
            .iter()
            .any(|(path, test)| !test && path.ends_with("run_driver/terminal.rs"))
    );
}

#[test]
fn dropped_table_guard_planted_runtime_violations_fail() {
    let mut paths:Vec<String>=NEW_PATH_ROOTS.iter().map(|m|format!("basal-core/src/{m}")).collect();
    paths.extend(["basal-host","basal-module","basal-worker","basal-launch","basal-proto"].map(|name|format!("{name}/src/runtime.rs")));
    for path in paths {
        for table in DROPPED_TABLES {
            for quoted in [format!("\"{table}\""),format!("'{table}'"),format!("[{table}]"),format!("`{table}`")] {
                let sql=format!("SELECT * FROM main.{quoted}");
                let source=format!("pub fn planted() {{ query({sql:?}); }}");
                let fixture=Fixture::new(&[(&path,&source)]);
                let scan=scan_runtime(&fixture.0);
                assert_eq!(scan.violations.len(),1,"missed {path} / {table}: {scan:?}");
                assert!(scan.violations[0].contains(table));
            }
        }
    }
}

#[test]
fn dropped_names_as_columns_and_non_sql_metadata_are_not_table_references() {
    let fixture=Fixture::new(&[("basal-module/src/reader.rs",r#"fn read(){query("SELECT runs FROM rate_windows");query("SELECT 1 AS journal FROM run_history");json!({"runs":[]});}"#)]);
    assert!(scan_runtime(&fixture.0).violations.is_empty());
}

#[test]
fn dropped_table_guard_migration_negative_control_passes() {
    let fixture=Fixture::new(&[
        ("basal-core/src/lib.rs",r#"mod run_cutover; const SQL:&str=include_str!("schema_cutover.sql");"#),
        ("basal-core/src/run_cutover.rs",r#"fn migrate(){execute("INSERT INTO run_history SELECT * FROM runs");}"#),
        ("basal-core/src/schema_cutover.sql","DROP VIEW journal_with_payloads; DROP TABLE journal;"),
    ]);
    assert!(scan_runtime(&fixture.0).violations.is_empty());
    fs::write(fixture.0.join("crates/basal-core/src/runtime_query.sql"),"SELECT * FROM mailbox").unwrap();
    fs::write(fixture.0.join("crates/basal-core/src/lib.rs"),r#"const SQL:&str=include_str!("runtime_query.sql");"#).unwrap();
    assert_eq!(scan_runtime(&fixture.0).violations.len(),1);
}

#[test]
fn layout_maintenance_is_confined_to_cutover_and_used_by_fresh_initialization() {
    for statement in [r#"execute("VACUUM")"#,r#"execute("PRAGMA page_size=1024")"#,r#"execute("PRAGMA journal_mode=DELETE")"#,r#"c.pragma_update(None,"page_size",1024)"#,r#"c.pragma_update(None,"journal_mode","DELETE")"#] {
        let source=format!("pub fn maintenance(){{{statement};}}");
        for path in ["basal-core/src/reader.rs","basal-module/src/reader.rs","basal-core/src/run_cutover/accidental_runtime.rs"] {
            let fixture=Fixture::new(&[(path,&source)]);
            assert_eq!(scan_runtime(&fixture.0).violations.len(),1,"{path}: {statement}");
        }
        let fixture=Fixture::new(&[("basal-core/src/run_cutover/pages.rs",&source)]);
        assert!(scan_runtime(&fixture.0).violations.is_empty(),"{statement}");
    }
}

#[test]
fn dropped_table_guard_follows_modules_and_only_exempts_test_cfgs() {
    let fixture = Fixture::new(&[
        (
            "basal-core/src/run_retention.rs",
            r##"
            // query("SELECT * FROM journal");
            #[cfg(test)] fn helper() { query("SELECT * FROM runs"); }
            #[cfg(any(test, feature="test-support"))] mod tests;
            #[cfg(unix)] fn platform() { query("SELECT * FROM [mailbox]"); }
            #[cfg(any(test,unix))] fn mixed() { query("SELECT * FROM `journal`"); }
            mod nested { fn runtime() { query("CREATE VIEW bad AS SELECT * FROM activations"); } }
            const SQL: &str = concat!("SELECT * FROM ", "call_audit");
            fn safe() { query("SELECT 'runs', 'journal' FROM run_history -- mailbox"); }
        "##,
        ),
        (
            "basal-core/src/run_retention/tests.rs",
            "fn helper() { query(\"SELECT * FROM runs\"); }",
        ),
    ]);
    let scan = scan_runtime(&fixture.0);
    assert_eq!(scan.violations.len(), 4, "{scan:?}");
}

fn source_files(directory: &Path, files: &mut BTreeSet<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "rs" || extension == "sql")
        {
            files.insert(path.canonicalize().unwrap());
        }
    }
}

fn scan(root: &Path) -> Scan {
    scan_sources(root, false)
}

fn scan_sources(root: &Path, dropped_tables: bool) -> Scan {
    let mut files = BTreeSet::new();
    let mut scan = Scan {
        dropped_tables,
        ..Scan::default()
    };
    for name in [
        "basal-core",
        "basal-host",
        "basal-module",
        "basal-launch",
        "basal-worker",
        "basal-proto",
    ] {
        let directory = root.join("crates").join(name).join("src");
        if !directory.exists() {
            continue;
        }
        source_files(&directory, &mut files);
        if directory.join("lib.rs").exists() {
            scan.file(&directory.join("lib.rs"), false);
        }
    }
    // Unlinked files and binary entry points are not silently exempt. A file
    // visited through both a test and production module is checked as production.
    for path in files {
        if path.extension().unwrap() == "sql" && !scan.visited.iter().any(|(seen,_)|seen==&path) {
            let source = fs::read_to_string(&path).unwrap();
            scan.sql(
                &path,
                &source,
                &Token {
                    text: source.clone(),
                    string: true,
                    group: None,
                    inner: Vec::new(),
                    at: 0,
                },
                "",
            );
        } else if !scan.visited.iter().any(|(seen, _)| seen == &path) {
            scan.file(&path, false);
        }
    }
    scan
}

#[test]
fn production_kv_reads_are_run_local() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let scan = scan(root);
    assert!(
        scan.visited.iter().filter(|(_, test)| !test).count() > 100,
        "scan did not cover the production source trees"
    );
    assert_eq!(scan.sql_reads, 0, "the retired eager kv SQL reader returned");
    assert_eq!(
        scan.run_local_reads, 1,
        "the audited run-local kv read changed"
    );
    assert_eq!(
        scan.terminal_overlay_writes, 3,
        "the audited terminal-overlay kv writes changed"
    );
    assert_eq!(
        scan.run_calls, 0,
        "retired eager kv calls returned"
    );
    assert!(
        scan.violations.is_empty(),
        "production kv reader found outside a run; stop and ask before continuing:\n{}",
        scan.violations.join("\n")
    );
}

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "kv-source-scan-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        for name in ["basal-core", "basal-host", "basal-module"] {
            let directory = root.join("crates").join(name).join("src");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("lib.rs"), "").unwrap();
        }
        for (path, source) in files {
            let path = root.join("crates").join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn scan_catches_planted_out_of_run_kv_reads_in_all_three_crates() {
    for name in ["basal-core", "basal-host", "basal-module"] {
        for sql in [
            "SELECT value FROM kv",
            "sElEcT value\nFROM main.\"kv\"",
            "SELECT * FROM runs JOIN [KV] USING(flow_id)",
            "SELECT * FROM runs, `kv`",
            "SELECT * FROM kv /* a comment */",
        ] {
            let source = format!("fn preview() {{ query(r###\"{sql}\"###); }}");
            let path = format!("{name}/src/lib.rs");
            let fixture = Fixture::new(&[(&path, &source)]);
            let scan = scan(&fixture.0);
            assert_eq!(scan.violations.len(), 1, "{name}: {sql}: {scan:?}");
            assert!(
                scan.violations[0]
                    .replace('\\', "/")
                    .contains(&format!("{name}/src/lib.rs:1:"))
            );
        }
    }
}

#[test]
fn scan_allows_the_named_run_driver_kv_sites_with_exact_counts() {
    let fixture = Fixture::new(&[
        (
            "basal-core/src/run_driver/local.rs",
            r#"fn prepare() { query("SELECT key,value FROM kv WHERE flow_id=?1"); }"#,
        ),
        (
            "basal-core/src/run_driver/terminal.rs",
            r#"
            impl Projection {
                fn apply() {
                    execute("DELETE FROM kv WHERE flow_id=?1 AND key=?2");
                    execute("INSERT INTO kv(flow_id,key,value) VALUES(?1,?2,?3)");
                    execute("DELETE FROM kv WHERE flow_id=?1 AND key=?2");
                }
            }
            "#,
        ),
    ]);
    let scan = scan(&fixture.0);
    assert!(scan.violations.is_empty(), "{scan:?}");
    assert_eq!(scan.sql_reads, 0);
    assert_eq!(scan.run_calls, 0);
    assert_eq!(scan.run_local_reads, 1);
    assert_eq!(scan.terminal_overlay_writes, 3);
}

#[test]
fn scan_catches_planted_kv_reads_elsewhere_in_run_driver() {
    for (path, source) in [
        (
            "basal-core/src/run_driver/preview.rs",
            r#"fn prepare() { query("SELECT value FROM kv"); }"#,
        ),
        (
            "basal-core/src/run_driver/local.rs",
            r#"fn preview() { query("SELECT value FROM kv"); }"#,
        ),
        (
            "basal-core/src/run_driver/terminal.rs",
            r#"fn preview() { query("SELECT value FROM kv"); }"#,
        ),
        (
            "basal-core/src/run_driver/local.rs",
            r#"fn prepare() { fn preview() { query("SELECT value FROM kv"); } }"#,
        ),
    ] {
        let fixture = Fixture::new(&[(path, source)]);
        let scan = scan(&fixture.0);
        assert_eq!(scan.violations.len(), 1, "missed {path}: {scan:?}");
        assert!(scan.violations[0].replace('\\', "/").contains(path));
        assert_eq!(scan.run_local_reads, 0);
        assert_eq!(scan.terminal_overlay_writes, 0);
    }
}

#[test]
fn scan_does_not_exempt_reader_files_or_import_aliases() {
    for (path, source) in [
        (
            "basal-core/src/kv.rs",
            "fn preview() { query(\"SELECT value FROM kv\"); }",
        ),
        ("basal-core/src/kv.rs", "fn rejection() { apply(); }"),
        (
            "basal-core/src/kv.rs",
            "fn rejection() { let reader = self::apply; }",
        ),
        ("basal-core/src/driver.rs", "fn preview() { kv::apply(); }"),
        (
            "basal-module/src/lib.rs",
            "use basal_core::kv::apply as read; fn preview() { read(); }",
        ),
        (
            "basal-module/src/lib.rs",
            "use basal_core::kv as state; fn preview() { state::apply(); }",
        ),
        (
            "basal-module/src/lib.rs",
            "use basal_core::{kv}; fn preview() { kv::apply(); }",
        ),
        (
            "basal-core/src/state_epoch.rs",
            "fn reset() { query(\"SELECT value FROM kv\"); }",
        ),
        (
            "basal-core/src/schema.rs",
            "const SQL: &str = \"SELECT value FROM kv\";",
        ),
        (
            "basal-module/src/reader_tests.rs",
            "fn preview() { query(\"SELECT value FROM kv\"); }",
        ),
        ("basal-module/src/reader.sql", "SELECT value FROM kv;"),
        (
            "basal-module/src/lib.rs",
            r#"fn preview() { query(concat!("SELECT value FROM ", "kv")); }"#,
        ),
        (
            "basal-module/src/lib.rs",
            r#"fn preview() { query("SELECT value FROM k\x76"); }"#,
        ),
    ] {
        let fixture = Fixture::new(&[(path, source)]);
        assert!(
            !scan(&fixture.0).violations.is_empty(),
            "missed {path}: {source}"
        );
    }
}

#[test]
fn scan_allows_only_provably_test_only_code_and_not_platform_cfgs() {
    let fixture = Fixture::new(&[
        (
            "basal-module/src/lib.rs",
            r##"
            // query("SELECT value FROM kv");
            /* kv::apply(); /* nested */ */
            fn docs() { let text = "kv::apply"; let brace = '}'; let byte = b'{'; }
            struct Config { kv: KvLimits }
            fn config() { Config { kv: KvLimits::default() }; "kv.delete"; }
            #[cfg(test)]
            mod inline { fn preview() { query("SELECT value FROM kv"); } }
            #[cfg(all(unix, any(test, feature = "test-support")))]
            #[path = "reader_tests.rs"] mod tests;
            #[cfg(not(not(test)))]
            fn test_helper() { kv::apply(); }
        "##,
        ),
        (
            "basal-module/src/reader_tests.rs",
            "fn preview() { query(\"SELECT value FROM kv\"); }",
        ),
    ]);
    assert!(scan(&fixture.0).violations.is_empty());

    for cfg in [
        "unix",
        "any(test, unix)",
        "not(test)",
        "feature = \"production\"",
    ] {
        let source = format!("#[cfg({cfg})] fn preview() {{ query(\"SELECT value FROM kv\"); }}");
        let fixture = Fixture::new(&[("basal-module/src/lib.rs", &source)]);
        assert_eq!(scan(&fixture.0).violations.len(), 1, "exempted {cfg}");
    }
}

#[test]
fn scan_checks_files_reachable_from_both_tests_and_production() {
    let fixture = Fixture::new(&[
        (
            "basal-module/src/lib.rs",
            r#"
            #[cfg(test)] #[path = "shared.rs"] mod tests;
            #[path = "shared.rs"] mod shared;
        "#,
        ),
        (
            "basal-module/src/shared.rs",
            "fn preview() { query(\"SELECT value FROM kv\"); }",
        ),
    ]);
    assert_eq!(scan(&fixture.0).violations.len(), 1);
}

#[test]
fn scan_allows_write_only_reset_and_migration_schema() {
    let fixture=Fixture::new(&[
        ("basal-core/src/state_epoch.rs",r#"fn reset() { execute("DELETE FROM kv WHERE flow_id=?1"); }"#),
        ("basal-core/src/run_cutover/schema.rs",r#"const SQL:&str="CREATE TABLE kv (key TEXT);";"#),
    ]);
    let scan=scan(&fixture.0);
    assert!(scan.violations.is_empty(),"{scan:?}");
    assert_eq!(scan.sql_reads,0);
    assert_eq!(scan.run_calls,0);
}

#[test]
fn scan_rejects_retired_eager_kv_reader_and_former_driver_allowances() {
    let fixture=Fixture::new(&[
        ("basal-core/src/kv.rs",r#"fn apply(){query("SELECT value FROM kv");}"#),
        ("basal-core/src/driver.rs",r#"use crate::kv; fn recover_calls(){kv::apply();} fn commit_local_call(){kv::apply();}"#),
    ]);
    let scan=scan(&fixture.0);
    assert_eq!(scan.violations.len(),4,"{scan:?}");
    assert_eq!(scan.sql_reads,0);
    assert_eq!(scan.run_calls,0);
}
