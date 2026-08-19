use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use crate::workspace_v2::RelativePath;

use super::super::process::{
    MAX_ARGUMENT_BYTES, MAX_ARGUMENTS, MAX_CWD_BYTES, MAX_ENV_VALUE_BYTES, MAX_ENV_VARS,
    MAX_PROGRAM_BYTES, MAX_TIMEOUT, MAX_TOTAL_ARGUMENT_BYTES, MIN_TIMEOUT, valid_argument,
    valid_environment_key, valid_environment_value, valid_program, valid_timeout,
};
use super::super::{ProcessPolicy, Tool, ToolContext, ToolError, ToolFuture, ToolSpec};
use super::{failure, success};

const INVALID_ARGUMENTS: &str = "tool arguments are invalid";
const POLICY_DENIED: &str = "command execution is not allowed";
const CWD_INVALID: &str = "command working directory is invalid";
const SPAWN_FAILED: &str = "command could not be started";
const DESCRIPTION: &str = "Run one executable with structured arguments. The workspace-relative cwd is only trusted-host pre-spawn validation; the child uses ambient host authority.";
const READ_BUFFER_BYTES: usize = 8 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Runs one direct child process with structured arguments and bounded output.
///
/// The model-relative cwd is validated through [`Workspace::command_cwd`] before
/// spawn. `Command::current_dir(Path)` receives only an ambient host path on a
/// trusted, non-adversarial host filesystem. This validation does not provide a
/// capability identity or process sandbox, does not claim protection against a
/// later host-filesystem replacement, and does not promise process-tree cleanup.
#[derive(Clone)]
pub struct RunCommandTool {
    policy: Arc<ProcessPolicy>,
}

impl RunCommandTool {
    pub fn new(policy: Arc<ProcessPolicy>) -> Self {
        Self { policy }
    }
}

impl Tool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "run_command".parse().expect("builtin name is valid"),
            DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "program": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_PROGRAM_BYTES
                    },
                    "args": {
                        "type": "array",
                        "maxItems": MAX_ARGUMENTS,
                        "items": {
                            "type": "string",
                            "maxLength": MAX_ARGUMENT_BYTES
                        }
                    },
                    "cwd": {
                        "type": "string",
                        "maxLength": MAX_CWD_BYTES
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": MIN_TIMEOUT.as_millis(),
                        "maximum": MAX_TIMEOUT.as_millis()
                    },
                    "env": {
                        "type": "object",
                        "maxProperties": MAX_ENV_VARS,
                        "additionalProperties": {
                            "type": "string",
                            "maxLength": MAX_ENV_VALUE_BYTES
                        }
                    }
                },
                "required": ["program"],
                "additionalProperties": false
            }),
        )
        .expect("builtin spec is valid")
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a> {
        let policy = Arc::clone(&self.policy);
        Box::pin(async move { execute_command(ctx, policy, args).await })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCommandArguments {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom("value must not be null"));
    }
    String::deserialize(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(serde::de::Error::custom("value must not be null"));
    }
    u64::deserialize(value)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

struct ParsedCommand {
    program: String,
    args: Vec<String>,
    cwd: Option<RelativePath>,
    timeout: Duration,
    env: BTreeMap<String, String>,
}

