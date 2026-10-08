use super::*;

#[test]
fn replacement_mode_never_carries_setuid_or_setgid() {
    assert_eq!(replacement_mode(0o6755), 0o755);
}
