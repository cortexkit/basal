use crate::command::Command;
use std::ffi::{OsStr, OsString};

/// Starts git without inheriting caller configuration or environment.
///
/// Fixture repositories need stable identities and must not run hooks or
/// consult machine-wide git settings supplied by the test runner.
pub fn git_command() -> Command {
    git_command_with_env(std::env::vars_os())
}

fn git_command_with_env(base_env: impl IntoIterator<Item = (OsString, OsString)>) -> Command {
    let base_env: Vec<_> = base_env.into_iter().collect();
    let mut command = Command::new("git");
    // Set the base environment on the command, then clear it. In production
    // the base is this process's own environment, so the clear only drops
    // what it would have inherited. A test can pass a base carrying injected
    // git settings instead: if the clear ever went missing, those settings
    // would reach git, and the isolation test would see the difference.
    command.envs(base_env.iter().cloned());
    command.env_clear();
    for key in [
        OsStr::new("PATH"),
        OsStr::new("HOME"),
        OsStr::new("SystemRoot"),
        OsStr::new("USERPROFILE"),
    ] {
        if let Some((_, value)) = base_env.iter().rev().find(|(name, _)| name == key) {
            command.env(key, value);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Basal Test")
        .env("GIT_AUTHOR_EMAIL", "basal-test@example.com")
        .env("GIT_COMMITTER_NAME", "Basal Test")
        .env("GIT_COMMITTER_EMAIL", "basal-test@example.com");
    command
}

fn empty_global_config() -> &'static std::path::Path {
    static CONFIG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    CONFIG.get_or_init(|| {
        let dir = crate::harness::scratch("empty-git-config");
        let path = dir.join("global.gitconfig");
        std::fs::write(&path, []).expect("empty git global config");
        path
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::git_command_with_env;

    #[test]
    fn fixture_git_commands_ignore_injected_global_hooks() {
        let dir = crate::harness::scratch("git-env-isolation");
        let hooks = dir.join("hooks");
        std::fs::create_dir_all(&hooks).expect("hooks dir");
        let marker = dir.join("hook-ran");
        let hook = hooks.join("post-commit");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nprintf 'ran\\n' > '{}'\n", marker.display()),
        )
        .expect("hook");
        let mut permissions = std::fs::metadata(&hook)
            .expect("hook metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).expect("make hook executable");

        let mut base_env: Vec<_> = std::env::vars_os().collect();
        base_env.extend([
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
            (
                "GIT_CONFIG_VALUE_0".into(),
                hooks.as_os_str().to_os_string(),
            ),
        ]);

        let status = git_command_with_env(base_env.clone())
            .current_dir(&dir)
            .args(["init", "-q", "-b", "main"])
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed: {status}");
        std::fs::write(dir.join("file"), "fixture\n").expect("fixture file");
        for args in [&["add", "file"][..], &["commit", "-q", "-m", "fixture"]] {
            let status = git_command_with_env(base_env.clone())
                .current_dir(&dir)
                .args(args)
                .status()
                .expect("git command");
            assert!(status.success(), "git {args:?} failed: {status}");
        }

        assert!(!marker.exists(), "injected post-commit hook ran");
        std::fs::remove_dir_all(dir).expect("remove scratch repository");
    }
}
