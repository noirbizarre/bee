//! Bounded, noninteractive host command execution with drop-safe termination.

use std::{path::Path, process::Stdio, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

use super::{Code, Context, Failure, MAX_OUTPUT_BYTES, MIN_OUTPUT_BYTES, Tool};

/// Host-owned defaults and ceilings. Contents are excluded from diagnostics.
#[derive(Clone)]
pub struct Settings {
    shell: String,
    timeout_ms: u64,
    max_timeout_ms: u64,
    output_limit: usize,
    max_output_limit: usize,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Settings([redacted])")
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shell: "bash".into(),
            timeout_ms: 120_000,
            max_timeout_ms: 600_000,
            output_limit: 65536,
            max_output_limit: MAX_OUTPUT_BYTES,
        }
    }
}

impl Settings {
    /// Read and validate `[tools.bash]` from merged launch-time configuration.
    pub fn from_config(config: &crate::config::Config) -> crate::Result<Self> {
        let mut settings = Self::default();
        let failure = |key: &str, reason| crate::Error::BashSetting {
            key: key.into(),
            path: config
                .origin(key)
                .map_or_else(|| "<configuration>".into(), |p| p.display().to_string()),
            reason,
            instruction: "use a Bash executable name or absolute path, positive timeouts, and output budgets between 256 and 1048576; defaults must not exceed ceilings",
        };
        if let Some(tools) = config.get("tools")
            && !tools.is_table()
        {
            return Err(failure("tools", "must be a table"));
        }
        let Some(value) = config.get("tools.bash") else {
            return Ok(settings);
        };
        let table = value
            .as_table()
            .ok_or_else(|| failure("tools.bash", "must be a table"))?;
        for (key, value) in table {
            // Unknown names can themselves contain secrets; diagnose the owning table.
            if ![
                "shell",
                "timeout_ms",
                "max_timeout_ms",
                "output_limit",
                "max_output_limit",
            ]
            .contains(&key.as_str())
            {
                let mut error = failure("tools.bash", "contains an unknown setting");
                if let crate::Error::BashSetting { path, .. } = &mut error
                    && let Some(origin) = config.origin(&format!("tools.bash.{key}"))
                {
                    *path = origin.display().to_string();
                }
                return Err(error);
            }
            let dotted = format!("tools.bash.{key}");
            if key == "shell" {
                let shell = value
                    .as_str()
                    .ok_or_else(|| failure(&dotted, "must be a string"))?;
                if shell.is_empty()
                    || shell.contains('\0')
                    || (!Path::new(shell).is_absolute()
                        && Path::new(shell).components().count() != 1)
                    || shell.contains(['/', '\\']) && !Path::new(shell).is_absolute()
                {
                    return Err(failure(
                        &dotted,
                        "must name an executable or an absolute path",
                    ));
                }
                settings.shell = shell.into();
                continue;
            }
            let number = value
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| failure(&dotted, "must be a positive integer"))?;
            match key.as_str() {
                "timeout_ms" | "max_timeout_ms" => {
                    if tokio::time::Instant::now()
                        .checked_add(Duration::from_millis(number))
                        .is_none()
                    {
                        return Err(failure(&dotted, "exceeds the supported timeout range"));
                    }
                    if key == "timeout_ms" {
                        settings.timeout_ms = number;
                    } else {
                        settings.max_timeout_ms = number;
                    }
                }
                _ => {
                    let number = usize::try_from(number)
                        .ok()
                        .filter(|n| (MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES).contains(n))
                        .ok_or_else(|| failure(&dotted, "is outside the output budget range"))?;
                    if key == "output_limit" {
                        settings.output_limit = number;
                    } else {
                        settings.max_output_limit = number;
                    }
                }
            }
        }
        for (key, invalid) in [
            ("timeout_ms", settings.timeout_ms > settings.max_timeout_ms),
            (
                "output_limit",
                settings.output_limit > settings.max_output_limit,
            ),
        ] {
            if invalid {
                let key = if table.contains_key(key) {
                    format!("tools.bash.{key}")
                } else {
                    format!("tools.bash.max_{key}")
                };
                return Err(failure(
                    &key,
                    "makes the default exceed its configured ceiling",
                ));
            }
        }
        Ok(settings)
    }
}

