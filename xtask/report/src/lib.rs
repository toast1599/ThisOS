//! What `cargo xtask` shows while it works. A command is a list of named
//! steps, run in order until one fails. A step captures the output of every
//! command it runs: interactive mode shows its last lines under a spinner
//! and the whole of it when the step fails, machine mode prints one ASCII
//! line per step and the failed step's output at the end, and both write
//! each step's output to a log file.
//!
//! The reporter is global, set up once by [`init`], so that the code that
//! runs commands needn't pass it around. Without it, as in the `qemu-test`
//! runner, every function here passes output straight through.

mod look;
mod parse;
mod screen;

use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitCode, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result, bail};

pub use look::Glyphs;
use look::Outcome;
use parse::{Lines, Progress};
use screen::Screen;

/// Who the output is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Someone at a terminal: spinners, bars, colors and a live tail.
    Interactive,
    /// CI and scripts: ASCII lines, a summary and the failed step's log.
    Machine,
}

/// A step's name. Steps of one kind that run one after another, like the
/// clippy pass of each crate, share a group: interactive mode shows them
/// under one header, which folds into one line once they all pass.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Name {
    pub group: Option<String>,
    pub item: String,
}

impl Name {
    pub fn grouped(group: impl Into<String>, item: impl Into<String>) -> Name {
        Name {
            group: Some(group.into()),
            item: item.into(),
        }
    }

    /// `clippy lib/console`: the name machine mode and the log files use.
    pub fn full(&self) -> String {
        match &self.group {
            Some(group) => format!("{group} {}", self.item),
            None => self.item.clone(),
        }
    }
}

impl From<&str> for Name {
    fn from(item: &str) -> Name {
        Name {
            group: None,
            item: item.to_owned(),
        }
    }
}

impl From<String> for Name {
    fn from(item: String) -> Name {
        Name { group: None, item }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub mode: Mode,
    pub glyphs: Glyphs,
    pub color: bool,
    /// Whether this runs under GitHub Actions, whose log folds groups.
    pub github: bool,
}

impl Settings {
    /// The settings for this process's environment, with `mode` and
    /// `glyphs`, from the command line, winning when given.
    pub fn detect(mode: Option<Mode>, glyphs: Option<Glyphs>) -> Result<Settings> {
        Settings::from_env(mode, glyphs, io::stderr().is_terminal(), &|name| {
            std::env::var(name).ok()
        })
    }

