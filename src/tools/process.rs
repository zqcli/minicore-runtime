use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use thiserror::Error;

pub const MAX_PROGRAM_BYTES: usize = 256;
pub const MAX_ARGUMENTS: usize = 128;
pub const MAX_ARGUMENT_BYTES: usize = 8_192;
pub const MAX_TOTAL_ARGUMENT_BYTES: usize = 64 * 1024;
pub const MAX_CWD_BYTES: usize = 4_096;
pub const MAX_ENV_VARS: usize = 32;
pub const MAX_ENV_KEY_BYTES: usize = 128;
pub const MAX_ENV_VALUE_BYTES: usize = 8_192;
pub const MIN_TIMEOUT: Duration = Duration::from_millis(100);
pub const MAX_TIMEOUT: Duration = Duration::from_millis(600_000);
pub const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
const OUTPUT_ESCAPE_MULTIPLIER: usize = 6;
const OUTPUT_FIXED_ENVELOPE_BYTES: usize = 256;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProcessPolicyError {
    #[error("process policy bounds are invalid")]
    InvalidBounds,
    #[error("process program is invalid")]
    InvalidProgram,
    #[error("process environment key is invalid")]
    InvalidEnvironmentKey,
    #[error("process output bounds exceed the tool output limit")]
    OutputTooLarge,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ProgramPolicy {
    Any,
    AllowList(BTreeSet<String>),
}

impl ProgramPolicy {
    pub const fn any() -> Self {
        Self::Any
    }

    pub fn allow_list<I, S>(programs: I) -> Result<Self, ProcessPolicyError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allowed = BTreeSet::new();
        for program in programs {
            let program = program.as_ref();
            if !valid_program(program) {
                return Err(ProcessPolicyError::InvalidProgram);
            }
            allowed.insert(program.to_owned());
        }
        if allowed.is_empty() {
            return Err(ProcessPolicyError::InvalidProgram);
        }
        Ok(Self::AllowList(allowed))
    }

    pub fn allows(&self, program: &str) -> bool {
        match self {
            Self::Any => valid_program(program),
            Self::AllowList(programs) => programs.contains(program),
        }
    }
}

impl fmt::Debug for ProgramPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProgramPolicy")
            .field(
                "kind",
                &match self {
                    Self::Any => "any",
                    Self::AllowList(_) => "allow_list",
                },
            )
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProcessPolicy {
    enabled: bool,
    allowed_programs: ProgramPolicy,
    inherit_env: bool,
    allowed_env: BTreeSet<String>,
    default_timeout: Duration,
    max_timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
}

