//! The mitigation policies the worker requires of its own process.
//!
//! The parent sets them as a creation attribute; the worker reads each policy
//! word back with `GetProcessMitigationPolicy` and checks single bits. Only
//! the listed bits are judged. Whole words are never compared: they differ
//! between Windows releases (Windows 11 reports an extra Win32k bit that
//! Server 2022 does not), and a bit added by a later release must not make a
//! correctly confined worker refuse to start.

/// One policy word per mitigation policy the worker reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MitigationWords {
    /// `PROCESS_MITIGATION_DYNAMIC_CODE_POLICY`.
    pub dynamic_code: u32,
    /// `PROCESS_MITIGATION_BINARY_SIGNATURE_POLICY`.
    pub signature: u32,
    /// `PROCESS_MITIGATION_IMAGE_LOAD_POLICY`.
    pub image_load: u32,
    /// `PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY`.
    pub system_call: u32,
    /// `PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY`.
    pub strict_handle: u32,
    /// `PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY`.
    pub extension_point: u32,
    /// `PROCESS_MITIGATION_CHILD_PROCESS_POLICY`.
    pub child_process: u32,
}

/// Which policy word a bit lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    DynamicCode,
    Signature,
    ImageLoad,
    SystemCall,
    StrictHandle,
    ExtensionPoint,
    ChildProcess,
}

impl Policy {
    fn word(self, words: &MitigationWords) -> u32 {
        match self {
            Self::DynamicCode => words.dynamic_code,
            Self::Signature => words.signature,
            Self::ImageLoad => words.image_load,
            Self::SystemCall => words.system_call,
            Self::StrictHandle => words.strict_handle,
            Self::ExtensionPoint => words.extension_point,
            Self::ChildProcess => words.child_process,
        }
    }
}

/// One named bit of a policy word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bit {
    pub policy: Policy,
    pub mask: u32,
    pub name: &'static str,
}

const fn bit(policy: Policy, index: u32, name: &'static str) -> Bit {
    Bit {
        policy,
        mask: 1 << index,
        name,
    }
}

/// The bits that must be set. The positions are those of the bit fields in
/// the `PROCESS_MITIGATION_*_POLICY` structures.
pub const REQUIRED: [Bit; 10] = [
    // No executable memory can be created, or made executable, after load.
    bit(Policy::DynamicCode, 0, "ProhibitDynamicCode"),
    // Only Microsoft-signed DLLs load.
    bit(Policy::Signature, 0, "MicrosoftSignedOnly"),
    bit(Policy::ImageLoad, 0, "NoRemoteImages"),
    bit(Policy::ImageLoad, 1, "NoLowMandatoryLabelImages"),
    bit(Policy::ImageLoad, 2, "PreferSystem32Images"),
    // No system calls into the GUI kernel component.
    bit(Policy::SystemCall, 0, "DisallowWin32kSystemCalls"),
    // Using an invalid handle raises an exception, and that cannot be undone.
    bit(
        Policy::StrictHandle,
        0,
        "RaiseExceptionOnInvalidHandleReference",
    ),
    bit(
        Policy::StrictHandle,
        1,
        "HandleExceptionsPermanentlyEnabled",
    ),
    // No legacy extension points (AppInit DLLs, hooks and the like).
    bit(Policy::ExtensionPoint, 0, "DisableExtensionPoints"),
    bit(Policy::ChildProcess, 0, "NoChildProcessCreation"),
];

/// The bits that must be clear: each lets code in the process turn the
/// dynamic-code prohibition off again, for one thread or from outside.
pub const FORBIDDEN: [Bit; 2] = [
    bit(Policy::DynamicCode, 1, "AllowThreadOptOut"),
    bit(Policy::DynamicCode, 2, "AllowRemoteDowngrade"),
];

/// Checks the words read back: every required bit set and every forbidden
/// bit clear. Any other bit is ignored. The error names every bit that is
/// wrong.
pub fn validate(words: &MitigationWords) -> Result<(), String> {
    let mut wrong: Vec<String> = Vec::new();
    for required in REQUIRED {
        if required.policy.word(words) & required.mask == 0 {
            wrong.push(format!("{} is off", required.name));
        }
    }
    for forbidden in FORBIDDEN {
        if forbidden.policy.word(words) & forbidden.mask != 0 {
            wrong.push(format!("{} is on", forbidden.name));
        }
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!("{} (read back {words:?})", wrong.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words measured in a fully confined child. Windows 11 reports
    /// Win32k word 5 and Server 2022 word 1; both must pass.
    fn measured(system_call: u32) -> MitigationWords {
        MitigationWords {
            dynamic_code: 1,
            signature: 5,
            image_load: 7,
            system_call,
            strict_handle: 3,
            extension_point: 1,
            child_process: 1,
        }
    }

    fn set(words: &mut MitigationWords, bit: Bit, on: bool) {
        let word = match bit.policy {
            Policy::DynamicCode => &mut words.dynamic_code,
            Policy::Signature => &mut words.signature,
            Policy::ImageLoad => &mut words.image_load,
            Policy::SystemCall => &mut words.system_call,
            Policy::StrictHandle => &mut words.strict_handle,
            Policy::ExtensionPoint => &mut words.extension_point,
            Policy::ChildProcess => &mut words.child_process,
        };
        if on {
            *word |= bit.mask;
        } else {
            *word &= !bit.mask;
        }
    }

    #[test]
    fn the_measured_words_of_both_images_pass() {
        assert_eq!(validate(&measured(5)), Ok(()));
        assert_eq!(validate(&measured(1)), Ok(()));
    }

    #[test]
    fn exactly_the_required_bits_pass_and_other_bits_are_ignored() {
        let mut words = MitigationWords::default();
        for required in REQUIRED {
            set(&mut words, required, true);
        }
        assert_eq!(validate(&words), Ok(()));
        // Bits nobody listed (audit modes, store signing, later additions)
        // change nothing, in any word but the dynamic-code one, whose other
        // bits include the forbidden opt-outs.
        let mut noisy = words;
        noisy.signature |= !1;
        noisy.image_load |= !7;
        noisy.system_call |= !1;
        noisy.strict_handle |= !3;
        noisy.extension_point |= !1;
        noisy.child_process |= !1;
        noisy.dynamic_code |= 1 << 3;
        assert_eq!(validate(&noisy), Ok(()));
    }

    #[test]
    fn each_missing_required_bit_is_refused_by_name() {
        for required in REQUIRED {
            let mut words = measured(5);
            set(&mut words, required, false);
            let error = validate(&words).expect_err(required.name);
            assert!(
                error.starts_with(&format!("{} is off", required.name)),
                "{error}"
            );
        }
        assert!(validate(&MitigationWords::default()).is_err());
    }

    /// Every required bit set does not excuse an opt-out: each forbidden bit
    /// alone, together with all the required ones, is refused.
    #[test]
    fn every_required_bit_with_a_forbidden_opt_out_is_refused() {
        let mut all_required = MitigationWords::default();
        for required in REQUIRED {
            set(&mut all_required, required, true);
        }
        for forbidden in FORBIDDEN {
            let mut words = all_required;
            set(&mut words, forbidden, true);
            let error = validate(&words).expect_err(forbidden.name);
            assert!(
                error.starts_with(&format!("{} is on", forbidden.name)),
                "{error}"
            );
        }
        assert_eq!(FORBIDDEN[0].name, "AllowThreadOptOut");
        assert_eq!(FORBIDDEN[0].mask, 0x2);
        assert_eq!(FORBIDDEN[1].name, "AllowRemoteDowngrade");
        assert_eq!(FORBIDDEN[1].mask, 0x4);
    }
}