    fn from_env(
        mode: Option<Mode>,
        glyphs: Option<Glyphs>,
        terminal: bool,
        var: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Settings> {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let mode = mode.unwrap_or(if terminal && set("CI").is_none() {
            Mode::Interactive
        } else {
            Mode::Machine
        });
        let glyphs = match (glyphs, set("THISOS_GLYPHS")) {
            (Some(glyphs), _) => glyphs,
            (None, Some(value)) => value.parse().context("reading THISOS_GLYPHS")?,
            (None, None) => Glyphs::detect(&set),
        };
        Ok(Settings {
            mode,
            glyphs,
            color: mode == Mode::Interactive && set("NO_COLOR").is_none(),
            github: set("GITHUB_ACTIONS").as_deref() == Some("true"),
        })
    }
}

static REPORTER: OnceLock<Reporter> = OnceLock::new();

struct Reporter {
    settings: Settings,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    title: String,
    /// Every step's name, in order.
    plan: Vec<Name>,
    /// The longest full name in `plan`, which machine mode pads to.
    width: usize,
    done: Vec<Done>,
    current: Option<Current>,
    started: Option<Instant>,
    logs: Option<PathBuf>,
    /// Commands running now. Ctrl-C reaches them too, so while there are
    /// any, the step that runs them reports the interrupt.
    children: usize,
    interrupted: bool,
    /// Whether the screen is hidden for a prompt or an attached command.
    suspended: bool,
    screen: Option<Screen>,
}

struct Current {
    name: Name,
    /// Where the step is in the plan, from 0.
    index: usize,
    /// Whether it shows under its group's header: it has a group of more
    /// than one step.
    grouped: bool,
    /// The steps opened inside this one, which only label it.
    phases: Vec<String>,
    started: Instant,
    log: String,
    progress: Option<Progress>,
    expected: Option<u64>,
}

struct Done {
    name: String,
    outcome: Outcome,
    /// What a step that didn't pass printed, and its error.
    log: Option<String>,
}

fn reporter() -> Option<&'static Reporter> {
    REPORTER.get()
}

impl Reporter {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a line a running step's command printed.
    fn line(&self, text: &str) {
        let mut state = self.state();
        let State {
            current, screen, ..
        } = &mut *state;
        let Some(current) = current else {
            eprintln!("{text}");
            return;
        };
        current.log.push_str(text);
        current.log.push('\n');
        let plain = parse::strip(text);
        let before = current.progress;
        parse::update(&plain, &mut current.progress, current.expected);
        match screen {
            Some(screen) => {
                if current.progress != before {
                    screen.relabel(label(screen, current));
                }
                screen.tail(&plain);
            }
            None if self.settings.github => eprintln!("{text}"),
            None => {}
        }
    }
}

fn label(screen: &Screen, current: &Current) -> String {
    let name = if current.grouped {
        current.name.item.clone()
    } else {
        current.name.full()
    };
    screen.label(&name, &current.phases, current.progress)
}

/// How many steps the run of `plan[index]`'s group around it has.
fn group_size(plan: &[Name], index: usize) -> usize {
    let group = &plan[index].group;
    if group.is_none() {
        return 1;
    }
    let before = plan[..index]
        .iter()
        .rev()
        .take_while(|name| &name.group == group)
        .count();
    let after = plan[index..]
        .iter()
        .take_while(|name| &name.group == group)
        .count();
    before + after
}

/// Sets up the reporter, and Ctrl-C's handler.
pub fn init(settings: Settings) -> Result<()> {
    console::set_colors_enabled(settings.color);
    console::set_colors_enabled_stderr(settings.color);
    let screen = (settings.mode == Mode::Interactive).then(|| Screen::new(settings.glyphs));
    let reporter = Reporter {
        settings,
        state: Mutex::new(State {
            screen,
            ..State::default()
        }),
    };
    if REPORTER.set(reporter).is_err() {
        bail!("the reporter is already set up");
    }
    ctrlc::set_handler(interrupt).context("setting up the Ctrl-C handler")
}

/// Whether commands should color their output, as a terminal would get.
pub fn color() -> bool {
    reporter().map_or_else(|| io::stderr().is_terminal(), |r| r.settings.color)
}

/// Starts a run of the steps named in `plan`, titled `title`, and empties
/// `logs`, the directory its steps' logs go in.
pub fn start(title: &str, plan: Vec<Name>, logs: &Path) -> Result<()> {
    let Some(r) = reporter() else {
        return Ok(());
    };
    match fs::remove_dir_all(logs) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", logs.display()));
        }
        _ => {}
    }
    fs::create_dir_all(logs).with_context(|| format!("creating {}", logs.display()))?;
    let mut state = r.state();
    let state = &mut *state;
    state.width = plan.iter().map(|name| name.full().len()).max().unwrap_or(0);
    state.title = title.to_owned();
    state.started = Some(Instant::now());
    state.logs = Some(logs.to_owned());
    match &mut state.screen {
        Some(screen) => screen.start(title, plan.len()),
        None => {
            let s = if plan.len() == 1 { "" } else { "s" };
            eprintln!("{title}: {} step{s}", plan.len());
        }
    }
    state.plan = plan;
    Ok(())
}

/// A step for [`all`].
pub struct Step {
    pub name: Name,
    pub run: Box<dyn FnOnce() -> Result<()>>,
}

