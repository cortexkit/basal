//! Parse Rust rather than matching lines: comments, strings and nested test
//! modules must not hide a production use or trigger a false alarm. Unknown
//! cfg conditions are treated as possibly production on some platform.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, Lit, Meta, Token};

// Only test and test-support are known to be absent from a production build.
// Other features and operating systems remain unknown, not false.
fn production_cfg(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(false),
        Meta::NameValue(value) if value.path.is_ident("feature") => {
            if let Expr::Lit(value) = &value.value
                && let Lit::Str(value) = &value.lit
                && value.value() == "test-support"
            {
                Some(false)
            } else {
                None
            }
        }
        Meta::List(list) => {
            let args = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .unwrap();
            let values: Vec<_> = args.iter().map(production_cfg).collect();
            if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|value| *value == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("not") && values.len() == 1 {
                values[0].map(|value| !value)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && production_cfg(&attr.parse_args::<Meta>().unwrap()) == Some(false)
    })
}

#[derive(Default)]
struct Scan {
    seen: BTreeSet<(PathBuf, bool)>,
    violations: BTreeSet<PathBuf>,
}

impl Scan {
    fn file(&mut self, path: &Path, test: bool) {
        let file = syn::parse_file(&std::fs::read_to_string(path).unwrap())
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let test = test || test_only(&file.attrs);
        if !self.seen.insert((path.to_owned(), test)) {
            return;
        }
        let parent = path.parent().unwrap().to_owned();
        let directory = match path.file_stem().unwrap().to_str().unwrap() {
            "lib" | "main" | "mod" => parent.clone(),
            name => parent.join(name),
        };
        Source {
            scan: self,
            path,
            directory,
            attribute_directory: parent,
            test,
        }
        .visit_file(&file);
    }
}

struct Source<'a> {
    scan: &'a mut Scan,
    path: &'a Path,
    directory: PathBuf,
    attribute_directory: PathBuf,
    test: bool,
}

impl<'ast> Visit<'ast> for Source<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs: &[Attribute] = match item {
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Enum(item) => &item.attrs,
            syn::Item::ExternCrate(item) => &item.attrs,
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::ForeignMod(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Macro(item) => &item.attrs,
            syn::Item::Mod(item) => &item.attrs,
            syn::Item::Static(item) => &item.attrs,
            syn::Item::Struct(item) => &item.attrs,
            syn::Item::Trait(item) => &item.attrs,
            syn::Item::TraitAlias(item) => &item.attrs,
            syn::Item::Type(item) => &item.attrs,
            syn::Item::Union(item) => &item.attrs,
            syn::Item::Use(item) => &item.attrs,
            _ => &[],
        };
        let previous = self.test;
        self.test |= test_only(attrs);
        visit::visit_item(self, item);
        self.test = previous;
    }

    fn visit_ident(&mut self, ident: &'ast syn::Ident) {
        if !self.test && ident == "throwaway" {
            self.scan.violations.insert(self.path.to_owned());
        }
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let test = self.test || test_only(&module.attrs);
        self.visit_ident(&module.ident);
        if let Some((_, items)) = &module.content {
            let mut nested = Source {
                scan: self.scan,
                path: self.path,
                directory: self.directory.join(module.ident.to_string()),
                attribute_directory: self.directory.join(module.ident.to_string()),
                test,
            };
            for item in items {
                nested.visit_item(item);
            }
        } else {
            let custom = module.attrs.iter().find_map(|attr| {
                if attr.path().is_ident("path")
                    && let Meta::NameValue(value) = &attr.meta
                    && let Expr::Lit(value) = &value.value
                    && let Lit::Str(value) = &value.lit
                {
                    Some(self.attribute_directory.join(value.value()))
                } else {
                    None
                }
            });
            let path = custom.unwrap_or_else(|| {
                let file = self.directory.join(format!("{}.rs", module.ident));
                if file.exists() {
                    file
                } else {
                    self.directory.join(module.ident.to_string()).join("mod.rs")
                }
            });
            self.scan.file(&path, test);
        }
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if !test_only(&function.attrs) {
            visit::visit_item_fn(self, function);
        }
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        if !test_only(&function.attrs) {
            visit::visit_impl_item_fn(self, function);
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // Macro bodies are token streams rather than parsed expressions.
        fn contains_throwaway(tokens: proc_macro2::TokenStream) -> bool {
            tokens.into_iter().any(|token| match token {
                proc_macro2::TokenTree::Ident(ident) => ident == "throwaway",
                proc_macro2::TokenTree::Group(group) => contains_throwaway(group.stream()),
                _ => false,
            })
        }
        if !self.test && contains_throwaway(mac.tokens.clone()) {
            self.scan.violations.insert(self.path.to_owned());
        }
    }
}