/// Execute Bash commands under a validated host policy.
#[derive(Default)]
pub struct Bash {
    settings: Settings,
}

impl Bash {
    /// Construct a tool without inspecting the host or starting a process.
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }
}

/// A noninteractive command and optional host-bounded limits.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Bash source, executed without interpolation by Bee.
    #[schemars(length(min = 1))]
    pub command: String,
    /// Requested timeout in milliseconds, at most the host ceiling.
    #[schemars(range(min = 1))]
    pub timeout_ms: Option<u64>,
    /// Complete serialized result budget, at most the host ceiling.
    #[schemars(range(min = 256, max = 1048576))]
    pub output_limit: Option<usize>,
}

/// Completed process information, including partial output on timeout.
#[derive(Serialize)]
pub struct Output {
    /// Lossy UTF-8 prefix of standard output.
    pub stdout: String,
    /// Lossy UTF-8 prefix of standard error.
    pub stderr: String,
    /// Exit code, absent for signal termination.
    pub exit_code: Option<i32>,
    /// Unix terminating signal, absent on other platforms or normal exit.
    pub signal: Option<i32>,
    /// Whether the deadline required termination.
    pub timed_out: bool,
    /// Whether standard output exceeded capture capacity.
    pub stdout_truncated: bool,
    /// Whether standard error exceeded capture capacity.
    pub stderr_truncated: bool,
}

fn execution() -> Failure {
    Failure::new(
        Code::Execution,
        "Command I/O failed; check the working directory and available host resources.",
    )
}

// The guard outlives wait/capture futures, including an outer ACP abort.
struct Process {
    child: Child,
    #[cfg(unix)]
    group: Option<nix::unistd::Pid>,
}

impl Process {
    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn capture(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    bytes: &mut Vec<u8>,
    truncated: &mut bool,
    capacity: usize,
    context: &Context,
    observed: &std::sync::atomic::AtomicU64,
) -> std::io::Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        let offset = observed.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
        let keep = (crate::presentation::TERMINAL_BYTES as u64)
            .saturating_sub(offset)
            .min(count as u64) as usize;
        if keep > 0 {
            context
                .emit(crate::presentation::Artifact::TerminalOutput(
                    chunk[..keep].to_vec(),
                ))
                .map_err(|_| std::io::Error::other("terminal presentation failed"))?;
        }
        let keep = count.min(capacity - bytes.len());
        bytes.extend_from_slice(&chunk[..keep]);
        *truncated |= keep < count;
        // Ready pipes must not starve the current-thread runtime's cancel/timer work.
        tokio::task::yield_now().await;
    }
}

struct Accounting<'a> {
    context: &'a Context,
    observed: &'a std::sync::atomic::AtomicU64,
}

impl Drop for Accounting<'_> {
    fn drop(&mut self) {
        // Abortable drops the reader futures before idle; retain omission accounting on that path too.
        let omitted = self
            .observed
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(crate::presentation::TERMINAL_BYTES as u64);
        let _ = self
            .context
            .emit(crate::presentation::Artifact::TerminalOmitted(omitted));
    }
}