impl Step {
    pub fn new(name: impl Into<Name>, run: impl FnOnce() -> Result<()> + 'static) -> Step {
        Step {
            name: name.into(),
            run: Box::new(run),
        }
    }
}

/// Runs `steps` in order, as [`step`] does, until one fails.
pub fn all(steps: Vec<Step>) -> Result<()> {
    for Step { name, run } in steps {
        named(&name, run)?;
    }
    Ok(())
}

/// Runs `f` as the step `name`, which has to be the next one the run's plan
/// names, capturing the output of the commands it runs. Inside another
/// step, `name` is a phase of that one instead and only changes its label.
pub fn step<T>(name: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    named(&Name::from(name), f)
}

fn named<T>(name: &Name, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let Some(r) = reporter() else {
        return f();
    };
    // Its own statement, so that the lock is released before `f` runs.
    let phase = r.state().open(r.settings, name)?;
    if let Some(progress) = phase {
        let result = f();
        r.state().close_phase(progress);
        return result;
    }
    let result = f();
    let mut state = r.state();
    let current = state.current.take().expect("the step is still running");
    let outcome = match (&result, state.interrupted) {
        (Ok(_), _) => Outcome::Ok,
        (Err(_), true) => Outcome::Interrupted,
        (Err(_), false) => Outcome::Failed,
    };
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    state.end(r.settings, current, outcome, error.as_deref());
    result
}

impl State {
    /// Starts the step `name`, or, inside a running step, the phase `name`
    /// of it, and then returns the running step's progress to put back
    /// when the phase ends.
    fn open(&mut self, settings: Settings, name: &Name) -> Result<Option<Option<Progress>>> {
        if self.interrupted {
            bail!("interrupted");
        }
        if let Some(current) = &mut self.current {
            // A phase's progress, like a download's, ends with it.
            let progress = current.progress;
            let phase = name.full();
            current.log.push_str(&format!("> {phase}\n"));
            current.phases.push(phase);
            if let Some(screen) = &self.screen {
                screen.relabel(label(screen, current));
            }
            return Ok(Some(progress));
        }
        let index = self.done.len();
        assert_eq!(
            self.plan.get(index),
            Some(name),
            "step {name:?} isn't the next one in the plan"
        );
        let size = group_size(&self.plan, index);
        let current = Current {
            name: name.clone(),
            index,
            grouped: size > 1,
            phases: Vec::new(),
            started: Instant::now(),
            log: String::new(),
            progress: None,
            expected: None,
        };
        match &mut self.screen {
            Some(screen) => {
                let group = (size > 1).then(|| (name.group.as_deref().unwrap_or_default(), size));
                let label = label(screen, &current);
                screen.begin(group, label);
            }
            None if settings.github => {
                let counter = look::counter(index + 1, self.plan.len());
                eprintln!("::group::{counter} {}", name.full());
            }
            None => {}
        }
        self.current = Some(current);
        Ok(None)
    }

    fn close_phase(&mut self, progress: Option<Progress>) {
        if let Some(current) = &mut self.current {
            current.phases.pop();
            current.progress = progress;
            if let Some(screen) = &self.screen {
                screen.relabel(label(screen, current));
            }
        }
    }

