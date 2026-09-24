//! Running OS commands (`security`, `networksetup`), with optional `sudo`.
//!
//! Everything that changes system settings goes through [`Runner`], so tests
//! can substitute a fake that records commands instead of executing them.

use std::io;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner {
    /// Run `program args...`, prefixed with `sudo` when `sudo` is true.
    fn run(&self, program: &str, args: &[&str], sudo: bool) -> io::Result<Output>;
}

/// Executes commands for real. `sudo` reads the password from the terminal.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str], sudo: bool) -> io::Result<Output> {
        let mut cmd = if sudo {
            let mut c = Command::new("sudo");
            c.arg(program);
            c
        } else {
            Command::new(program)
        };
        let out = cmd.args(args).stdin(Stdio::inherit()).stderr(Stdio::piped()).stdout(Stdio::piped()).output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// A shell-style rendering of a command, for messages and dry runs.
pub fn display(program: &str, args: &[&str], sudo: bool) -> String {
    let quote = |a: &str| {
        if a.is_empty() || a.contains([' ', '"', '\'', '(', ')', '*']) {
            format!("\"{}\"", a.replace('"', "\\\""))
        } else {
            a.to_string()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if sudo {
        parts.push("sudo".into());
    }
    parts.push(program.into());
    parts.extend(args.iter().map(|a| quote(a)));
    parts.join(" ")
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;

    /// Records every call; answers from a list of (prefix, output) rules.
    #[derive(Default)]
    pub struct FakeRunner {
        pub calls: RefCell<Vec<String>>,
        pub rules: Vec<(String, Output)>,
    }

    impl FakeRunner {
        pub fn with(rules: &[(&str, &str, bool)]) -> Self {
            FakeRunner {
                calls: RefCell::default(),
                rules: rules
                    .iter()
                    .map(|(p, out, ok)| (p.to_string(), Output { success: *ok, stdout: out.to_string(), stderr: String::new() }))
                    .collect(),
            }
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, program: &str, args: &[&str], sudo: bool) -> io::Result<Output> {
            let line = display(program, args, sudo);
            self.calls.borrow_mut().push(line.clone());
            let out = self
                .rules
                .iter()
                .find(|(prefix, _)| line.starts_with(prefix.as_str()))
                .map(|(_, o)| o.clone())
                .unwrap_or(Output { success: true, ..Default::default() });
            Ok(out)
        }
    }
}