impl Tool for Bash {
    fn permissions(&self, input: &Input) -> Vec<crate::permissions::Check> {
        vec![crate::permissions::Check::command(&input.command)]
    }
    fn kind(&self) -> &'static str {
        "execute"
    }
    type Input = Input;
    type Output = Output;
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a noninteractive Bash command in the session directory. Returns separate stdout/stderr, exit status, timeout and truncation flags. Commands run with host privileges; cwd is not a sandbox."
    }

    fn preflight(&self, input: &Input) -> Result<(), Failure> {
        let timeout = input.timeout_ms.unwrap_or(self.settings.timeout_ms);
        let budget = input.output_limit.unwrap_or(self.settings.output_limit);
        if input.command.is_empty()
            || input.command.contains('\0')
            || timeout == 0
            || timeout > self.settings.max_timeout_ms
            || !(MIN_OUTPUT_BYTES..=self.settings.max_output_limit).contains(&budget)
        {
            return Err(Failure::new(
                Code::InvalidArguments,
                "Use a nonempty command without NUL and positive limits within the configured host ceilings.",
            ));
        }
        tokio::time::Instant::now()
            .checked_add(Duration::from_millis(timeout))
            .ok_or_else(|| {
                Failure::new(
                    Code::InvalidArguments,
                    "Timeout exceeds the supported range; request a shorter timeout.",
                )
            })?;
        Ok(())
    }

    async fn execute(&self, input: Input, context: &Context) -> Result<Output, Failure> {
        self.preflight(&input)?;
        let timeout = input.timeout_ms.unwrap_or(self.settings.timeout_ms);
        let budget = input.output_limit.unwrap_or(self.settings.output_limit);
        // Human authorization time is not part of the process execution budget.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
        let mut command = Command::new(&self.settings.shell);
        command
            .args(["--noprofile", "--norc", "-c", &input.command])
            .env_remove("BASH_ENV")
            .current_dir(context.cwd())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|error| Failure::new(Code::Execution, match error.kind() {
            std::io::ErrorKind::NotFound => "Bash or the working directory was not found; install Bash, configure tools.bash.shell, and check the session directory.",
            std::io::ErrorKind::PermissionDenied => "Bash could not start due to permissions; check executable and working-directory access.",
            _ => "Bash could not start; check the configured executable, session directory, and host process resources.",
        }))?;
        let mut process = Process {
            #[cfg(unix)]
            group: child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .map(nix::unistd::Pid::from_raw),
            child,
        };
        context.emit(crate::presentation::Artifact::TerminalStarted {
            command: crate::presentation::bounded(
                &input.command,
                crate::presentation::CALL_BYTES / 4,
            ),
            cwd: context.cwd().to_path_buf(),
        })?;
        let mut stdout = process.child.stdout.take().ok_or_else(execution)?;
        let mut stderr = process.child.stderr.take().ok_or_else(execution)?;
        // Six serialized bytes per input byte covers JSON controls and lossy UTF-8.
        // Reserve fixed metadata even at the minimum registry budget.
        let capacity = (budget.min(context.output_limit()) - 192) / 12;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut out_cut, mut err_cut) = (false, false);
        let mut timed_out = false;
        let observed = std::sync::atomic::AtomicU64::new(0);
        let accounting = Accounting {
            context,
            observed: &observed,
        };
        let status;
        {
            let drain = async {
                tokio::try_join!(
                    capture(
                        &mut stdout,
                        &mut out,
                        &mut out_cut,
                        capacity,
                        context,
                        &observed
                    ),
                    capture(
                        &mut stderr,
                        &mut err,
                        &mut err_cut,
                        capacity,
                        context,
                        &observed
                    )
                )
            };
            let work = async {
                let wait = async {
                    let status = process.child.wait().await?;
                    // A successful foreground shell must not leave background jobs behind.
                    process.terminate();
                    Ok::<_, std::io::Error>(status)
                };
                let (status, _) = tokio::try_join!(wait, drain)?;
                Ok::<_, std::io::Error>(status)
            };
            match tokio::time::timeout_at(deadline, work).await {
                Ok(result) => status = result.map_err(|_| execution())?,
                Err(_) => {
                    timed_out = true;
                    process.terminate();
                    status = process.child.wait().await.map_err(|_| execution())?;
                    // Do not wait for EOF from descendants that escaped our process group.
                    out_cut = true;
                    err_cut = true;
                }
            }
        }
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        drop(accounting);
        context.emit(crate::presentation::Artifact::TerminalExited {
            code: status.code().and_then(|code| u32::try_from(code).ok()),
            signal: signal.and_then(|number| {
                #[cfg(unix)]
                {
                    nix::sys::signal::Signal::try_from(number)
                        .ok()
                        .map(|s| s.to_string())
                }
                #[cfg(not(unix))]
                {
                    let _ = number;
                    None
                }
            }),
        })?;
        Ok(Output {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            exit_code: status.code(),
            signal,
            timed_out,
            stdout_truncated: out_cut,
            stderr_truncated: err_cut,
        })
    }
}