    /// Ends the step `current` with `outcome`: writes its log, shows its
    /// line and records it.
    fn end(&mut self, settings: Settings, current: Current, outcome: Outcome, error: Option<&str>) {
        let number = current.index + 1;
        let full = current.name.full();
        let elapsed = current.started.elapsed();
        let log = current.log;
        if let Some(dir) = &self.logs {
            let path = dir.join(format!("{number:02}-{}.log", look::slug(&full)));
            if let Err(e) = fs::write(&path, parse::strip(&log).as_bytes()) {
                self.note(&format!("couldn't write {}: {e}", path.display()));
            }
        }
        let total = self.plan.len();
        match &mut self.screen {
            Some(screen) => {
                let next = self.plan.get(current.index + 1);
                let last_in_group = next.is_none_or(|next| next.group != current.name.group);
                let item = if current.grouped {
                    &current.name.item
                } else {
                    &full
                };
                let failed_log = (outcome == Outcome::Failed).then_some(log.as_str());
                screen.end(
                    current.grouped,
                    last_in_group,
                    item,
                    outcome,
                    elapsed,
                    failed_log,
                );
            }
            None => {
                if settings.github {
                    eprintln!("::endgroup::");
                }
                eprintln!(
                    "{}",
                    look::machine_line(number, total, outcome, &full, self.width, Some(elapsed))
                );
                if let (true, Outcome::Failed, Some(error)) = (settings.github, outcome, error) {
                    eprintln!(
                        "::error title={}::{}",
                        look::github_escape(&full, true),
                        look::github_escape(error, false)
                    );
                }
            }
        }
        self.done.push(Done {
            name: full,
            outcome,
            log: (outcome != Outcome::Ok).then_some(log),
        });
    }

    /// Shows `text` to the user, above the bars in interactive mode.
    fn note(&self, text: &str) {
        match &self.screen {
            Some(screen) if !self.suspended => screen.println(text),
            _ => eprintln!("{text}"),
        }
    }

    /// Prints the skipped steps and the summary, and returns the exit code.
    fn finish(&mut self, settings: Settings, error: Option<&anyhow::Error>) -> ExitCode {
        let code = if self.interrupted {
            ExitCode::from(130)
        } else if error.is_some() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
        // An interrupt says all there is to say; its error only repeats it.
        let error = error
            .filter(|_| !self.interrupted)
            .map(|e| format!("{e:#}"));
        let Some(started) = self.started else {
            if let Some(error) = error {
                self.note(&format!("error: {error}"));
            }
            return code;
        };
        let skipped = self
            .plan
            .get(self.done.len()..)
            .unwrap_or_default()
            .to_vec();
        let count = |outcome| {
            self.done
                .iter()
                .filter(|done| done.outcome == outcome)
                .count()
        };
        let result = if self.interrupted {
            "interrupted"
        } else if error.is_some() {
            "failed"
        } else {
            "passed"
        };
        let summary = look::summary(
            &self.title,
            result,
            started.elapsed(),
            &[
                (count(Outcome::Ok), "ok"),
                (count(Outcome::Failed), "failed"),
                (count(Outcome::Interrupted), "interrupted"),
                (skipped.len(), "skipped"),
            ],
        );
        match &mut self.screen {
            Some(screen) => {
                let skipped: Vec<_> = skipped
                    .iter()
                    .map(|name| (name.group.clone(), name.full()))
                    .collect();
                screen.finish(&skipped, &summary, result == "passed", error.as_deref());
            }
            None => {
                let total = self.plan.len();
                for (i, name) in skipped.iter().enumerate() {
                    let number = self.done.len() + 1 + i;
                    eprintln!(
                        "{}",
                        look::machine_line(
                            number,
                            total,
                            Outcome::Skipped,
                            &name.full(),
                            self.width,
                            None
                        )
                    );
                }
                eprintln!("{summary}");
                if let Some(error) = &error {
                    eprintln!("error: {error}");
                }
                // Under GitHub Actions the log is already in the step's group.
                let failed = self
                    .done
                    .iter()
                    .find(|done| done.outcome == Outcome::Failed);
                if let (
                    false,
                    Some(Done {
                        name,
                        log: Some(log),
                        ..
                    }),
                ) = (settings.github, failed)
                {
                    eprintln!("\n--- {name}: log ---");
                    eprint!("{log}");
                    eprintln!("--- end of log ---");
                }
            }
        }
        code
    }
}

/// Ends the run: prints the skipped steps, the summary and `result`'s
/// error, and returns the exit code, 130 after Ctrl-C.
pub fn finish(result: &Result<()>) -> ExitCode {
    let Some(r) = reporter() else {
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        };
    };
    r.state().finish(r.settings, result.as_ref().err())
}

