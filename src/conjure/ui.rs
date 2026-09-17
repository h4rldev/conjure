//! Build output monitor.
//!
//! Despite the name, this draws nothing itself: it wraps indicatif's
//! [`MultiProgress`] to present one coherent view of a build - transient
//! spinners/bars for work in progress and persistent status lines for
//! milestones (artifacts built, cache reuse, link fallbacks). Child-process
//! stdout/stderr are forwarded line-by-line through the progress manager so tool
//! output interleaves with our lines instead of tearing the display.
//!
//! [`Ui::println`] suspends the bars and writes straight to stderr on purpose:
//! indicatif only flushes its own `println` lines while a bar is actively
//! drawing, so a status printed from a quiet phase - or after the last bar has
//! finished - would be silently dropped.

/***********************************************************************/

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use miette::{IntoDiagnostic, Result};
use owo_colors::OwoColorize;
use std::{
  borrow::Cow,
  ffi::{OsStr, OsString},
  io::{BufRead, BufReader, Read, Write},
  path::Path,
  process::{Command, Stdio},
  sync::atomic::{AtomicU8, Ordering},
  time::Duration,
};

/***********************************************************************/

const SPIN_CHARS: &str = "⠉⠙⠘⠰⢠⣠⣀⣄⡄⠆⠃⠋";
static VERBOSE: AtomicU8 = AtomicU8::new(0);

pub fn set_verbose(level: u8) {
  VERBOSE.store(level, Ordering::Relaxed);
}

/// Leading glyph for a status line, or the message unchanged when `status` is
/// `None` (e.g. forwarded tool output).
pub enum StepStatus {
  Success,
  Failure,
  Info,
}

/// A build's progress display and child-process runner.
///
/// One `Ui` owns a single [`MultiProgress`]; every spinner/bar created through
/// it shares the same live region. Clone of the underlying `MultiProgress` is
/// used to forward subprocess output from worker threads.
pub struct Ui {
  mp: MultiProgress,
  verbose: u8,
}