impl Default for ProcessPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ProcessPolicy {
    pub fn new(
        enabled: bool,
        allowed_programs: ProgramPolicy,
        inherit_env: bool,
        allowed_env: BTreeSet<String>,
    ) -> Result<Self, ProcessPolicyError> {
        Self {
            enabled,
            allowed_programs,
            inherit_env,
            allowed_env,
            default_timeout: DEFAULT_TIMEOUT,
            max_timeout: MAX_TIMEOUT,
            max_stdout_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_stderr_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
        .checked()
    }

    pub fn with_limits(
        mut self,
        default_timeout: Duration,
        max_timeout: Duration,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Result<Self, ProcessPolicyError> {
        self.default_timeout = default_timeout;
        self.max_timeout = max_timeout;
        self.max_stdout_bytes = max_stdout_bytes;
        self.max_stderr_bytes = max_stderr_bytes;
        self.checked()
    }

    fn checked(self) -> Result<Self, ProcessPolicyError> {
        let Self {
            enabled,
            allowed_programs,
            inherit_env,
            allowed_env,
            default_timeout,
            max_timeout,
            max_stdout_bytes,
            max_stderr_bytes,
        } = self;
        if !valid_timeout(default_timeout)
            || !valid_timeout(max_timeout)
            || default_timeout > max_timeout
            || allowed_env.len() > MAX_ENV_VARS
        {
            return Err(ProcessPolicyError::InvalidBounds);
        }
        if allowed_env.iter().any(|key| !valid_environment_key(key)) {
            return Err(ProcessPolicyError::InvalidEnvironmentKey);
        }
        if !output_fits_bound(max_stdout_bytes, max_stderr_bytes) {
            return Err(ProcessPolicyError::OutputTooLarge);
        }
        if let ProgramPolicy::AllowList(programs) = &allowed_programs {
            if programs.is_empty() || programs.iter().any(|program| !valid_program(program)) {
                return Err(ProcessPolicyError::InvalidProgram);
            }
        }
        Ok(Self {
            enabled,
            allowed_programs,
            inherit_env,
            allowed_env,
            default_timeout,
            max_timeout,
            max_stdout_bytes,
            max_stderr_bytes,
        })
    }

    pub fn disabled() -> Self {
        Self::new(false, ProgramPolicy::Any, false, BTreeSet::new())
            .expect("the disabled process policy is valid")
    }

    pub fn coding_agent_local() -> Self {
        let mut allowed_env = BTreeSet::new();
        allowed_env.insert("PATH".to_owned());
        #[cfg(windows)]
        {
            allowed_env.insert("SYSTEMROOT".to_owned());
            allowed_env.insert("TEMP".to_owned());
            allowed_env.insert("TMP".to_owned());
            allowed_env.insert("USERPROFILE".to_owned());
            allowed_env.insert("CARGO_HOME".to_owned());
            allowed_env.insert("RUSTUP_HOME".to_owned());
            allowed_env.insert("ProgramFiles(x86)".to_owned());
            allowed_env.insert("ProgramFiles".to_owned());
        }

        #[cfg(not(windows))]
        let allowed_programs = vec!["cargo", "rustc", "rustfmt", "rg"];
        #[cfg(windows)]
        let allowed_programs = vec![
            "cargo",
            "rustc",
            "rustfmt",
            "rg",
            "cargo.exe",
            "rustc.exe",
            "rustfmt.exe",
            "rg.exe",
        ];
        let allowed_programs =
            ProgramPolicy::allow_list(allowed_programs).expect("local programs are valid");

        Self::new(true, allowed_programs, true, allowed_env)
            .expect("the local coding process policy is valid")
            .with_limits(
                DEFAULT_TIMEOUT,
                MAX_TIMEOUT,
                DEFAULT_MAX_OUTPUT_BYTES,
                DEFAULT_MAX_OUTPUT_BYTES,
            )
            .expect("the local coding process limits are valid")
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub const fn allowed_programs(&self) -> &ProgramPolicy {
        &self.allowed_programs
    }

    pub const fn inherit_env(&self) -> bool {
        self.inherit_env
    }

    pub const fn allowed_env(&self) -> &BTreeSet<String> {
        &self.allowed_env
    }

    pub const fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    pub const fn max_timeout(&self) -> Duration {
        self.max_timeout
    }

    pub const fn max_stdout_bytes(&self) -> usize {
        self.max_stdout_bytes
    }

    pub const fn max_stderr_bytes(&self) -> usize {
        self.max_stderr_bytes
    }
}

impl fmt::Debug for ProcessPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessPolicy")
            .field("enabled", &self.enabled)
            .field("allowed_programs", &"[redacted]")
            .field("inherit_env", &self.inherit_env)
            .field("allowed_env", &"[redacted]")
            .field("default_timeout", &self.default_timeout)
            .field("max_timeout", &self.max_timeout)
            .field("max_stdout_bytes", &self.max_stdout_bytes)
            .field("max_stderr_bytes", &self.max_stderr_bytes)
            .finish()
    }
}

pub(crate) fn valid_program(program: &str) -> bool {
    !program.is_empty()
        && program.len() <= MAX_PROGRAM_BYTES
        && !program.as_bytes().contains(&0)
        && !program.chars().any(char::is_control)
}

pub(crate) fn valid_argument(argument: &str) -> bool {
    argument.len() <= MAX_ARGUMENT_BYTES && !argument.as_bytes().contains(&0)
}

pub(crate) fn valid_environment_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_ENV_KEY_BYTES
        && key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || valid_windows_key_punctuation(byte)
        })
}