/// Ctrl-C's handler. A running command gets the signal too and fails, and
/// the step that ran it reports the interrupt; with none running, the
/// handler ends the run itself.
fn interrupt() {
    let Some(r) = reporter() else {
        process::exit(130);
    };
    let mut state = r.state();
    state.interrupted = true;
    if state.children > 0 {
        return;
    }
    if state.suspended {
        // indicatif holds its own lock while hidden, so drawing would wait
        // for the prompt forever; only the log and a plain line are safe.
        if let (Some(current), Some(dir)) = (&state.current, &state.logs) {
            let path = dir.join(format!(
                "{:02}-{}.log",
                current.index + 1,
                look::slug(&current.name.full())
            ));
            if let Err(e) = fs::write(&path, parse::strip(&current.log).as_bytes()) {
                eprintln!("couldn't write {}: {e}", path.display());
            }
        }
        eprintln!("\ninterrupted");
        process::exit(130);
    }
    if let Some(current) = state.current.take() {
        state.end(r.settings, current, Outcome::Interrupted, None);
    }
    // The exit code is 130 whatever finish says.
    let _ = state.finish(r.settings, None);
    process::exit(130);
}

/// Where a captured command's standard output goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stdout {
    /// Into the step's log, like its standard error.
    Log,
    /// Back to the caller.
    Keep,
}

pub struct Output {
    pub status: ExitStatus,
    /// Standard output, when kept.
    pub stdout: Vec<u8>,
    /// Standard error, when it didn't go into a step's log.
    pub stderr: Option<Vec<u8>>,
}

