//! The launch variants: the full confinement, and the test-only variants.

use super::error::Refusal;
use super::launch::MITIGATION_POLICY;

/// Which launch recipe to build.
///
/// A build without the `deviations` feature has only [`Deviation::Full`].
/// With the feature, every other variant is a test control:
///
/// - two weaker positive controls, which show that a denial seen under the
///   full confinement comes from the confinement and not from a missing
///   target or a broken probe;
/// - one variant per confinement check, each building the full recipe with
///   exactly one property broken, so that exactly that check must refuse it.
///   A check's refusal test and its mutation control share this one
///   definition of what the check is meant to catch.
///
/// For a worker check, the parent's own checks expect the broken property, so
/// the worker gets to start and must refuse by itself. For a parent check,
/// the parent still expects the full recipe and must refuse before resume.
#[cfg_attr(
    not(feature = "deviations"),
    doc = "```compile_fail\nuse basal_launch::Deviation;\nlet _ = Deviation::PlainWithoutHandleList;\n```"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Deviation {
    /// The full confinement. The only variant in production builds.
    #[default]
    Full,

    /// Positive control: a Less Privileged AppContainer with no capabilities,
    /// started with `CreateProcessW` and the parent's unrestricted token, no
    /// mitigations and no initial thread token.
    #[cfg(feature = "deviations")]
    LpacOnly,
    /// Positive control: no AppContainer at all.
    #[cfg(feature = "deviations")]
    Plain,
    /// Plain positive control with all inheritable parent handles passed through.
    #[cfg(feature = "deviations")]
    PlainWithoutHandleList,

    /// Worker check `thread-token-present`. The parent cannot leave a thread
    /// token behind in a correctly built worker, so it asks the worker to
    /// keep one (see [`Deviation::child_argument`]).
    #[cfg(feature = "deviations")]
    ThreadTokenPresent,
    /// Worker check `integrity-lower-failed`. The parent cannot make lowering
    /// fail in a correctly built worker, so it asks the worker to simulate the
    /// failure (see [`Deviation::child_argument`]).
    #[cfg(feature = "deviations")]
    IntegrityLowerFailed,
    /// Worker check `not-lpac`: the opt-out from the `ALL_APPLICATION_PACKAGES`
    /// group is left out, so objects that grant every AppContainer package
    /// grant this worker too, as for an ordinary AppContainer.
    #[cfg(feature = "deviations")]
    NotLpac,
    /// Worker check `capabilities-present`: one capability is granted.
    #[cfg(feature = "deviations")]
    CapabilitiesPresent,
    /// Worker check `restricting-sid-mismatch`: Everyone is added to the
    /// restricting SIDs, next to the NULL SID.
    #[cfg(feature = "deviations")]
    RestrictingSidMismatch,
    /// Worker check `group-not-deny-only`: the logon SID stays enabled.
    #[cfg(feature = "deviations")]
    GroupNotDenyOnly,
    /// Worker check `privileges-present`: the privileges that survive
    /// filtering are not removed.
    #[cfg(feature = "deviations")]
    PrivilegesPresent,
    /// Worker check `integrity-not-untrusted`. The worker lowers its own
    /// integrity, so the parent asks it to skip that step (see
    /// [`Deviation::child_argument`]).
    #[cfg(feature = "deviations")]
    IntegrityNotUntrusted,
    /// Worker check `mitigation-mismatch`: the dynamic-code prohibition is
    /// left out of the mitigation policy.
    #[cfg(feature = "deviations")]
    MitigationMismatch,
    /// Worker check `handle-not-allowed`: the explicit inherited-handle list
    /// is left out, so every inheritable handle of the parent is inherited.
    #[cfg(feature = "deviations")]
    HandleNotAllowed,

    /// Parent check `job-limits-mismatch`: the job is created with an
    /// active-process limit of 2.
    #[cfg(feature = "deviations")]
    JobLimitsMismatch,
    /// Parent check `not-in-owned-job`: the child is not created in the job.
    #[cfg(feature = "deviations")]
    NotInOwnedJob,
    /// Parent check `birth-token-mismatch`: the privileges that survive
    /// filtering are not removed, while the check still expects none.
    #[cfg(feature = "deviations")]
    BirthTokenMismatch,
    /// Parent check `initial-token-open`: the start-up thread token is never
    /// set on the suspended thread.
    #[cfg(feature = "deviations")]
    InitialTokenOpen,
}

