//! The check of the worker's actual primary token (startup step 4).
//!
//! The parent builds the primary and checks it on the suspended process, but
//! the system changes a token at creation (process creation makes an
//! AppContainer primary Low whatever integrity was supplied), and the worker
//! lowers its own integrity after load. So the worker reads its token back
//! once more, after every change, and judges what it reads rather than what
//! anyone meant to build.
//!
//! The readers report each property separately, and a property that could
//! not be read fails the check that needed it. The checks run in a fixed
//! order and the first failure is the reason.

use super::{Refusal, reason};

/// The NULL SID, the only restricting SID the primary may have.
pub const NULL_SID: &str = "S-1-0-0";
/// The Untrusted mandatory label the worker lowers itself to.
pub const UNTRUSTED_INTEGRITY: &str = "S-1-16-0";
/// The claim that marks a Less Privileged AppContainer token.
pub const LPAC_CLAIM: &str = "WIN://NOALLAPPPKG";
/// `SE_GROUP_INTEGRITY`: marks the integrity label, which is not an access
/// group.
pub const SE_GROUP_INTEGRITY: u32 = 0x20;
/// `SE_GROUP_USE_FOR_DENY_ONLY`: the group matches only deny entries.
pub const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x10;

/// The `WIN://NOALLAPPPKG` security attribute as read from the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The token has no attribute of that name.
    Absent,
    /// The attribute holds unsigned 64-bit values.
    Unsigned(Vec<u64>),
    /// The attribute holds values of another type, by its type number.
    OtherType(u16),
}

/// One access group: its SID and attribute flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub sid: String,
    pub attributes: u32,
}

/// What was read from the primary. Each field is the property, or why it
/// could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryFacts {
    pub lpac_claim: Result<Claim, String>,
    /// The token's AppContainer SID, if it is an AppContainer token, and
    /// whether it is equal (`EqualSid`) to the `--package-sid` argument.
    pub package: Result<Option<(String, bool)>, String>,
    pub capabilities: Result<Vec<String>, String>,
    pub restricting_sids: Result<Vec<String>, String>,
    pub groups: Result<Vec<Group>, String>,
    pub privileges: Result<usize, String>,
    pub integrity: Result<String, String>,
}

/// The property, or the named refusal of the check that could not read it.
fn read<'a, T>(reason: &'static str, result: &'a Result<T, String>) -> Result<&'a T, Refusal> {
    result
        .as_ref()
        .map_err(|error| Refusal::new(reason, format!("could not read: {error}")))
}

