use super::*;
use basal_proto::{LandlockReport, PROTOCOL_VERSION, PreludeHash};

fn welcome(confinement: Confinement) -> Welcome {
    Welcome {
        protocol_version: PROTOCOL_VERSION,
        engine: "fixture".into(),
        prelude_hash: PreludeHash([0; 32]),
        codemode_prelude_hash: PreludeHash([0; 32]),
        confinement,
    }
}

#[test]
fn landlock_is_required_by_default() {
    assert_eq!(
        PoolConfig::new("worker", WorkerLaunch::Plain).landlock,
        LandlockPolicy::Required
    );
}

#[test]
fn health_reports_the_explicit_policy_only_on_linux() {
    for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
        let mut config = PoolConfig::new("unused-worker", WorkerLaunch::Plain);
        config.landlock = mode;
        config.warm_spares = 0;
        let spawner = Arc::new(ProcessSpawner::new(&config));
        let pool = Pool::new(
            config,
            spawner,
            Clock::manual(0),
            Arc::new(Metrics::default()),
        );
        assert_eq!(
            pool.worker_confinement(),
            serde_json::json!({
                "os": std::env::consts::OS,
                "landlock": if cfg!(target_os = "linux") { serde_json::json!(mode.as_str()) } else { serde_json::Value::Null },
                CONFINEMENT_FAULT_DEATHS: 0,
            })
        );
        pool.stop();
    }
}

#[test]
fn an_unconfined_worker_is_refused_in_both_modes() {
    for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
        assert!(!accepts_welcome(&welcome(Confinement::None), mode));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_accepts_seatbelt_but_refuses_linux() {
    for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
        assert!(accepts_welcome(&welcome(Confinement::Seatbelt), mode));
        assert!(!accepts_welcome(
            &welcome(Confinement::Linux {
                seccomp: true,
                landlock: Some(LandlockReport {
                    runtime_abi: LANDLOCK_ABI,
                    applied_abi: LANDLOCK_ABI
                }),
            }),
            mode
        ));
        assert!(!accepts_welcome(
            &welcome(Confinement::Windows {
                lpac: true,
                untrusted: true,
                no_thread_token: true,
                mitigations: true,
                handle_table: true,
            }),
            mode
        ));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    fn linux(seccomp: bool, report: Option<(u32, u32)>) -> Welcome {
        welcome(Confinement::Linux {
            seccomp,
            landlock: report.map(|(runtime_abi, applied_abi)| LandlockReport {
                runtime_abi,
                applied_abi,
            }),
        })
    }

    fn refused_in_both_modes(report: Option<(u32, u32)>) {
        for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
            assert!(
                !accepts_welcome(&linux(true, report), mode),
                "{report:?} {mode:?}"
            );
        }
    }

    #[test]
    fn linux_refuses_the_wrong_os_variant() {
        for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
            assert!(!accepts_welcome(&welcome(Confinement::Seatbelt), mode));
            assert!(!accepts_welcome(
                &welcome(Confinement::Windows {
                    lpac: true,
                    untrusted: true,
                    no_thread_token: true,
                    mitigations: true,
                    handle_table: true,
                }),
                mode
            ));
        }
    }

    #[test]
    fn seccomp_is_mandatory_even_with_valid_landlock() {
        for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
            for report in [None, Some((LANDLOCK_ABI, LANDLOCK_ABI))] {
                assert!(!accepts_welcome(&linux(false, report), mode));
            }
        }
    }

    #[test]
    fn absent_landlock_requires_an_explicit_optional_policy() {
        assert!(!accepts_welcome(
            &linux(true, None),
            LandlockPolicy::Required
        ));
        assert!(accepts_welcome(
            &linux(true, None),
            LandlockPolicy::Optional
        ));
    }

    #[test]
    fn runtime_abi_zero_is_refused() {
        refused_in_both_modes(Some((0, 0)));
    }

    #[test]
    fn applied_abi_zero_is_refused() {
        refused_in_both_modes(Some((LANDLOCK_ABI, 0)));
    }

    #[test]
    fn applied_abi_above_runtime_is_refused() {
        refused_in_both_modes(Some((1, 2)));
    }

    #[test]
    fn applied_abi_below_the_supported_minimum_is_refused() {
        refused_in_both_modes(Some((LANDLOCK_ABI, LANDLOCK_ABI - 1)));
        refused_in_both_modes(Some((LANDLOCK_ABI + 1, LANDLOCK_ABI - 1)));
    }

    #[test]
    fn applied_abi_above_the_pinned_abi_is_refused() {
        refused_in_both_modes(Some((LANDLOCK_ABI + 1, LANDLOCK_ABI + 1)));
    }

    #[test]
    fn all_supported_abis_and_newer_kernels_are_accepted() {
        for runtime_abi in 1..=LANDLOCK_ABI + 2 {
            for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
                assert!(accepts_welcome(
                    &linux(true, Some((runtime_abi, runtime_abi.min(LANDLOCK_ABI)))),
                    mode
                ));
            }
        }
    }
}

#[test]
fn windows_confinement_acceptance() {
    let all_true = welcome(Confinement::Windows {
        lpac: true,
        untrusted: true,
        no_thread_token: true,
        mitigations: true,
        handle_table: true,
    });
    for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
        assert_eq!(accepts_welcome(&all_true, mode), cfg!(windows));
    }
    for i in 0..5 {
        let mut fields = [true; 5];
        fields[i] = false;
        let w = welcome(Confinement::Windows {
            lpac: fields[0],
            untrusted: fields[1],
            no_thread_token: fields[2],
            mitigations: fields[3],
            handle_table: fields[4],
        });
        for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
            assert!(!accepts_welcome(&w, mode));
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    #[test]
    fn windows_refuses_other_os_variants() {
        for mode in [LandlockPolicy::Required, LandlockPolicy::Optional] {
            assert!(!accepts_welcome(&welcome(Confinement::Seatbelt), mode));
            assert!(!accepts_welcome(
                &welcome(Confinement::Linux {
                    seccomp: true,
                    landlock: None,
                }),
                mode
            ));
            assert!(!accepts_welcome(&welcome(Confinement::None), mode));
        }
    }
}