/// The three basic shapes a launch can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// The full confinement, possibly with one property broken.
    Confined,
    /// The LPAC-only positive control.
    #[cfg(feature = "deviations")]
    LpacOnly,
    /// The plain positive control.
    #[cfg(feature = "deviations")]
    Plain,
}

impl Deviation {
    /// Every variant, for tests that cover them all.
    #[cfg(feature = "deviations")]
    pub const ALL: [Self; 18] = [
        Self::Full,
        Self::LpacOnly,
        Self::Plain,
        Self::PlainWithoutHandleList,
        Self::ThreadTokenPresent,
        Self::IntegrityLowerFailed,
        Self::NotLpac,
        Self::CapabilitiesPresent,
        Self::RestrictingSidMismatch,
        Self::GroupNotDenyOnly,
        Self::PrivilegesPresent,
        Self::IntegrityNotUntrusted,
        Self::MitigationMismatch,
        Self::HandleNotAllowed,
        Self::JobLimitsMismatch,
        Self::NotInOwnedJob,
        Self::BirthTokenMismatch,
        Self::InitialTokenOpen,
    ];

    /// The variant's stable name. For a check's variant it is the check's
    /// reason token.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            #[cfg(feature = "deviations")]
            Self::LpacOnly => "lpac-only",
            #[cfg(feature = "deviations")]
            Self::Plain => "plain",
            #[cfg(feature = "deviations")]
            Self::PlainWithoutHandleList => "plain-without-handle-list",
            #[cfg(feature = "deviations")]
            Self::ThreadTokenPresent => "thread-token-present",
            #[cfg(feature = "deviations")]
            Self::IntegrityLowerFailed => "integrity-lower-failed",
            #[cfg(feature = "deviations")]
            Self::NotLpac => "not-lpac",
            #[cfg(feature = "deviations")]
            Self::CapabilitiesPresent => "capabilities-present",
            #[cfg(feature = "deviations")]
            Self::RestrictingSidMismatch => "restricting-sid-mismatch",
            #[cfg(feature = "deviations")]
            Self::GroupNotDenyOnly => "group-not-deny-only",
            #[cfg(feature = "deviations")]
            Self::PrivilegesPresent => "privileges-present",
            #[cfg(feature = "deviations")]
            Self::IntegrityNotUntrusted => "integrity-not-untrusted",
            #[cfg(feature = "deviations")]
            Self::MitigationMismatch => "mitigation-mismatch",
            #[cfg(feature = "deviations")]
            Self::HandleNotAllowed => "handle-not-allowed",
            #[cfg(feature = "deviations")]
            Self::JobLimitsMismatch => "job-limits-mismatch",
            #[cfg(feature = "deviations")]
            Self::NotInOwnedJob => "not-in-owned-job",
            #[cfg(feature = "deviations")]
            Self::BirthTokenMismatch => "birth-token-mismatch",
            #[cfg(feature = "deviations")]
            Self::InitialTokenOpen => "initial-token-open",
        }
    }

    /// The reason the worker must exit with, for a worker check's variant.
    pub const fn worker_reason(self) -> Option<&'static str> {
        #[cfg(feature = "deviations")]
        if matches!(
            self,
            Self::ThreadTokenPresent
                | Self::IntegrityLowerFailed
                | Self::NotLpac
                | Self::CapabilitiesPresent
                | Self::RestrictingSidMismatch
                | Self::GroupNotDenyOnly
                | Self::PrivilegesPresent
                | Self::IntegrityNotUntrusted
                | Self::MitigationMismatch
                | Self::HandleNotAllowed
        ) {
            return Some(self.name());
        }
        None
    }

    /// The refusal the parent must return, for a parent check's variant.
    pub const fn parent_refusal(self) -> Option<Refusal> {
        #[cfg(feature = "deviations")]
        match self {
            Self::JobLimitsMismatch => return Some(Refusal::JobLimitsMismatch),
            Self::NotInOwnedJob => return Some(Refusal::NotInOwnedJob),
            Self::BirthTokenMismatch => return Some(Refusal::BirthTokenMismatch),
            Self::InitialTokenOpen => return Some(Refusal::InitialTokenOpen),
            _ => {}
        }
        None
    }

    /// The argument added to the child's command line for a worker check the
    /// parent cannot set up itself. Only a worker built with its own test
    /// support acts on it.
    pub fn child_argument(self) -> Option<String> {
        #[cfg(feature = "deviations")]
        if matches!(
            self,
            Self::ThreadTokenPresent | Self::IntegrityLowerFailed | Self::IntegrityNotUntrusted
        ) {
            return Some(format!("--confinement-deviation={}", self.name()));
        }
        None
    }

    pub(crate) const fn shape(self) -> Shape {
        #[cfg(feature = "deviations")]
        match self {
            Self::LpacOnly => return Shape::LpacOnly,
            Self::Plain | Self::PlainWithoutHandleList => return Shape::Plain,
            _ => {}
        }
        Shape::Confined
    }

    /// Whether the `ALL_APPLICATION_PACKAGES` opt-out, which makes the
    /// AppContainer a Less Privileged one, is applied.
    pub(crate) const fn less_privileged(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::NotLpac) {
            return false;
        }
        true
    }

    /// Whether one capability is granted to the AppContainer. Production
    /// builds grant none, so they do not have this question at all.
    #[cfg(feature = "deviations")]
    pub(crate) const fn grants_capability(self) -> bool {
        matches!(self, Self::CapabilitiesPresent)
    }

    /// Whether the logon SID is left enabled in the primary token.
    pub(crate) const fn keeps_logon_group(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::GroupNotDenyOnly) {
            return true;
        }
        false
    }

    /// Whether Everyone joins the NULL SID among the restricting SIDs.
    pub(crate) const fn widens_restricting_sids(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::RestrictingSidMismatch) {
            return true;
        }
        false
    }

    /// Whether the privileges that survive filtering stay in the primary.
    pub(crate) const fn keeps_privileges(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::PrivilegesPresent | Self::BirthTokenMismatch) {
            return true;
        }
        false
    }

    /// Whether the parent's birth check expects privileges in the primary.
    /// It differs from `keeps_privileges` only for the variant that tests the
    /// birth check itself.
    pub(crate) const fn expects_privileges(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::PrivilegesPresent) {
            return true;
        }
        false
    }

    /// The mitigation policy passed at creation.
    pub(crate) const fn mitigation_policy(self) -> u64 {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::MitigationMismatch) {
            return MITIGATION_POLICY & !super::launch::MITIGATION_PROHIBIT_DYNAMIC_CODE;
        }
        MITIGATION_POLICY
    }

    /// Whether the explicit inherited-handle list is passed.
    pub(crate) const fn restricts_inherited_handles(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::HandleNotAllowed | Self::PlainWithoutHandleList) {
            return false;
        }
        true
    }

    /// The active-process limit the job is created with.
    pub(crate) const fn job_active_process_limit(self) -> u32 {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::JobLimitsMismatch) {
            return 2;
        }
        1
    }

    /// Whether the child is created inside the owned job.
    pub(crate) const fn joins_job(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::NotInOwnedJob) {
            return false;
        }
        true
    }

    /// Whether the start-up thread token is set on the suspended thread.
    pub(crate) const fn sets_initial_token(self) -> bool {
        #[cfg(feature = "deviations")]
        if matches!(self, Self::InitialTokenOpen) {
            return false;
        }
        true
    }
}

