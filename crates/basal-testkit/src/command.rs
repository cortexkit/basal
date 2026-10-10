//! Commands used by test parents, with an explicit environment on Windows.
//!
//! Keeping the environment here avoids trying to recover `env_clear` from a
//! standard Command, which does not expose whether inheritance was disabled.

#[cfg(unix)]
pub use std::process::Command;

#[cfg(windows)]
mod windows {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Output};
    use std::time::Duration;

    pub struct Command {
        program: OsString,
        args: Vec<OsString>,
        env: BTreeMap<OsString, OsString>,
        cwd: Option<PathBuf>,
    }

    impl Command {
        pub fn new(program: impl AsRef<OsStr>) -> Self {
            Self {
                program: program.as_ref().into(),
                args: vec![],
                env: std::env::vars_os().collect(),
                cwd: None,
            }
        }

        pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
            self.args.push(arg.as_ref().into());
            self
        }

        pub fn args<I, S>(&mut self, args: I) -> &mut Self
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            for arg in args {
                self.arg(arg);
            }
            self
        }

        pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
            self.env_remove(key.as_ref());
            self.env.insert(key.as_ref().into(), value.as_ref().into());
            self
        }

        pub fn envs<I, K, V>(&mut self, env: I) -> &mut Self
        where
            I: IntoIterator<Item = (K, V)>,
            K: AsRef<OsStr>,
            V: AsRef<OsStr>,
        {
            for (key, value) in env {
                self.env(key, value);
            }
            self
        }

        pub fn env_clear(&mut self) -> &mut Self {
            self.env.clear();
            self
        }

        pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
            // Windows environment keys are case-insensitive, unlike OsString's ordering.
            let key = key.as_ref().to_string_lossy();
            self.env
                .retain(|name, _| !name.to_string_lossy().eq_ignore_ascii_case(&key));
            self
        }

        pub fn current_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
            self.cwd = Some(path.as_ref().into());
            self
        }

        pub fn output(&mut self) -> io::Result<Output> {
            self.output_until(Duration::from_secs(950))
        }
        pub fn status(&mut self) -> io::Result<ExitStatus> {
            self.output().map(|output| output.status)
        }

        pub fn output_until(&self, timeout: Duration) -> io::Result<Output> {
            use basal_launch::{PlainCommand, PlainStdio};
            let program = resolve_program(&self.program, &self.env, self.cwd.as_deref())?;
            let command = PlainCommand {
                program,
                args: self.args.clone(),
                env: self
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                cwd: self.cwd.clone(),
                stdin: PlainStdio::Null,
                stdout: PlainStdio::Piped,
                stderr: PlainStdio::Piped,
            };
            super::super::process::windows::output_until(&command, timeout)
        }
    }

    fn resolve_program(
        program: &OsStr,
        env: &BTreeMap<OsString, OsString>,
        cwd: Option<&Path>,
    ) -> io::Result<PathBuf> {
        let path = Path::new(program);
        if path.is_absolute() {
            return Ok(path.into());
        }
        if path.components().count() > 1 {
            return Ok(cwd
                .map(PathBuf::from)
                .unwrap_or(std::env::current_dir()?)
                .join(path));
        }
        let search = env
            .iter()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "command PATH is absent"))?;
        for directory in std::env::split_paths(search) {
            let candidate = directory.join(path);
            let candidate = if candidate.extension().is_none() {
                candidate.with_extension("exe")
            } else {
                candidate
            };
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("executable {} not found", path.display()),
        ))
    }
}

#[cfg(windows)]
pub use windows::Command;