impl Ui {
  /// The only fallible primitive: spawn `args` in `dir` with `env`, forward
  /// both streams, and return `None` on exit 0 or the exit code otherwise.
  ///
  /// `env` is layered on top of the inherited environment. It exists for the
  /// MSVC toolchain, whose `cl`/`link.exe` need `PATH`/`INCLUDE`/`LIB` captured
  /// from `vcvars`; gnu and external dep builds pass an empty slice.
  fn run<S: AsRef<OsStr>>(
    &self,
    dir: &Path,
    args: &[S],
    env: &[(OsString, OsString)],
  ) -> Result<Option<i32>> {
    let joined = args
      .iter()
      .map(|a| a.as_ref().to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");

    if self.verbose > 0 {
      self.println(None, format!("(cd {} && {joined})", dir.display()))?;
      if self.verbose > 1 {
        for (k, v) in env {
          self.println(
            None,
            format!("  {}={}", k.to_string_lossy(), v.to_string_lossy()),
          )?;
        }
      }
    }

    let mut child = Command::new(&args[0])
      .args(&args[1..])
      .current_dir(dir)
      .env("CLICOLOR_FORCE", "1")
      .env("FORCE_COLOR", "1")
      .env("CMAKE_COLOR_MAKEFILE", "ON")
      .envs(env.iter().cloned())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .into_diagnostic()
      .map_err(|e| {
        miette::miette!(
          help = "Is it in path?",
          "Failed to run `{joined}`: {e}"
        )
      })?;

    // Drain both pipes on separate threads so a chatty child can't deadlock on
    // a full pipe buffer while we wait for it to exit.
    let mp = self.mp.clone();
    let forward = move |stream: Box<dyn Read + Send>, mp: MultiProgress| {
      std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
          let _ = mp.println(line);
        }
      })
    };

    let t1 = forward(Box::new(child.stdout.take().unwrap()), mp.clone());
    let t2 = forward(Box::new(child.stderr.take().unwrap()), mp.clone());

    let status = child.wait().into_diagnostic()?;
    t1.join().expect("Failed to join stdout thread");
    t2.join().expect("Failed to join stderr thread");

    if status.success() {
      Ok(None)
    } else {
      Ok(status.code())
    }
  }

  /// Create a UI and hide the cursor until this value is dropped. Presenting an
  /// upright cursor over a live progress region looks broken, hence the escape
  /// sequences rather than a framework feature.
  pub fn new() -> Self {
    eprint!("\x1b[?25l");
    let _ = std::io::stdout().flush();
    Self {
      mp: MultiProgress::new(),
      verbose: VERBOSE.load(Ordering::Relaxed),
    }
  }

  pub fn spinner(&self, msg: impl Into<Cow<'static, str>>) -> ProgressBar {
    let pb = self.mp.add(ProgressBar::new_spinner());

    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
      ProgressStyle::default_spinner()
        .template("{spinner:.green} {msg} ({elapsed_precise:.dim})")
        .unwrap()
        .tick_chars(SPIN_CHARS),
    );
    pb.set_message(msg);
    pb
  }

  /// A determinate progress bar. `len == 0` yields an indeterminate bar, so
  /// callers that don't know the total up front can still show activity.
  pub fn bar(
    &self,
    len: u64,
    msg: impl Into<Cow<'static, str>>,
    suffix: &str,
  ) -> ProgressBar {
    let pb = if len == 0 {
      self.mp.add(ProgressBar::no_length())
    } else {
      self.mp.add(ProgressBar::new(len))
    };
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
      ProgressStyle::with_template(&format!(
        "{{spinner:.green}} {{msg}} [{{bar:40.cyan/blue}}] {{pos}}/{{len}} {suffix}"
      ))
      .unwrap()
      .progress_chars("=> ").tick_chars(SPIN_CHARS),
    );
    pb.set_message(msg.into());
    pb
  }

  /// A bar for one of several concurrent clones. The label is padded to a fixed
  /// width (based on the longest name in the batch) and the bar is a fixed
  /// width, so every row's `[` lands in the same column and no row wraps into
  /// the next.
  pub fn fetch_bar(
    &self,
    name: &str,
    name_w: usize,
    suffix: &str,
  ) -> ProgressBar {
    let width = 10 + name_w;
    let pb = self.mp.add(ProgressBar::no_length());
    pb.set_style(
      ProgressStyle::with_template(&format!(
        "{{spinner:.green}} {{msg}} [{{bar:38.cyan/blue}}] {{pos}}/{{len}} {suffix}"
      ))
      .unwrap()
      .progress_chars("=> ")
      .tick_chars(SPIN_CHARS),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message(format!("{:<width$}", format!("Fetching {name}")));
    pb
  }

  /// Replace a live spinner with a permanent one-line status for `header`.
  pub fn finish(&self, pb: &ProgressBar, status: &StepStatus, header: &str) {
    pb.set_style(ProgressStyle::default_spinner().template("{msg}").unwrap());
    pb.finish_with_message(mark(Some(status), header));
  }

  /// Print a persistent, status-prefixed line.
  ///
  /// Suspends the live bars and writes to stderr directly. See the module doc
  /// for why this bypasses [`MultiProgress::println`].
  pub fn println(
    &self,
    status: Option<&StepStatus>,
    msg: impl Into<Cow<'static, str>>,
  ) -> Result<()> {
    let line = mark(status, msg.into().as_ref());
    self.mp.suspend(|| eprintln!("{line}"));
    Ok(())
  }

  /// Like [`Self::exec`] but reports success/failure instead of erroring, so a
  /// caller can fall back (e.g. a raw link that fails -> the compiler driver).
  pub fn try_exec_env<S: AsRef<OsStr>>(
    &self,
    dir: &Path,
    args: &[S],
    env: &[(OsString, OsString)],
  ) -> Result<Option<i32>> {
    self.run(dir, args, env)
  }

  /// Run a command, failing the build (with the captured output already
  /// forwarded) if it exits non-zero.
  pub fn exec<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<()> {
    self.exec_env(dir, args, &[])
  }

  /// [`Self::exec`] with an explicit environment overlay.
  pub fn exec_env<S: AsRef<OsStr>>(
    &self,
    dir: &Path,
    args: &[S],
    env: &[(OsString, OsString)],
  ) -> Result<()> {
    let joined = args
      .iter()
      .map(|a| a.as_ref().to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");

    match self.run(dir, args, env)? {
      None => Ok(()),
      Some(code) => {
        let inner = miette::miette!(
          help = "Check the output above for more information",
          "`{joined}` failed in {} with exit status: {}",
          dir.display(),
          code
        );
        Err(inner.wrap_err(format!("Failed to run `{joined}`")))
      }
    }
  }

  /// Run a command under a spinner that resolves to a success/failure line.
  pub fn run_step_env<S: AsRef<OsStr>>(
    &self,
    header: String,
    dir: &Path,
    args: &[S],
    env: &[(OsString, OsString)],
  ) -> Result<()> {
    let pb = self.spinner(header.clone());
    let result = self.exec_env(dir, args, env);
    let status = if result.is_ok() {
      StepStatus::Success
    } else {
      StepStatus::Failure
    };
    self.finish(&pb, &status, &header);
    result
  }

  /// Run a dependency's sequence of build commands under a single spinner. The
  /// commands share one header because a dep build is one logical step even when
  /// it is several invocations (configure then make, etc.).
  pub fn run_dep<S: AsRef<OsStr>>(
    &self,
    header: String,
    commands: &[(&Path, &[S])],
  ) -> Result<()> {
    let pb = self.spinner(header.clone());
    let result = commands
      .iter()
      .try_for_each(|(dir, args)| self.exec(dir, args));
    let status = if result.is_ok() {
      StepStatus::Success
    } else {
      StepStatus::Failure
    };
    self.finish(&pb, &status, &header);
    result
  }

  /// Run `op` under a spinner. For work that isn't a subprocess (locking,
  /// network-independent steps) but still wants a progress line.
  pub fn wrap<T>(
    &self,
    header: String,
    op: impl FnOnce() -> Result<T>,
  ) -> Result<T> {
    let pb = self.spinner(header.clone());
    let result = op();

    match &result {
      Ok(_) => pb.finish_with_message(format!("✓ {header}")),
      Err(_) => pb.finish_with_message(format!("✗ {header}")),
    }

    result
  }
}

impl Drop for Ui {
  /// Restore the cursor hidden by [`Ui::new`].
  fn drop(&mut self) {
    eprint!("\x1b[?25h");
    let _ = std::io::stdout().flush();
  }
}

/// Prefix `msg` with the glyph for `status`, or leave it bare when there is
/// none.
pub fn mark(status: Option<&StepStatus>, msg: &str) -> String {
  match status {
    Some(StepStatus::Success) => format!("{} {}", "✓".green().bold(), msg),
    Some(StepStatus::Failure) => format!("{} {}", "✗".red().bold(), msg),
    Some(StepStatus::Info) => format!("{} {}", "ℹ".cyan().bold(), msg),
    None => msg.to_string(),
  }
}