impl std::fmt::Display for Deviation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_is_the_default_and_names_no_check() {
        assert_eq!(Deviation::default(), Deviation::Full);
        assert_eq!(Deviation::Full.name(), "full");
        assert_eq!(Deviation::Full.worker_reason(), None);
        assert_eq!(Deviation::Full.parent_refusal(), None);
        assert_eq!(Deviation::Full.child_argument(), None);
        assert_eq!(Deviation::Full.shape(), Shape::Confined);
        assert_eq!(Deviation::Full.mitigation_policy(), MITIGATION_POLICY);
        assert!(Deviation::Full.less_privileged());
        #[cfg(feature = "deviations")]
        assert!(!Deviation::Full.grants_capability());
        assert!(!Deviation::Full.keeps_logon_group());
        assert!(!Deviation::Full.widens_restricting_sids());
        assert!(!Deviation::Full.keeps_privileges());
        assert!(!Deviation::Full.expects_privileges());
        assert!(Deviation::Full.restricts_inherited_handles());
        assert_eq!(Deviation::Full.job_active_process_limit(), 1);
        assert!(Deviation::Full.joins_job());
        assert!(Deviation::Full.sets_initial_token());
    }

    /// Without the `deviations` feature the launcher can build only the full
    /// recipe. The match has no wildcard arm, so this test stops compiling
    /// if any other variant exists in such a build.
    #[cfg(not(feature = "deviations"))]
    #[test]
    fn a_build_without_the_feature_has_only_the_full_recipe() {
        match Deviation::default() {
            Deviation::Full => {}
        }
    }

    /// Every check's variant names exactly one check, and changes exactly
    /// one property of the full recipe (or, for the three worker checks the
    /// parent cannot set up, only adds the request to the worker).
    #[cfg(feature = "deviations")]
    #[test]
    fn every_check_variant_breaks_exactly_one_property() {
        let full = knobs(Deviation::Full);
        for deviation in Deviation::ALL {
            let checks = usize::from(deviation.worker_reason().is_some())
                + usize::from(deviation.parent_refusal().is_some());
            let control = matches!(
                deviation,
                Deviation::Full
                    | Deviation::LpacOnly
                    | Deviation::Plain
                    | Deviation::PlainWithoutHandleList
            );
            assert_eq!(checks, usize::from(!control), "{deviation}");
            if let Some(reason) = deviation.worker_reason() {
                assert_eq!(reason, deviation.name());
            }
            if let Some(refusal) = deviation.parent_refusal() {
                assert_eq!(refusal.reason(), deviation.name());
            }
            if control {
                continue;
            }
            let changed = knobs(deviation)
                .iter()
                .zip(full.iter())
                .filter(|(a, b)| a != b)
                .count();
            let expected = match deviation {
                // `BirthTokenMismatch` builds the same token as
                // `PrivilegesPresent` but leaves the parent expecting no
                // privileges, so only `keeps_privileges` differs from the full
                // recipe. `PrivilegesPresent` changes both `keeps_privileges`
                // and `expects_privileges`.
                Deviation::PrivilegesPresent => 2,
                _ => 1,
            };
            assert_eq!(changed, expected, "{deviation}");
        }
    }

    #[cfg(feature = "deviations")]
    #[test]
    fn dropped_handle_list_controls_change_only_the_handle_list() {
        for (normal, dropped) in [
            (Deviation::Full, Deviation::HandleNotAllowed),
            (Deviation::Plain, Deviation::PlainWithoutHandleList),
        ] {
            assert_eq!(normal.shape(), dropped.shape());
            let mut expected = knobs(normal);
            assert_eq!(expected[7], 1);
            expected[7] = 0;
            assert_eq!(knobs(dropped), expected);
        }
    }

    #[cfg(feature = "deviations")]
    fn knobs(deviation: Deviation) -> Vec<u64> {
        vec![
            u64::from(deviation.less_privileged()),
            u64::from(deviation.grants_capability()),
            u64::from(deviation.keeps_logon_group()),
            u64::from(deviation.widens_restricting_sids()),
            u64::from(deviation.keeps_privileges()),
            u64::from(deviation.expects_privileges()),
            deviation.mitigation_policy(),
            u64::from(deviation.restricts_inherited_handles()),
            u64::from(deviation.job_active_process_limit()),
            u64::from(deviation.joins_job()),
            u64::from(deviation.sets_initial_token()),
            u64::from(deviation.child_argument().is_some()),
        ]
    }
}