async fn execute_command(
    ctx: ToolContext<'_>,
    policy: Arc<ProcessPolicy>,
    value: Value,
) -> Result<super::super::ToolOutput, ToolError> {
    let arguments = match serde_json::from_value::<RunCommandArguments>(value) {
        Ok(arguments) => arguments,
        Err(_) => return failure(INVALID_ARGUMENTS),
    };
    let parsed = match validate_arguments(arguments, &policy) {
        Ok(parsed) => parsed,
        Err(ArgumentError::Invalid) => return failure(INVALID_ARGUMENTS),
        Err(ArgumentError::Denied) => return failure(POLICY_DENIED),
    };
    if ctx.cancellation().is_cancelled() {
        return Err(ToolError::Cancelled);
    }

    let cwd = match ctx.workspace().command_cwd(parsed.cwd.as_ref()).await {
        Ok(cwd) => cwd,
        Err(_) => return failure(CWD_INVALID),
    };
    if ctx.cancellation().is_cancelled() {
        return Err(ToolError::Cancelled);
    }

    let mut command = Command::new(&parsed.program);
    command
        .args(&parsed.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    if policy.inherit_env() {
        for key in policy.allowed_env() {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
    for (key, value) in &parsed.env {
        command.env(key, value);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return failure(SPAWN_FAILED),
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = kill_and_wait(&mut child).await;
        return Err(ToolError::Internal);
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = kill_and_wait(&mut child).await;
        return Err(ToolError::Internal);
    };

    run_child(
        child,
        stdout,
        stderr,
        ctx.cancellation().clone(),
        parsed.timeout,
        policy.max_stdout_bytes(),
        policy.max_stderr_bytes(),
    )
    .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArgumentError {
    Invalid,
    Denied,
}

fn validate_arguments(
    arguments: RunCommandArguments,
    policy: &ProcessPolicy,
) -> Result<ParsedCommand, ArgumentError> {
    if !valid_program(&arguments.program)
        || arguments.args.len() > MAX_ARGUMENTS
        || arguments
            .args
            .iter()
            .any(|argument| !valid_argument(argument))
        || arguments.args.iter().map(String::len).sum::<usize>() > MAX_TOTAL_ARGUMENT_BYTES
    {
        return Err(ArgumentError::Invalid);
    }
    let cwd = match arguments.cwd {
        None => None,
        Some(value) if value.len() <= MAX_CWD_BYTES => {
            Some(value.parse().map_err(|_| ArgumentError::Invalid)?)
        }
        Some(_) => return Err(ArgumentError::Invalid),
    };
    let timeout = match arguments.timeout_ms {
        None => policy.default_timeout(),
        Some(milliseconds) => {
            let timeout = Duration::from_millis(milliseconds);
            if !valid_timeout(timeout) {
                return Err(ArgumentError::Invalid);
            }
            timeout
        }
    };
    if timeout > policy.max_timeout() {
        return Err(ArgumentError::Denied);
    }
    if arguments.env.len() > MAX_ENV_VARS
        || arguments.env.keys().any(|key| !valid_environment_key(key))
        || arguments
            .env
            .values()
            .any(|value| !valid_environment_value(value))
    {
        return Err(ArgumentError::Invalid);
    }
    if !policy.enabled()
        || !policy.allowed_programs().allows(&arguments.program)
        || arguments
            .env
            .keys()
            .any(|key| !policy.allowed_env().contains(key))
    {
        return Err(ArgumentError::Denied);
    }
    Ok(ParsedCommand {
        program: arguments.program,
        args: arguments.args,
        cwd,
        timeout,
        env: arguments.env,
    })
}

async fn run_child(
    mut child: Child,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    cancellation: CancellationToken,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<super::super::ToolOutput, ToolError> {
    let started = Instant::now();
    let mut timeout_sleep = Box::pin(tokio::time::sleep(timeout));
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_buffer = [0_u8; READ_BUFFER_BYTES];
    let mut stderr_buffer = [0_u8; READ_BUFFER_BYTES];
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut child_done = false;
    let mut exit_code = None;

    while !child_done || !stdout_done || !stderr_done {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                if !child_done {
                    cleanup_after_kill(
                        &mut child,
                        &mut stdout,
                        &mut stderr,
                        &mut stdout_bytes,
                        &mut stderr_bytes,
                        max_stdout_bytes,
                        max_stderr_bytes,
                    )
                    .await?;
                }
                return Err(ToolError::Cancelled);
            }
            _ = &mut timeout_sleep => {
                if !child_done {
                    exit_code = cleanup_after_kill(
                        &mut child,
                        &mut stdout,
                        &mut stderr,
                        &mut stdout_bytes,
                        &mut stderr_bytes,
                        max_stdout_bytes,
                        max_stderr_bytes,
                    )
                    .await?;
                }
                return render_output(
                    exit_code,
                    stdout_bytes,
                    stderr_bytes,
                    true,
                    true,
                    started,
                );
            }
            result = child.wait(), if !child_done => {
                let status = result.map_err(|_| ToolError::Internal)?;
                exit_code = status.code();
                child_done = true;
            }
            result = read_bounded(&mut stdout, &mut stdout_buffer, &mut stdout_bytes, max_stdout_bytes), if !stdout_done => {
                match result {
                    Ok(ReadResult::Eof) => stdout_done = true,
                    Ok(ReadResult::Data) => {}
                    Ok(ReadResult::Overflow) => {
                        if !child_done {
                            exit_code = cleanup_after_kill(
                                &mut child,
                                &mut stdout,
                                &mut stderr,
                                &mut stdout_bytes,
                                &mut stderr_bytes,
                                max_stdout_bytes,
                                max_stderr_bytes,
                            )
                            .await?;
                        }
                        return render_output(
                            exit_code,
                            stdout_bytes,
                            stderr_bytes,
                            false,
                            true,
                            started,
                        );
                    }
                    Err(_) => {
                        if !child_done {
                            cleanup_after_kill(
                                &mut child,
                                &mut stdout,
                                &mut stderr,
                                &mut stdout_bytes,
                                &mut stderr_bytes,
                                max_stdout_bytes,
                                max_stderr_bytes,
                            )
                            .await?;
                        }
                        return Err(ToolError::Internal);
                    }
                }
            }
            result = read_bounded(&mut stderr, &mut stderr_buffer, &mut stderr_bytes, max_stderr_bytes), if !stderr_done => {
                match result {
                    Ok(ReadResult::Eof) => stderr_done = true,
                    Ok(ReadResult::Data) => {}
                    Ok(ReadResult::Overflow) => {
                        if !child_done {
                            exit_code = cleanup_after_kill(
                                &mut child,
                                &mut stdout,
                                &mut stderr,
                                &mut stdout_bytes,
                                &mut stderr_bytes,
                                max_stdout_bytes,
                                max_stderr_bytes,
                            )
                            .await?;
                        }
                        return render_output(
                            exit_code,
                            stdout_bytes,
                            stderr_bytes,
                            false,
                            true,
                            started,
                        );
                    }
                    Err(_) => {
                        if !child_done {
                            cleanup_after_kill(
                                &mut child,
                                &mut stdout,
                                &mut stderr,
                                &mut stdout_bytes,
                                &mut stderr_bytes,
                                max_stdout_bytes,
                                max_stderr_bytes,
                            )
                            .await?;
                        }
                        return Err(ToolError::Internal);
                    }
                }
            }
        }
    }

    render_output(exit_code, stdout_bytes, stderr_bytes, false, false, started)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadResult {
    Data,
    Eof,
    Overflow,
}

async fn drain_after_kill(
    stdout: &mut ChildStdout,
    stderr: &mut ChildStderr,
    stdout_bytes: &mut Vec<u8>,
    stderr_bytes: &mut Vec<u8>,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    deadline: TokioInstant,
) -> Result<bool, ToolError> {
    let mut stdout_buffer = [0_u8; READ_BUFFER_BYTES];
    let mut stderr_buffer = [0_u8; READ_BUFFER_BYTES];
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut output_truncated = false;

    while !stdout_done || !stderr_done {
        let read = tokio::time::timeout_at(deadline, async {
            tokio::select! {
                biased;
                result = read_bounded(
                    stdout,
                    &mut stdout_buffer,
                    stdout_bytes,
                    max_stdout_bytes,
                ), if !stdout_done => (true, result),
                result = read_bounded(
                    stderr,
                    &mut stderr_buffer,
                    stderr_bytes,
                    max_stderr_bytes,
                ), if !stderr_done => (false, result),
            }
        })
        .await;

        let (is_stdout, result) = match read {
            Ok(read) => read,
            Err(_) => return Ok(true),
        };
        match result {
            Ok(ReadResult::Data) => {}
            Ok(ReadResult::Eof) => {
                if is_stdout {
                    stdout_done = true;
                } else {
                    stderr_done = true;
                }
            }
            Ok(ReadResult::Overflow) => {
                output_truncated = true;
                if is_stdout {
                    stdout_done = true;
                } else {
                    stderr_done = true;
                }
            }
            Err(_) => return Err(ToolError::Internal),
        }
    }
    Ok(output_truncated)
}

async fn read_bounded<R>(
    reader: &mut R,
    buffer: &mut [u8; READ_BUFFER_BYTES],
    output: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<ReadResult>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let remaining = limit.saturating_sub(output.len());
    let read_limit = remaining.saturating_add(1).min(buffer.len());
    let count = reader.read(&mut buffer[..read_limit]).await?;
    if count == 0 {
        return Ok(ReadResult::Eof);
    }
    if count > remaining {
        output.extend_from_slice(&buffer[..remaining]);
        return Ok(ReadResult::Overflow);
    }
    output.extend_from_slice(&buffer[..count]);
    Ok(ReadResult::Data)
}

fn cleanup_deadline() -> TokioInstant {
    TokioInstant::now() + CLEANUP_TIMEOUT
}

async fn cleanup_after_kill(
    child: &mut Child,
    stdout: &mut ChildStdout,
    stderr: &mut ChildStderr,
    stdout_bytes: &mut Vec<u8>,
    stderr_bytes: &mut Vec<u8>,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
) -> Result<Option<i32>, ToolError> {
    let deadline = cleanup_deadline();
    let exit_code = kill_and_wait_until(child, deadline).await?;
    let _ = drain_after_kill(
        stdout,
        stderr,
        stdout_bytes,
        stderr_bytes,
        max_stdout_bytes,
        max_stderr_bytes,
        deadline,
    )
    .await?;
    Ok(exit_code)
}

async fn kill_and_wait(child: &mut Child) -> Result<Option<i32>, ToolError> {
    kill_and_wait_until(child, cleanup_deadline()).await
}

async fn kill_and_wait_until(
    child: &mut Child,
    deadline: TokioInstant,
) -> Result<Option<i32>, ToolError> {
    let kill_result = child.start_kill();
    let wait_result = tokio::time::timeout_at(deadline, child.wait()).await;
    match (kill_result, wait_result) {
        (Ok(()), Ok(Ok(status))) => Ok(status.code()),
        // A kill error is acceptable only because this wait status proves the
        // direct child had already exited.
        (Err(_), Ok(Ok(status))) => Ok(status.code()),
        (_, Ok(Err(_))) | (_, Err(_)) => Err(ToolError::Internal),
    }
}

fn render_output(
    exit_code: Option<i32>,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
    timed_out: bool,
    output_truncated: bool,
    started: Instant,
) -> Result<super::super::ToolOutput, ToolError> {
    let stdout_lossy = std::str::from_utf8(&stdout_bytes).is_err();
    let stderr_lossy = std::str::from_utf8(&stderr_bytes).is_err();
    let output = RunCommandOutput {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        timed_out,
        cancelled: false,
        output_truncated,
        stdout_lossy,
        stderr_lossy,
        duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    };
    let text = serde_json::to_string(&output).map_err(|_| ToolError::Internal)?;
    success(text)
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RunCommandOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    cancelled: bool,
    output_truncated: bool,
    stdout_lossy: bool,
    stderr_lossy: bool,
    duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use std as runtime;
    use std::env;
    use std::io::Write as _;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::super::super::{
        InteractionClient, ProcessPolicy, ProgramPolicy, ToolContext, ToolError, ToolOutput,
    };
    use super::*;
    use crate::workspace_v2::{Workspace, WorkspaceAccess};

    static NEXT_PROCESS_ROOT: AtomicU64 = AtomicU64::new(0);

    fn helper_program() -> String {
        env::current_exe().unwrap().to_string_lossy().into_owned()
    }

    fn helper_policy(
        stdout: usize,
        stderr: usize,
        timeout: Duration,
        allowed_env: &[&str],
    ) -> Arc<ProcessPolicy> {
        Arc::new(
            ProcessPolicy::new(
                true,
                ProgramPolicy::allow_list([helper_program()]).unwrap(),
                true,
                allowed_env
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
            )
            .unwrap()
            .with_limits(
                timeout,
                timeout.max(Duration::from_millis(100)),
                stdout,
                stderr,
            )
            .unwrap(),
        )
    }

    fn context<'a>(
        workspace: &'a Workspace,
        cancellation: CancellationToken,
        interactions: &'a InteractionClient,
    ) -> ToolContext<'a> {
        ToolContext::new(
            crate::ids_v2::SessionId::new().unwrap(),
            crate::ids_v2::TurnId::new().unwrap(),
            workspace,
            cancellation,
            interactions,
        )
        .unwrap()
    }

    fn helper_args(mode: &str) -> Value {
        json!({
            "program": helper_program(),
            "args": [
                "--exact",
                "tools_v2::builtins::run_command::tests::helper_process_mode_is_a_test_only_child_entrypoint",
                "--nocapture"
            ],
            "env": {"MINICORE_P7_HELPER_MODE": mode}
        })
    }

    fn descendant_args(ready_marker: &Path, exit_marker: &Path) -> Value {
        json!({
            "program": helper_program(),
            "args": [
                "--exact",
                "tools_v2::builtins::run_command::tests::helper_process_mode_is_a_test_only_child_entrypoint",
                "--nocapture"
            ],
            "env": {
                "MINICORE_P7_HELPER_MODE": "descendant",
                "MINICORE_P7_READY_MARKER": ready_marker.to_string_lossy(),
                "MINICORE_P7_EXIT_MARKER": exit_marker.to_string_lossy(),
                "MINICORE_P7_HOLD_MS": "5000"
            }
        })
    }

    async fn execute(
        policy: Arc<ProcessPolicy>,
        cancellation: CancellationToken,
        args: Value,
    ) -> Result<ToolOutput, ToolError> {
        let sequence = NEXT_PROCESS_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "minicore-p7-process-unit-{}-{sequence}",
            std::process::id()
        ));
        let _ = runtime::fs::remove_dir_all(&root);
        runtime::fs::create_dir_all(&root).unwrap();
        runtime::fs::create_dir_all(root.join("nested")).unwrap();
        let workspace = Workspace::open(&root, WorkspaceAccess::ReadWrite).unwrap();
        let (interactions, _receiver) = InteractionClient::channel();
        let result = RunCommandTool::new(policy)
            .execute(context(&workspace, cancellation, &interactions), args)
            .await;
        workspace.shutdown().await.unwrap();
        let _ = runtime::fs::remove_dir_all(root);
        result
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cleanup_deadline_is_fixed_and_can_be_advanced_without_wall_clock_waiting() {
        let deadline = cleanup_deadline();
        assert_eq!(deadline - TokioInstant::now(), CLEANUP_TIMEOUT);
        assert!(CLEANUP_TIMEOUT <= Duration::from_secs(5));
        tokio::time::advance(CLEANUP_TIMEOUT).await;
        assert!(TokioInstant::now() >= deadline);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normal_nonzero_exit_race_does_not_turn_a_wait_proof_into_internal() {
        let mut child = Command::new(helper_program())
            .args([
                "--exact",
                "tools_v2::builtins::run_command::tests::helper_process_mode_is_a_test_only_child_entrypoint",
                "--nocapture",
            ])
            .env("MINICORE_P7_HELPER_MODE", "echo")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            kill_and_wait_until(&mut child, cleanup_deadline()).await,
            Ok(Some(7))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn helper_process_returns_structured_nonzero_output_and_literal_args_without_shell() {
        let policy = helper_policy(
            4 * 1024,
            4 * 1024,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE"],
        );
        let output = execute(policy, CancellationToken::new(), helper_args("echo"))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(output.text()).unwrap();
        assert_eq!(value["exit_code"], 7);
        assert!(value["stdout"].as_str().unwrap().contains("helper_process"));
        assert_eq!(value["cancelled"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_uses_the_validated_nested_workspace_cwd() {
        let policy = helper_policy(
            4096,
            4096,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE"],
        );
        let mut args = helper_args("cwd");
        args["cwd"] = json!("nested");
        let output = execute(policy, CancellationToken::new(), args)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(output.text()).unwrap();
        assert!(value["stdout"].as_str().unwrap().contains("nested"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn policy_disabled_and_program_mismatch_are_fixed_failure_outputs() {
        let disabled = Arc::new(ProcessPolicy::disabled());
        let result = execute(disabled, CancellationToken::new(), helper_args("echo"))
            .await
            .unwrap();
        assert!(result.is_error());
        assert_eq!(result.text(), "command execution is not allowed");

        let policy = helper_policy(1024, 1024, Duration::from_secs(5), &[]);
        let mut args = helper_args("echo");
        args["program"] = json!("not-the-helper");
        let result = execute(policy, CancellationToken::new(), args)
            .await
            .unwrap();
        assert_eq!(result.text(), "command execution is not allowed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_returns_bounded_truthful_output_and_cancellation_returns_cancelled() {
        let policy = helper_policy(
            1024,
            1024,
            Duration::from_millis(100),
            &["MINICORE_P7_HELPER_MODE"],
        );
        let timed_out = execute(
            policy.clone(),
            CancellationToken::new(),
            helper_args("sleep"),
        )
        .await
        .unwrap();
        let value: Value = serde_json::from_str(timed_out.text()).unwrap();
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["cancelled"], false);

        let cancellation = CancellationToken::new();
        let mut operation = Box::pin(execute(policy, cancellation.clone(), helper_args("sleep")));
        let result = tokio::select! {
            result = &mut operation => result,
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                cancellation.cancel();
                operation.await
            }
        };
        assert_eq!(result, Err(ToolError::Cancelled));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn descendant_pipe_handles_remain_bounded_by_the_original_user_timeout() {
        let sequence = NEXT_PROCESS_ROOT.fetch_add(1, Ordering::Relaxed);
        let ready_marker = env::temp_dir().join(format!(
            "minicore-p7-descendant-ready-{}-{sequence}",
            std::process::id()
        ));
        let exit_marker = env::temp_dir().join(format!(
            "minicore-p7-descendant-exit-{}-{sequence}",
            std::process::id()
        ));
        let _ = runtime::fs::remove_file(&ready_marker);
        let _ = runtime::fs::remove_file(&exit_marker);
        let policy = helper_policy(
            1024,
            1024,
            Duration::from_secs(3),
            &[
                "MINICORE_P7_HELPER_MODE",
                "MINICORE_P7_READY_MARKER",
                "MINICORE_P7_EXIT_MARKER",
                "MINICORE_P7_HOLD_MS",
            ],
        );
        let output = tokio::time::timeout(
            Duration::from_secs(8),
            execute(
                policy,
                CancellationToken::new(),
                descendant_args(&ready_marker, &exit_marker),
            ),
        )
        .await
        .expect("the user timeout must bound inherited pipe handles")
        .unwrap();
        let value: Value = serde_json::from_str(output.text()).unwrap();
        assert_eq!(value["timed_out"], true, "unexpected output: {value}");
        assert_eq!(
            value["output_truncated"], true,
            "unexpected output: {value}"
        );
        assert_eq!(value["exit_code"], 0, "unexpected output: {value}");
        assert!(
            ready_marker.exists(),
            "the direct child must spawn its descendant"
        );

        tokio::time::sleep(Duration::from_millis(5_800)).await;
        assert!(exit_marker.exists(), "the finite test descendant must exit");
        let _ = runtime::fs::remove_file(ready_marker);
        let _ = runtime::fs::remove_file(exit_marker);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_direct_child_exit_does_not_wait_for_inherited_pipes() {
        let sequence = NEXT_PROCESS_ROOT.fetch_add(1, Ordering::Relaxed);
        let ready_marker = env::temp_dir().join(format!(
            "minicore-p7-cancel-descendant-ready-{}-{sequence}",
            std::process::id()
        ));
        let exit_marker = env::temp_dir().join(format!(
            "minicore-p7-cancel-descendant-exit-{}-{sequence}",
            std::process::id()
        ));
        let _ = runtime::fs::remove_file(&ready_marker);
        let _ = runtime::fs::remove_file(&exit_marker);
        let policy = helper_policy(
            1024,
            1024,
            Duration::from_secs(30),
            &[
                "MINICORE_P7_HELPER_MODE",
                "MINICORE_P7_READY_MARKER",
                "MINICORE_P7_EXIT_MARKER",
                "MINICORE_P7_HOLD_MS",
            ],
        );
        let cancellation = CancellationToken::new();
        let mut operation = Box::pin(execute(
            policy,
            cancellation.clone(),
            descendant_args(&ready_marker, &exit_marker),
        ));
        for _ in 0..1_000 {
            if ready_marker.exists() {
                break;
            }
            tokio::select! {
                result = &mut operation => panic!("command completed before the direct child exit marker: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        assert!(ready_marker.exists());
        tokio::select! {
            result = &mut operation => panic!("command completed before cancellation: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), operation)
            .await
            .expect("cancellation must not wait for inherited pipe handles");
        assert_eq!(result, Err(ToolError::Cancelled));

        tokio::time::sleep(Duration::from_millis(5_800)).await;
        assert!(exit_marker.exists(), "the finite test descendant must exit");
        let _ = runtime::fs::remove_file(ready_marker);
        let _ = runtime::fs::remove_file(exit_marker);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_caps_truncate_without_deadlock_and_env_is_cleared_and_allowlisted() {
        let policy = helper_policy(
            64,
            64,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE", "MINICORE_P7_ALLOWED"],
        );
        let mut args = helper_args("large");
        args["env"] = json!({
            "MINICORE_P7_HELPER_MODE": "large",
            "MINICORE_P7_ALLOWED": "visible"
        });
        let output = execute(policy, CancellationToken::new(), args)
            .await
            .unwrap();
        assert!(output.text().len() <= 256 * 1024);
        let value: Value = serde_json::from_str(output.text()).unwrap();
        assert_eq!(value["output_truncated"], true);
        assert!(value["stdout"].as_str().unwrap().len() <= 64);
        assert!(value["stderr"].as_str().unwrap().len() <= 64);
        assert_eq!(value["stdout_lossy"], false);
        assert_eq!(value["stderr_lossy"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_utf8_is_lossy_and_optional_null_or_unknown_arguments_fail_closed() {
        let policy = helper_policy(
            1024,
            1024,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE"],
        );
        let output = execute(
            policy.clone(),
            CancellationToken::new(),
            helper_args("invalid"),
        )
        .await
        .unwrap();
        let value: Value = serde_json::from_str(output.text()).unwrap();
        assert_eq!(value["stdout_lossy"], true);

        for args in [
            json!({}),
            json!({"program": helper_program(), "cwd": null}),
            json!({"program": helper_program(), "timeout_ms": null}),
            json!({"program": helper_program(), "extra": true}),
            json!({"program": helper_program(), "timeout_ms": 99}),
        ] {
            let output = execute(policy.clone(), CancellationToken::new(), args)
                .await
                .unwrap();
            assert!(output.is_error());
            assert_eq!(output.text(), "tool arguments are invalid");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn environment_precedence_is_explicit_and_unallowlisted_host_values_are_absent() {
        let policy = helper_policy(
            4096,
            4096,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE", "MINICORE_P7_ALLOWED"],
        );
        let mut args = helper_args("env");
        args["env"] = json!({
            "MINICORE_P7_HELPER_MODE": "env",
            "MINICORE_P7_ALLOWED": "model-value"
        });
        let output = execute(policy, CancellationToken::new(), args)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(output.text()).unwrap();
        let stdout = value["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("allowed=Ok(\"model-value\")"),
            "unexpected helper output: {stdout:?}"
        );
        assert!(stdout.contains("secret=Err"));
        assert!(stdout.contains("path=Err"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_started_future_kills_the_direct_child() {
        let sequence = NEXT_PROCESS_ROOT.fetch_add(1, Ordering::Relaxed);
        let marker = env::temp_dir().join(format!(
            "minicore-p7-process-marker-{}-{sequence}",
            std::process::id()
        ));
        let _ = runtime::fs::remove_file(&marker);
        let policy = helper_policy(
            1024,
            1024,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE", "MINICORE_P7_MARKER"],
        );
        let mut args = helper_args("marker");
        args["env"] = json!({
            "MINICORE_P7_HELPER_MODE": "marker",
            "MINICORE_P7_MARKER": marker
        });
        let mut operation = Box::pin(execute(policy, CancellationToken::new(), args));
        tokio::select! {
            _ = &mut operation => panic!("marker helper completed before it could be dropped"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        drop(operation);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!marker.exists());
        let _ = runtime::fs::remove_file(marker);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cwd_and_env_validation_rejects_escape_and_unallowed_model_keys() {
        let policy = helper_policy(1024, 1024, Duration::from_secs(5), &[]);
        for cwd in ["../outside", "/tmp"] {
            let mut args = helper_args("echo");
            args["cwd"] = json!(cwd);
            let output = execute(policy.clone(), CancellationToken::new(), args)
                .await
                .unwrap();
            assert_eq!(output.text(), "tool arguments are invalid");
        }
        let mut args = helper_args("echo");
        args["env"] = json!({"SECRET": "hidden"});
        let output = execute(policy, CancellationToken::new(), args)
            .await
            .unwrap();
        assert_eq!(output.text(), "command execution is not allowed");
    }

    #[test]
    fn structured_arguments_keep_shell_metacharacters_literal() {
        let policy = helper_policy(
            1024,
            1024,
            Duration::from_secs(5),
            &["MINICORE_P7_HELPER_MODE"],
        );
        let arguments = RunCommandArguments {
            program: helper_program(),
            args: vec!["literal;$(not-shell)".to_owned()],
            cwd: None,
            timeout_ms: None,
            env: [("MINICORE_P7_HELPER_MODE".to_owned(), "echo".to_owned())]
                .into_iter()
                .collect(),
        };
        let parsed = validate_arguments(arguments, &policy).unwrap();
        assert_eq!(parsed.args, ["literal;$(not-shell)"]);
    }

    #[test]
    fn helper_process_mode_is_a_test_only_child_entrypoint() {
        let Ok(mode) = env::var("MINICORE_P7_HELPER_MODE") else {
            return;
        };
        match mode.as_str() {
            "echo" => {
                println!("{}", env::args().skip(1).collect::<Vec<_>>().join("|"));
                std::process::exit(7);
            }
            "sleep" => std::thread::sleep(Duration::from_secs(30)),
            "large" => {
                print!("{}", "o".repeat(64));
                eprint!("{}", "e".repeat(64));
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                print!("{}", "o".repeat(100_000));
                eprint!("{}", "e".repeat(100_000));
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
            }
            "invalid" => {
                let _ = std::io::stdout().write_all(&[0xff, 0xfe, b'x']);
                let _ = std::io::stdout().flush();
            }
            "env" => {
                println!(
                    "allowed={:?}|secret={:?}|path={:?}",
                    env::var("MINICORE_P7_ALLOWED"),
                    env::var("MINICORE_P7_SECRET"),
                    env::var("PATH")
                );
            }
            "cwd" => {
                println!("{}", env::current_dir().unwrap().display());
            }
            "marker" => {
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(path) = env::var("MINICORE_P7_MARKER") {
                    let _ = runtime::fs::write(path, b"child completed");
                }
            }
            "descendant" => {
                let mut descendant = std::process::Command::new(env::current_exe().unwrap());
                descendant
                    .args([
                        "--exact",
                        "tools_v2::builtins::run_command::tests::helper_process_mode_is_a_test_only_child_entrypoint",
                        "--nocapture",
                    ])
                    .env("MINICORE_P7_HELPER_MODE", "hold_pipe")
                    .spawn()
                    .unwrap();
                if let Ok(path) = env::var("MINICORE_P7_READY_MARKER") {
                    let _ = runtime::fs::write(path, b"direct child is exiting");
                }
                std::thread::sleep(Duration::from_millis(50));
                std::process::exit(0);
            }
            "hold_pipe" => {
                let hold_ms = env::var("MINICORE_P7_HOLD_MS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(800);
                std::thread::sleep(Duration::from_millis(hold_ms));
                if let Ok(path) = env::var("MINICORE_P7_EXIT_MARKER") {
                    let _ = runtime::fs::write(path, b"descendant exited");
                }
                std::process::exit(0);
            }
            _ => std::process::exit(9),
        }
    }
}