#[cfg(windows)]
const fn valid_windows_key_punctuation(byte: u8) -> bool {
    matches!(byte, b'(' | b')')
}

#[cfg(not(windows))]
const fn valid_windows_key_punctuation(_: u8) -> bool {
    false
}

pub(crate) fn valid_environment_value(value: &str) -> bool {
    value.len() <= MAX_ENV_VALUE_BYTES && !value.as_bytes().contains(&0)
}

pub(crate) fn valid_timeout(timeout: Duration) -> bool {
    (MIN_TIMEOUT..=MAX_TIMEOUT).contains(&timeout)
}

pub(crate) fn output_fits_bound(stdout_bytes: usize, stderr_bytes: usize) -> bool {
    stdout_bytes
        .checked_add(stderr_bytes)
        .and_then(|total| total.checked_mul(OUTPUT_ESCAPE_MULTIPLIER))
        .and_then(|total| total.checked_add(OUTPUT_FIXED_ENVELOPE_BYTES))
        .is_some_and(|total| total <= MAX_TOOL_OUTPUT_BYTES)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use super::*;

    #[test]
    fn policy_constructors_are_checked_and_debug_redacted() {
        let policy = ProcessPolicy::coding_agent_local();
        assert!(policy.enabled());
        assert!(policy.inherit_env());
        assert!(policy.allowed_env().contains("PATH"));
        for program in ["cargo", "rustc", "rustfmt", "rg"] {
            assert!(policy.allowed_programs().allows(program));
        }
        assert!(!policy.allowed_programs().allows("sh"));
        assert!(!policy.allowed_programs().allows("bash"));
        assert!(
            !policy
                .allowed_programs()
                .allows(&std::env::current_exe().unwrap().to_string_lossy())
        );
        #[cfg(windows)]
        for program in ["cargo.exe", "rustc.exe", "rustfmt.exe", "rg.exe"] {
            assert!(policy.allowed_programs().allows(program));
        }
        #[cfg(windows)]
        for key in [
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "ProgramFiles(x86)",
            "ProgramFiles",
        ] {
            assert!(policy.allowed_env().contains(key));
        }
        assert!(!format!("{policy:?}").contains("cargo"));
        assert!(!format!("{policy:?}").contains("PATH"));

        let programs = ProgramPolicy::allow_list(["literal;program", "cargo"]).unwrap();
        assert!(programs.allows("literal;program"));
        assert!(!programs.allows("literal"));
        assert!(!format!("{programs:?}").contains("cargo"));

        let bad = ProcessPolicy::new(true, ProgramPolicy::any(), false, BTreeSet::new())
            .unwrap()
            .with_limits(Duration::from_millis(99), Duration::from_secs(1), 1, 1);
        assert_eq!(bad, Err(ProcessPolicyError::InvalidBounds));
    }

    #[test]
    fn environment_keys_keep_platform_grammar_and_bounds() {
        for key in ["", "A=B", "A\0B", "A\nB", "A\tB", "A;B", "A-B"] {
            assert!(!valid_environment_key(key), "unexpectedly accepted {key:?}");
        }
        assert!(!valid_environment_key(&"A".repeat(MAX_ENV_KEY_BYTES + 1)));
        assert!(valid_environment_key("PATH"));
        assert!(valid_environment_key("ProgramFiles"));
        #[cfg(windows)]
        assert!(valid_environment_key("ProgramFiles(x86)"));
        #[cfg(not(windows))]
        assert!(!valid_environment_key("ProgramFiles(x86)"));
    }

    #[test]
    fn output_bound_rejects_caps_that_cannot_fit_encoded_json() {
        assert!(output_fits_bound(16 * 1024, 16 * 1024));
        assert!(!output_fits_bound(32 * 1024, 32 * 1024));
        assert!(!output_fits_bound(usize::MAX, 1));
    }
}
