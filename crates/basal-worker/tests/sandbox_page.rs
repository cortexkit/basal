//! The public sandbox page, docs/sandbox.md, must keep stating the limits of
//! the worker's confinement on each operating system. Operators and flow
//! authors read that page to decide what a flow could do if it subverted the
//! engine, so an edit that drops one of these statements fails here.

const PAGE: &str = include_str!("../../../docs/sandbox.md");

/// The text under a `## ` heading, up to the next `## ` heading, with line
/// wrapping collapsed so a phrase can be found across a line break.
fn section(heading: &str) -> String {
    let start = PAGE
        .lines()
        .position(|line| line == format!("## {heading}"))
        .unwrap_or_else(|| panic!("docs/sandbox.md has no `## {heading}` section"));
    let body: Vec<&str> = PAGE
        .lines()
        .skip(start + 1)
        .take_while(|line| !line.starts_with("## "))
        .collect();
    normalize(&body.join("\n"))
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The paragraph (blank-line separated) of `text` that contains `anchor`.
fn paragraph<'a>(text: &'a str, anchor: &str) -> &'a str {
    text.split("\n\n")
        .find(|p| normalize(p).contains(anchor))
        .unwrap_or_else(|| panic!("no paragraph containing {anchor:?}"))
}

fn assert_mentions(text: &str, phrase: &str) {
    assert!(
        text.contains(phrase),
        "docs/sandbox.md must say {phrase:?}; section text: {text}"
    );
}

#[test]
fn linux_section_names_seccomp_as_the_boundary() {
    assert_mentions(&section("Linux"), "seccomp is the Linux boundary");
}

#[test]
fn linux_section_lists_every_landlock_gap_in_one_paragraph() {
    let raw = PAGE
        .split("\n## ")
        .find(|s| s.starts_with("Linux\n"))
        .expect("Linux section");
    let gap = normalize(paragraph(raw, "Landlock alone"));
    for item in [
        "stat",
        "readlink",
        "getxattr",
        "UDP",
        "netlink",
        "path-based Unix sockets",
    ] {
        assert_mentions(&gap, item);
    }
}

#[test]
fn macos_section_says_thread_creation_is_not_blocked() {
    assert_mentions(&section("macOS"), "Thread creation is not blocked on macOS");
}

#[test]
fn macos_section_describes_only_what_the_worker_applies_today() {
    let macos = section("macOS");
    for layer in [
        "worker.sb",
        "deny-by-default",
        "hardened runtime",
        "disclaimed",
    ] {
        assert_mentions(&macos, layer);
    }
}

#[test]
fn pooled_worker_cpu_bound_is_the_wall_deadline_plus_the_idle_retire_limit() {
    let differences = section("Differences between the systems");
    assert_mentions(&differences, "wall-clock deadline");
    assert_mentions(&differences, "idle-retire limit");
}

#[test]
fn a_caught_memory_or_stack_failure_does_not_retire_the_worker() {
    assert_mentions(
        &section("Differences between the systems"),
        "A caught memory or stack failure does not retire the worker",
    );
}

#[test]
fn windows_section_says_confinement_is_not_provided_yet() {
    assert_mentions(
        &section("Windows"),
        "Windows confinement is not provided yet",
    );
}