/// Runs the six token checks in order and returns the first failure.
pub fn check(facts: &PrimaryFacts) -> Result<(), Refusal> {
    let claim = read(reason::NOT_LPAC, &facts.lpac_claim)?;
    if !matches!(claim, Claim::Unsigned(values) if values.as_slice() == [1]) {
        return Err(Refusal::new(
            reason::NOT_LPAC,
            format!("{LPAC_CLAIM} is {claim:?}, not [1]"),
        ));
    }
    match read(reason::NOT_LPAC, &facts.package)? {
        Some((_, true)) => {}
        Some((sid, false)) => {
            return Err(Refusal::new(
                reason::NOT_LPAC,
                format!("AppContainer SID {sid} is not the --package-sid"),
            ));
        }
        None => return Err(Refusal::new(reason::NOT_LPAC, "not an AppContainer token")),
    }

    let capabilities = read(reason::CAPABILITIES_PRESENT, &facts.capabilities)?;
    if !capabilities.is_empty() {
        return Err(Refusal::new(
            reason::CAPABILITIES_PRESENT,
            format!("capabilities {capabilities:?}"),
        ));
    }

    let sids = read(reason::RESTRICTING_SID_MISMATCH, &facts.restricting_sids)?;
    if sids.as_slice() != [NULL_SID] {
        return Err(Refusal::new(
            reason::RESTRICTING_SID_MISMATCH,
            format!("restricting SIDs {sids:?}, not [{NULL_SID}]"),
        ));
    }

    let groups = read(reason::GROUP_NOT_DENY_ONLY, &facts.groups)?;
    let enabled: Vec<&str> = groups
        .iter()
        .filter(|group| group.attributes & SE_GROUP_INTEGRITY == 0)
        .filter(|group| group.attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0)
        .map(|group| group.sid.as_str())
        .collect();
    if !enabled.is_empty() {
        return Err(Refusal::new(
            reason::GROUP_NOT_DENY_ONLY,
            format!("groups not deny-only {enabled:?}"),
        ));
    }

    let privileges = *read(reason::PRIVILEGES_PRESENT, &facts.privileges)?;
    if privileges != 0 {
        return Err(Refusal::new(
            reason::PRIVILEGES_PRESENT,
            format!("{privileges} privileges"),
        ));
    }

    let integrity = read(reason::INTEGRITY_NOT_UNTRUSTED, &facts.integrity)?;
    if integrity != UNTRUSTED_INTEGRITY {
        return Err(Refusal::new(
            reason::INTEGRITY_NOT_UNTRUSTED,
            format!("integrity {integrity}, not {UNTRUSTED_INTEGRITY}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = "S-1-15-2-1-2-3-4-5-6-7";

    /// The primary measured in a fully confined worker after it lowered
    /// itself: deny-only groups (logon included) plus the integrity label.
    fn confined() -> PrimaryFacts {
        PrimaryFacts {
            lpac_claim: Ok(Claim::Unsigned(vec![1])),
            package: Ok(Some((PACKAGE.into(), true))),
            capabilities: Ok(vec![]),
            restricting_sids: Ok(vec![NULL_SID.into()]),
            groups: Ok(vec![
                Group {
                    sid: "S-1-1-0".into(),
                    attributes: SE_GROUP_USE_FOR_DENY_ONLY,
                },
                Group {
                    sid: "S-1-5-5-0-1".into(),
                    attributes: SE_GROUP_USE_FOR_DENY_ONLY | 0xc000_0000,
                },
                Group {
                    sid: UNTRUSTED_INTEGRITY.into(),
                    attributes: SE_GROUP_INTEGRITY | 0x40,
                },
            ]),
            privileges: Ok(0),
            integrity: Ok(UNTRUSTED_INTEGRITY.into()),
        }
    }

    fn reason_of(facts: &PrimaryFacts) -> &'static str {
        check(facts).expect_err("must be refused").reason
    }

    #[test]
    fn the_confined_primary_passes() {
        assert_eq!(check(&confined()), Ok(()));
        // No group count is pinned: a token with fewer groups passes too.
        let mut fewer = confined();
        fewer.groups = Ok(vec![]);
        assert_eq!(check(&fewer), Ok(()));
    }

    /// One way to break the confined primary, and the reason it must get.
    type Case = (&'static str, Box<dyn Fn(&mut PrimaryFacts)>);

    #[test]
    fn each_broken_property_is_refused_with_its_reason() {
        let cases: Vec<Case> = vec![
            (
                reason::NOT_LPAC,
                Box::new(|f| f.lpac_claim = Ok(Claim::Absent)),
            ),
            (
                reason::NOT_LPAC,
                Box::new(|f| f.lpac_claim = Ok(Claim::Unsigned(vec![0]))),
            ),
            (
                reason::NOT_LPAC,
                Box::new(|f| f.lpac_claim = Ok(Claim::Unsigned(vec![1, 1]))),
            ),
            (
                reason::NOT_LPAC,
                Box::new(|f| f.lpac_claim = Ok(Claim::OtherType(1))),
            ),
            (
                reason::NOT_LPAC,
                Box::new(|f| f.package = Ok(Some(("S-1-15-2-9".into(), false)))),
            ),
            (reason::NOT_LPAC, Box::new(|f| f.package = Ok(None))),
            (
                reason::CAPABILITIES_PRESENT,
                Box::new(|f| f.capabilities = Ok(vec!["S-1-15-3-1".into()])),
            ),
            (
                reason::RESTRICTING_SID_MISMATCH,
                Box::new(|f| f.restricting_sids = Ok(vec![])),
            ),
            (
                reason::RESTRICTING_SID_MISMATCH,
                Box::new(|f| f.restricting_sids = Ok(vec![NULL_SID.into(), "S-1-1-0".into()])),
            ),
            (
                reason::RESTRICTING_SID_MISMATCH,
                Box::new(|f| f.restricting_sids = Ok(vec!["S-1-1-0".into()])),
            ),
            (
                reason::GROUP_NOT_DENY_ONLY,
                Box::new(|f| {
                    f.groups.as_mut().unwrap().push(Group {
                        sid: "S-1-5-5-0-1".into(),
                        attributes: 0xc000_0007,
                    })
                }),
            ),
            (
                reason::PRIVILEGES_PRESENT,
                Box::new(|f| f.privileges = Ok(1)),
            ),
            (
                reason::INTEGRITY_NOT_UNTRUSTED,
                Box::new(|f| f.integrity = Ok("S-1-16-4096".into())),
            ),
        ];
        for (expected, breaks) in cases {
            let mut facts = confined();
            breaks(&mut facts);
            assert_eq!(reason_of(&facts), expected, "{facts:?}");
        }
    }

    #[test]
    fn an_unreadable_property_fails_the_check_that_needs_it() {
        fn error<T>() -> Result<T, String> {
            Err("Win32 5".to_owned())
        }
        let mut facts = confined();
        facts.lpac_claim = error();
        assert_eq!(reason_of(&facts), reason::NOT_LPAC);
        let mut facts = confined();
        facts.package = error();
        assert_eq!(reason_of(&facts), reason::NOT_LPAC);
        let mut facts = confined();
        facts.capabilities = error();
        assert_eq!(reason_of(&facts), reason::CAPABILITIES_PRESENT);
        let mut facts = confined();
        facts.restricting_sids = error();
        assert_eq!(reason_of(&facts), reason::RESTRICTING_SID_MISMATCH);
        let mut facts = confined();
        facts.groups = error();
        assert_eq!(reason_of(&facts), reason::GROUP_NOT_DENY_ONLY);
        let mut facts = confined();
        facts.privileges = error();
        assert_eq!(reason_of(&facts), reason::PRIVILEGES_PRESENT);
        let mut facts = confined();
        facts.integrity = error();
        assert_eq!(reason_of(&facts), reason::INTEGRITY_NOT_UNTRUSTED);
    }

    /// The checks run in the spec's order: with every property broken, the
    /// first one is reported, and fixing it reveals the next.
    #[test]
    fn the_first_failing_check_in_order_is_reported() {
        let mut facts = PrimaryFacts {
            lpac_claim: Ok(Claim::Absent),
            package: Ok(None),
            capabilities: Ok(vec!["S-1-15-3-1".into()]),
            restricting_sids: Ok(vec![]),
            groups: Ok(vec![Group {
                sid: "S-1-1-0".into(),
                attributes: 7,
            }]),
            privileges: Ok(3),
            integrity: Ok("S-1-16-4096".into()),
        };
        let fixed = confined();
        let order = [
            reason::NOT_LPAC,
            reason::CAPABILITIES_PRESENT,
            reason::RESTRICTING_SID_MISMATCH,
            reason::GROUP_NOT_DENY_ONLY,
            reason::PRIVILEGES_PRESENT,
            reason::INTEGRITY_NOT_UNTRUSTED,
        ];
        for expected in order {
            assert_eq!(reason_of(&facts), expected);
            match expected {
                reason::NOT_LPAC => {
                    facts.lpac_claim = fixed.lpac_claim.clone();
                    facts.package = fixed.package.clone();
                }
                reason::CAPABILITIES_PRESENT => facts.capabilities = fixed.capabilities.clone(),
                reason::RESTRICTING_SID_MISMATCH => {
                    facts.restricting_sids = fixed.restricting_sids.clone()
                }
                reason::GROUP_NOT_DENY_ONLY => facts.groups = fixed.groups.clone(),
                reason::PRIVILEGES_PRESENT => facts.privileges = fixed.privileges.clone(),
                _ => facts.integrity = fixed.integrity.clone(),
            }
        }
        assert_eq!(check(&facts), Ok(()));
    }
}
