/// This module despite its name, doesnt actually handle any real drawing, but simply a build output monitor
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use miette::{IntoDiagnostic, Result, WrapErr};
use owo_colors::OwoColorize;
use std::{
  borrow::Cow,
  ffi::OsStr,
  io::{BufRead, BufReader, Read, Write},
  path::Path,
  process::{Command, Stdio},
  time::Duration,
};

pub struct Ui {
  mp: MultiProgress,
}

pub enum StepStatus {
  Success,
  Failure,
  Info,
}

pub fn mark(status: Option<&StepStatus>, msg: &str) -> String {
  match status {
    Some(StepStatus::Success) => format!("{} {}", "✓".green().bold(), msg),
    Some(StepStatus::Failure) => format!("{} {}", "✗".red().bold(), msg),
    Some(StepStatus::Info) => format!("{} {}", "ℹ".cyan().bold(), msg),
    None => msg.to_string(),
  }
}

impl Ui {
  pub fn new() -> Self {
    eprint!("\x1b[?25l");
    let _ = std::io::stdout().flush();
    Self {
      mp: MultiProgress::new(),
    }
  }

  pub fn finish(&self, pb: &ProgressBar, status: &StepStatus, header: &str) {
    pb.set_style(ProgressStyle::default_spinner().template("{msg}").unwrap());
    pb.finish_with_message(mark(Some(status), header));
  }

  pub fn spinner(&self, msg: impl Into<Cow<'static, str>>) -> ProgressBar {
    let pb = self.mp.add(ProgressBar::new_spinner());
    let mut finito = vec!["⠁", "⠉", "⠙", "⠛", "⠟", "⠿", "⡿", "⣿"];
    let mut part2 = finito.iter().rev().cloned().collect::<Vec<_>>();
    finito.append(&mut part2);

    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
      ProgressStyle::default_spinner()
        .template("{spinner:.green} {msg} ({elapsed_precise:.dim})")
        .unwrap()
        .tick_strings(&finito),
    );
    pb.set_message(msg);
    pb
  }

  pub fn bar(&self, len: u64, msg: impl Into<Cow<'static, str>>, suffix: &str) -> ProgressBar {
    let pb = if len == 0 {
      self.mp.add(ProgressBar::no_length())
    } else {
      self.mp.add(ProgressBar::new(len))
    };
    pb.set_style(
      ProgressStyle::with_template(&format!(
        "{{spinner:.green}} {{msg}} [{{bar:40.cyan/blue}}] {{pos}}/{{len}} {suffix}"
      ))
      .unwrap()
      .progress_chars("=> "),
    );
    pb.set_message(msg.into());
    pb
  }

  pub fn exec<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<()> {
    let joined = args
      .iter()
      .map(|a| a.as_ref().to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");

    let mut child = Command::new(&args[0])
      .args(&args[1..])
      .current_dir(dir)
      .env("CLICOLOR_FORCE", "1")
      .env("FORCE_COLOR", "1")
      .env("CMAKE_COLOR_MAKEFILE", "ON")
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .into_diagnostic()
      .wrap_err(format!("failed to run `{joined}`"))?;

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
    t1.join().expect("failed to join stdout thread");
    t2.join().expect("failed to join stderr thread");

    if !status.success() {
      let inner = miette::miette!(
        help = "Check the output above for more information",
        "`{joined}` failed in {} with exit status: {}",
        dir.display(),
        status.code().unwrap_or(-1),
      );
      return Err(inner.wrap_err(format!("Failed to run `{joined}`")));
    }
    Ok(())
  }

  pub fn run_step<S: AsRef<OsStr>>(&self, header: String, dir: &Path, args: &[S]) -> Result<()> {
    let pb = self.spinner(header.clone());
    let result = self.exec(dir, args);
    let status = if result.is_ok() {
      StepStatus::Success
    } else {
      StepStatus::Failure
    };
    self.finish(&pb, &status, &header);
    result
  }

  pub fn run_dep<S: AsRef<OsStr>>(&self, header: String, commands: &[(&Path, &[S])]) -> Result<()> {
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

  pub fn wrap<T>(&self, header: String, op: impl FnOnce() -> Result<T>) -> Result<T> {
    let pb = self.spinner(header.clone());
    let result = op();

    match &result {
      Ok(_) => pb.finish_with_message(format!("✓ {header}")),
      Err(_) => pb.finish_with_message(format!("✗ {header}")),
    }

    result
  }

  pub fn println(
    &self,
    status: Option<&StepStatus>,
    msg: impl Into<Cow<'static, str>>,
  ) -> Result<()> {
    self
      .mp
      .println(mark(status, msg.into().as_ref()))
      .into_diagnostic()
  }
}

impl Drop for Ui {
  fn drop(&mut self) {
    eprint!("\x1b[?25h");
    let _ = std::io::stdout().flush();
  }
}