fn source_files(directory: &Path, files: &mut BTreeSet<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.insert(path);
        }
    }
}

fn scan(directory: &Path) -> Scan {
    let mut files = BTreeSet::new();
    source_files(directory, &mut files);
    let mut scan = Scan::default();
    for path in &files {
        let name = path.file_name().unwrap();
        if name == "lib.rs"
            || name == "main.rs"
            || path
                .parent()
                .is_some_and(|parent| parent.ends_with("src/bin"))
        {
            scan.file(path, false);
        }
    }
    // Orphan source files are not silently exempted by their names.
    for path in files {
        if !scan.seen.iter().any(|(seen, _)| *seen == path) {
            scan.file(&path, false);
        }
    }
    scan
}

#[test]
fn production_sources_never_use_throwaway() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
    let mut scanned = 0;
    for entry in std::fs::read_dir(crates).unwrap() {
        let source = entry.unwrap().path().join("src");
        if source.is_dir() {
            let scan = scan(&source);
            scanned += scan.seen.len();
            assert!(
                scan.violations.is_empty(),
                "production uses throwaway: {:?}",
                scan.violations
            );
        }
    }
    assert!(
        scanned > 100,
        "the guard must scan the workspace, not an empty fixture"
    );
}

fn fixture(files: &[(&str, &str)]) -> BTreeSet<PathBuf> {
    let directory = basal_core::test_support::timestamp_scratch("durability-scan", "fixture");
    for (name, source) in files {
        let path = directory.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let result = scan(&directory)
        .violations
        .into_iter()
        .map(|path| path.strip_prefix(&directory).unwrap().to_owned())
        .collect();
    std::fs::remove_dir_all(directory).unwrap();
    result
}

#[test]
fn scan_catches_planted_production_uses_including_macros_and_platform_cfg() {
    for source in [
        "fn main() { Durability::throwaway(); }",
        "#[cfg(any(test, windows))] fn main() { Durability::throwaway(); }",
        "#[cfg(feature = \"other\")] fn main() { Durability::throwaway(); }",
        "macro_rules! store { () => { Durability::throwaway() }; }",
    ] {
        assert_eq!(
            fixture(&[("src/main.rs", source)]),
            BTreeSet::from([PathBuf::from("src/main.rs")])
        );
    }
    assert_eq!(
        fixture(&[("src/lib.rs", "pub fn store() { Durability::throwaway(); }")]),
        BTreeSet::from([PathBuf::from("src/lib.rs")])
    );
}

#[test]
fn scan_allows_only_test_code_and_ignores_comments_and_strings() {
    assert!(
        fixture(&[
            (
                "src/lib.rs",
                r#"
            // Durability::throwaway() is forbidden in production.
            const TEXT: &str = "throwaway";
            #[cfg(test)] mod tests { fn fixture() { Durability::throwaway(); } }
            #[cfg(any(test, feature = "test-support"))] mod support;
            #[cfg(all(target_os = "macos", test))] fn fixture() { Durability::throwaway(); }
        "#
            ),
            (
                "src/support.rs",
                "mod nested; fn fixture() { Durability::throwaway(); }"
            ),
            (
                "src/support/nested.rs",
                "fn fixture() { Durability::throwaway(); }"
            ),
        ])
        .is_empty()
    );
}

#[test]
fn scan_does_not_exempt_a_file_also_reachable_from_production() {
    assert_eq!(
        fixture(&[
            (
                "src/lib.rs",
                "#[cfg(test)] #[path = \"shared.rs\"] mod tests; mod shared;"
            ),
            ("src/shared.rs", "fn fixture() { Durability::throwaway(); }"),
        ]),
        BTreeSet::from([PathBuf::from("src/shared.rs")])
    );
}
