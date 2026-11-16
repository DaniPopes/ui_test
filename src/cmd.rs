use crate::config::AbortCheck;
use crate::display;
use std::{
    ffi::OsString,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
/// A command, its args and its environment. Used for
/// the main command, the dependency builder and the cfg-reader.
pub struct CommandBuilder {
    /// Path to the binary.
    pub program: PathBuf,
    /// Arguments to the binary.
    pub args: Vec<OsString>,
    /// A flag to prefix before the path to where output files should be written.
    pub out_dir_flag: Option<OsString>,
    /// A flag to set as the last flag in the command, so the `build` caller can
    /// append the filename themselves.
    pub input_file_flag: Option<OsString>,
    /// Environment variables passed to the binary that is executed.
    /// The environment variable is removed if the second tuple field is `None`
    pub envs: Vec<(OsString, Option<OsString>)>,
    /// A flag to add in order to make the command print the cfgs it supports
    pub cfg_flag: Option<OsString>,
}

impl CommandBuilder {
    /// Uses the `CARGO` env var or just a program named `cargo` and the argument `build`.
    pub fn cargo() -> Self {
        Self {
            program: PathBuf::from(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())),
            args: vec![
                "build".into(),
                "--color=never".into(),
                "--quiet".into(),
                "--jobs".into(),
                "1".into(),
            ],
            out_dir_flag: Some("--target-dir".into()),
            input_file_flag: Some("--manifest-path".into()),
            envs: vec![],
            cfg_flag: None,
        }
    }

    /// Uses the `RUSTC` env var or just a program named `rustc` and the argument `--error-format=json`.
    ///
    /// Take care to only append unless you actually meant to overwrite the defaults.
    /// Overwriting the defaults may make `//~ ERROR` style comments stop working.
    pub fn rustc() -> Self {
        Self {
            program: PathBuf::from(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into())),
            args: vec!["--error-format=json".into()],
            out_dir_flag: Some("--out-dir".into()),
            input_file_flag: None,
            envs: vec![],
            cfg_flag: Some("--print=cfg".into()),
        }
    }

    /// Build a `CommandBuilder` for a command without any argumemnts.
    /// You can still add arguments later.
    pub fn cmd(cmd: impl Into<PathBuf>) -> Self {
        Self {
            program: cmd.into(),
            args: vec![],
            out_dir_flag: None,
            input_file_flag: None,
            envs: vec![],
            cfg_flag: None,
        }
    }

    /// Render the command like you'd use it on a command line.
    pub fn display(&self) -> impl std::fmt::Display + '_ {
        struct Display<'a>(&'a CommandBuilder);
        impl std::fmt::Display for Display<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                for (var, val) in &self.0.envs {
                    if let Some(val) = val {
                        write!(f, "{var:?}={val:?} ")?;
                    }
                }
                display(&self.0.program).fmt(f)?;
                for arg in &self.0.args {
                    write!(f, " {arg:?}")?;
                }
                if let Some(flag) = &self.0.out_dir_flag {
                    write!(f, " {flag:?} OUT_DIR")?;
                }
                if let Some(flag) = &self.0.input_file_flag {
                    write!(f, " {flag:?}")?;
                }
                Ok(())
            }
        }
        Display(self)
    }

    /// Create a command with the given settings.
    pub fn build(&self, out_dir: &Path) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(self.args.iter());
        if let Some(flag) = &self.out_dir_flag {
            cmd.arg(flag).arg(out_dir);
        }
        if let Some(flag) = &self.input_file_flag {
            cmd.arg(flag);
        }
        self.apply_env(&mut cmd);
        cmd
    }

    pub(crate) fn apply_env(&self, cmd: &mut Command) {
        for (var, val) in self.envs.iter() {
            if let Some(val) = val {
                cmd.env(var, val);
            } else {
                cmd.env_remove(var);
            }
        }
    }
}

/// Runs `cmd` like [`Command::output`], killing it after `timeout` or once `abort` is set.
///
/// Returns the output so far and whether the command timed out. On Unix the command runs in its
/// own process group, and the whole group is killed.
pub(crate) fn output_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    abort: &AbortCheck,
) -> io::Result<(Output, bool)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    let mut child = cmd.spawn()?;

    // Read both pipes while waiting, so a full pipe cannot stall the command.
    let stdout = read_to_end(child.stdout.take().unwrap());
    let stderr = read_to_end(child.stderr.take().unwrap());

    let start = Instant::now();
    let mut delay = Duration::from_millis(1);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if abort.aborted() || start.elapsed() >= timeout {
            timed_out = !abort.aborted();
            kill(&mut child);
            break child.wait()?;
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(50));
    };

    let output = Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    };
    Ok((output, timed_out))
}

fn read_to_end(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Kills the child and, on Unix, its process group.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: `killpg` only sends a signal. The child leads a group with the child's pid, set by
    // `process_group(0)`, and is not yet reaped, so the pid is still its own.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn run(script: &str, timeout: Duration) -> (Output, bool, Duration) {
        let start = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        let (output, timed_out) =
            output_with_timeout(&mut cmd, timeout, &AbortCheck::default()).unwrap();
        (output, timed_out, start.elapsed())
    }

    #[test]
    fn fast_command_is_not_killed() {
        let (output, timed_out, _) = run("echo out; echo err >&2", Duration::from_secs(60));
        assert!(!timed_out);
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[test]
    fn slow_command_is_killed_with_its_output_so_far() {
        let (output, timed_out, elapsed) = run("echo before; sleep 60", Duration::from_millis(300));
        assert!(timed_out);
        assert_eq!(output.stdout, b"before\n");
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }

    #[test]
    fn grandchildren_are_killed_too() {
        // The background `sleep` keeps the pipes open until the whole group is killed.
        let (_, timed_out, elapsed) = run("sleep 60 & wait", Duration::from_millis(300));
        assert!(timed_out);
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    }
}