/// Runs `cmd`. Inside a step its output goes into the step's log, but for
/// standard output with [`Stdout::Keep`]. Outside one, [`Stdout::Log`]
/// leaves both on the terminal and [`Stdout::Keep`] captures both.
pub fn run(cmd: &mut Command, stdout: Stdout) -> io::Result<Output> {
    let Some(r) = reporter().filter(|r| r.state().current.is_some()) else {
        return match stdout {
            Stdout::Log => cmd.status().map(|status| Output {
                status,
                stdout: Vec::new(),
                stderr: None,
            }),
            Stdout::Keep => cmd.output().map(|out| Output {
                status: out.status,
                stdout: out.stdout,
                stderr: Some(out.stderr),
            }),
        };
    };
    if r.settings.color {
        cmd.env("CARGO_TERM_COLOR", "always");
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = {
        let mut state = r.state();
        if state.interrupted {
            return Err(io::Error::other("interrupted"));
        }
        let child = cmd.spawn()?;
        state.children += 1;
        child
    };
    let mut out = child.stdout.take().expect("standard output is piped");
    let err = child.stderr.take().expect("standard error is piped");
    let kept = thread::scope(|scope| {
        let errs = scope.spawn(|| feed(r, err));
        let kept = match stdout {
            Stdout::Log => feed(r, out).map(|()| Vec::new()),
            Stdout::Keep => {
                let mut kept = Vec::new();
                out.read_to_end(&mut kept).map(|_| kept)
            }
        };
        let errs = errs.join().expect("reading standard error doesn't panic");
        errs.and(kept)
    });
    let status = child.wait();
    r.state().children -= 1;
    Ok(Output {
        status: status?,
        stdout: kept?,
        stderr: None,
    })
}

/// Passes what `from` yields to the running step, line by line.
fn feed(r: &Reporter, mut from: impl Read) -> io::Result<()> {
    let mut lines = Lines::default();
    let mut buf = [0; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for line in lines.push(&buf[..n]) {
            r.line(&line);
        }
    }
    if let Some(line) = lines.end() {
        r.line(&line);
    }
    Ok(())
}

/// Runs `cmd` with the terminal to itself, the screen hidden meanwhile: for
/// QEMU's serial console and sudo's password prompt.
pub fn attached(cmd: &mut Command) -> io::Result<ExitStatus> {
    let Some(r) = reporter() else {
        return cmd.status();
    };
    {
        let mut state = r.state();
        if state.interrupted {
            return Err(io::Error::other("interrupted"));
        }
        state.children += 1;
    }
    let status = suspend(|| cmd.status());
    r.state().children -= 1;
    status
}

/// Calls `f` with the screen hidden, so it can use the terminal.
pub fn suspend<T>(f: impl FnOnce() -> T) -> T {
    let Some(r) = reporter() else {
        return f();
    };
    let multi = {
        let mut state = r.state();
        state.suspended = true;
        state.screen.as_ref().map(Screen::multi)
    };
    let result = match multi {
        Some(multi) => multi.suspend(f),
        None => f(),
    };
    r.state().suspended = false;
    result
}

/// Adds `text` to the running step's log, line by line.
pub fn log(text: &str) {
    let running = reporter().filter(|r| r.state().current.is_some());
    match running {
        Some(r) => text.lines().for_each(|line| r.line(line)),
        None => eprintln!("{text}"),
    }
}

/// Shows `text` to the user, and adds it to the running step's log.
pub fn println(text: &str) {
    let Some(r) = reporter() else {
        eprintln!("{text}");
        return;
    };
    let mut state = r.state();
    if let Some(current) = &mut state.current {
        current.log.push_str(text);
        current.log.push('\n');
    }
    state.note(text);
}

/// Sets how many bytes the running step's command will write, so that its
/// dd `status=progress` lines show as a percentage.
pub fn expect_bytes(total: u64) {
    if let Some(r) = reporter()
        && let Some(current) = &mut r.state().current
    {
        current.expected = Some(total);
    }
}

/// Reports that the running step has moved `done` bytes, of `total` when
/// it's known.
pub fn bytes(done: u64, total: Option<u64>) {
    let Some(r) = reporter() else { return };
    let mut state = r.state();
    let state = &mut *state;
    if let Some(current) = &mut state.current {
        current.progress = Some(Progress::Bytes { done, total });
        if let Some(screen) = &state.screen {
            screen.relabel(label(screen, current));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{Glyphs, Mode, Settings};

    fn settings(terminal: bool, vars: &[(&str, &str)]) -> Settings {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        Settings::from_env(None, None, terminal, &|name| {
            vars.get(name).map(|value| (*value).to_owned())
        })
        .unwrap()
    }

    #[test]
    fn picks_the_mode_from_the_terminal_and_ci() {
        let utf8 = ("LANG", "C.UTF-8");
        assert_eq!(settings(true, &[utf8]).mode, Mode::Interactive);
        assert_eq!(settings(false, &[utf8]).mode, Mode::Machine);
        let ci = settings(true, &[utf8, ("CI", "true"), ("GITHUB_ACTIONS", "true")]);
        assert_eq!(ci.mode, Mode::Machine);
        assert!(ci.github && !ci.color);
        // An empty CI is unset, as for NO_COLOR.
        let local = settings(true, &[utf8, ("CI", ""), ("NO_COLOR", "1")]);
        assert_eq!(local.mode, Mode::Interactive);
        assert!(!local.color);
    }

    #[test]
    fn thisos_glyphs_overrides_detection_and_the_flag_overrides_it() {
        let vars = [
            ("LANG", "C.UTF-8"),
            ("TERM", "xterm-kitty"),
            ("THISOS_GLYPHS", "ascii"),
        ];
        assert_eq!(settings(true, &vars).glyphs, Glyphs::Ascii);
        let vars: HashMap<&str, &str> = vars.into_iter().collect();
        let flag = Settings::from_env(None, Some(Glyphs::Unicode), true, &|name| {
            vars.get(name).map(|value| (*value).to_owned())
        })
        .unwrap();
        assert_eq!(flag.glyphs, Glyphs::Unicode);
        assert!(
            Settings::from_env(None, None, true, &|name| {
                (name == "THISOS_GLYPHS").then(|| "emoji".to_owned())
            })
            .is_err()
        );
    }
}
