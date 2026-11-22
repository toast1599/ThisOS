//! Interactive mode: the running step's spinner and tail above an overall
//! bar, with finished steps printed above them. Steps of one group show
//! under a header while it runs and fold into one line once it passes.
//! Times and counts sit at the terminal's right edge.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use console::{Term, measure_text_width, style, truncate_str};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::look::{self, Glyphs, Outcome};
use crate::parse::Progress;

/// How many of a running step's last lines show under it.
const TAIL: usize = 5;

const TICK: Duration = Duration::from_millis(100);

pub struct Screen {
    multi: MultiProgress,
    glyphs: Glyphs,
    overall: Option<ProgressBar>,
    group: Option<Group>,
    step: Option<StepBars>,
}

/// A running group of steps.
struct Group {
    name: String,
    total: usize,
    started: Instant,
    header: ProgressBar,
    /// The finished steps' lines on screen, the oldest dropped first when
    /// they would push the running step off a short terminal.
    shown: VecDeque<ProgressBar>,
    /// Every finished step, to print the group in full when one fails.
    finished: Vec<(String, Duration)>,
}

struct StepBars {
    spinner: ProgressBar,
    tail: Vec<ProgressBar>,
    lines: VecDeque<String>,
    indent: &'static str,
}

fn style_of(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).expect("the templates here are valid")
}

/// The terminal's rows and columns.
fn size() -> (usize, usize) {
    let (rows, cols) = Term::stderr().size();
    (rows.into(), cols.into())
}

/// `left` and `right` on one line as wide as the terminal, `left` cut with
/// an ellipsis when they don't fit.
fn aligned(left: &str, right: &str) -> String {
    let (_, cols) = size();
    let right_width = measure_text_width(right);
    let left = truncate_str(left, cols.saturating_sub(right_width + 1), "…");
    let pad = cols.saturating_sub(measure_text_width(&left) + right_width);
    format!("{left}{}{right}", " ".repeat(pad))
}

/// A running group's header: its name and how many of its steps are done.
fn header(name: &str, done: usize, total: usize) -> String {
    format!(
        "{}  {}",
        style(name).bold(),
        style(format!("{done}/{total}")).cyan()
    )
}

impl Screen {
    pub fn new(glyphs: Glyphs) -> Screen {
        Screen {
            multi: MultiProgress::with_draw_target(ProgressDrawTarget::stderr()),
            glyphs,
            overall: None,
            group: None,
            step: None,
        }
    }

    /// The handle `suspend` hides the bars through, to call without holding
    /// the reporter's lock.
    pub fn multi(&self) -> MultiProgress {
        self.multi.clone()
    }

    /// Adds `bar` above the overall bar, below whatever is there already. A
    /// bar draws by itself until it is added, so set its prefix and message
    /// only on what this returns.
    fn add(&self, bar: ProgressBar) -> ProgressBar {
        match &self.overall {
            Some(overall) => self.multi.insert_before(overall, bar),
            None => self.multi.add(bar),
        }
    }

    fn remove(&self, bar: &ProgressBar) {
        bar.finish_and_clear();
        self.multi.remove(bar);
    }

    fn spinner(&self, indent: &'static str, message: String) -> ProgressBar {
        let bar = self.add(
            ProgressBar::new_spinner().with_style(
                style_of("{prefix}{spinner:.cyan} {wide_msg} {elapsed:.dim}")
                    .tick_strings(self.glyphs.spinner()),
            ),
        );
        bar.set_prefix(indent);
        bar.set_message(message);
        bar.enable_steady_tick(TICK);
        bar
    }

    pub fn start(&mut self, title: &str, total: usize) {
        let bar = self.multi.add(ProgressBar::new(total as u64));
        bar.set_style(
            style_of(
                "{prefix:.bold} {wide_bar:.cyan/dim} {pos}/{len}  {percent:>3}%  {elapsed:.dim}",
            )
            .progress_chars(self.glyphs.bar()),
        );
        bar.set_prefix(title.to_owned());
        bar.enable_steady_tick(TICK);
        self.overall = Some(bar);
    }

    /// Starts showing a step labelled `label`, in `group`, a name and how
    /// many steps it has, when it has one.
    pub fn begin(&mut self, group: Option<(&str, usize)>, label: String) {
        let indent = match group {
            Some((name, total)) => {
                if self.group.is_none() {
                    let header = self.spinner("", header(name, 0, total));
                    self.group = Some(Group {
                        name: name.to_owned(),
                        total,
                        started: Instant::now(),
                        header,
                        shown: VecDeque::new(),
                        finished: Vec::new(),
                    });
                }
                "  "
            }
            None => "",
        };
        let spinner = self.spinner(indent, label);
        self.step = Some(StepBars {
            spinner,
            tail: Vec::new(),
            lines: VecDeque::new(),
            indent,
        });
    }

    /// The running step's label: its name, phases and progress.
    pub fn label(&self, name: &str, phases: &[String], progress: Option<Progress>) -> String {
        let mut label = name.to_owned();
        for phase in phases {
            label = format!("{label} {} {phase}", style(self.glyphs.arrow()).dim());
        }
        if let Some(progress) = progress {
            label = format!("{label}  {}", style(look::progress(progress)).cyan());
        }
        label
    }

    pub fn relabel(&self, label: String) {
        if let Some(step) = &self.step {
            step.spinner.set_message(label);
        }
    }

    /// Shows `line`, ANSI codes stripped, in the running step's tail.
    pub fn tail(&mut self, line: &str) {
        let Some(step) = &self.step else { return };
        if line.trim().is_empty() {
            return;
        }
        let gutter = format!("{}{} ", step.indent, style(self.glyphs.gutter()).dim());
        let bar = (step.tail.len() < TAIL).then(|| {
            let bar =
                self.add(ProgressBar::new_spinner().with_style(style_of("{prefix}{wide_msg}")));
            bar.set_prefix(gutter.clone());
            bar
        });
        let Some(step) = &mut self.step else { return };
        step.tail.extend(bar);
        if step.lines.len() == TAIL {
            step.lines.pop_front();
        }
        let (_, cols) = size();
        let room = cols.saturating_sub(measure_text_width(&gutter));
        step.lines
            .push_back(truncate_str(&look::highlight(line), room, "…").into_owned());
        for (bar, line) in step.tail.iter().zip(&step.lines) {
            bar.set_message(line.clone());
        }
    }

    /// Replaces the running step's bars with its finished line, followed by
    /// `failed_log` when it failed. A step in a group adds its line under
    /// the group's header; the group's last step folds the group into one
    /// line if it passed, and a step that didn't pass prints the group out.
    pub fn end(
        &mut self,
        grouped: bool,
        last_in_group: bool,
        item: &str,
        outcome: Outcome,
        elapsed: Duration,
        failed_log: Option<&str>,
    ) {
        if let Some(step) = self.step.take() {
            for bar in step.tail.iter().chain([&step.spinner]) {
                self.remove(bar);
            }
        }
        if let Some(overall) = &self.overall {
            overall.inc(1);
        }
        let item_name = if outcome == Outcome::Ok {
            style(item.to_owned())
        } else {
            style(item.to_owned()).red()
        };
        let mark = look::mark(self.glyphs, outcome);
        let time = look::time_styled(outcome, elapsed);
        match self.group.take() {
            Some(mut running) if grouped && outcome == Outcome::Ok && !last_in_group => {
                running.finished.push((item.to_owned(), elapsed));
                let line = self
                    .add(ProgressBar::new_spinner().with_style(style_of("{wide_msg} {prefix}")));
                line.set_prefix(time);
                line.set_message(format!("  {mark} {item_name}"));
                running.shown.push_back(line);
                // Room for the header, the running step, its tail, the
                // overall bar and a line to spare.
                let (rows, _) = size();
                let room = rows.saturating_sub(TAIL + 4).max(1);
                while running.shown.len() > room {
                    if let Some(old) = running.shown.pop_front() {
                        self.remove(&old);
                    }
                }
                running.header.set_message(header(
                    &running.name,
                    running.finished.len(),
                    running.total,
                ));
                self.group = Some(running);
            }
            Some(running) if grouped => {
                for bar in running.shown.iter().chain([&running.header]) {
                    self.remove(bar);
                }
                let group_time = look::time_styled(outcome, running.started.elapsed());
                let name = style(&running.name).bold();
                if outcome == Outcome::Ok {
                    let count = style(format!("{} ok", running.total)).green();
                    self.println(&aligned(&format!("{mark} {name}  {count}"), &group_time));
                } else {
                    let count = format!("{}/{}", running.finished.len() + 1, running.total);
                    let count = if outcome == Outcome::Failed {
                        style(count).red()
                    } else {
                        style(count).yellow()
                    };
                    self.println(&aligned(&format!("{mark} {name}  {count}"), &group_time));
                    let ok = look::mark(self.glyphs, Outcome::Ok);
                    for (item, elapsed) in &running.finished {
                        let time = look::time_styled(Outcome::Ok, *elapsed);
                        self.println(&aligned(&format!("  {ok} {item}"), &time));
                    }
                    self.println(&aligned(&format!("  {mark} {item_name}"), &time));
                }
            }
            running => {
                self.group = running;
                self.println(&aligned(&format!("{mark} {item_name}"), &time));
            }
        }
        if let Some(log) = failed_log {
            self.multi.suspend(|| {
                eprint!("{log}");
                if !log.ends_with('\n') {
                    eprintln!();
                }
            });
        }
    }

    /// Prints `line` above the bars.
    pub fn println(&self, line: &str) {
        if self.multi.is_hidden() || self.multi.println(line).is_err() {
            eprintln!("{line}");
        }
    }

    /// Clears the bars and prints the skipped steps, given as their group
    /// and full name, with each group's as one line; then the summary and
    /// the error, if any.
    pub fn finish(
        &mut self,
        skipped: &[(Option<String>, String)],
        summary: &str,
        passed: bool,
        error: Option<&str>,
    ) {
        if let Some(running) = self.group.take() {
            for bar in running.shown.iter().chain([&running.header]) {
                self.remove(bar);
            }
        }
        if let Some(overall) = self.overall.take() {
            self.remove(&overall);
        }
        let mark = look::mark(self.glyphs, Outcome::Skipped);
        let mut i = 0;
        while let Some((group, name)) = skipped.get(i) {
            let run = skipped[i..]
                .iter()
                .take_while(|(other, _)| group.is_some() && other == group)
                .count()
                .max(1);
            let text = match group {
                Some(group) if run > 1 => format!("{group}  {run} skipped"),
                _ => name.clone(),
            };
            self.println(&format!("{mark} {}", style(text).dim()));
            i += run;
        }
        let summary = if passed {
            style(summary).green().bold()
        } else {
            style(summary).red().bold()
        };
        self.println(&summary.to_string());
        if let Some(error) = error {
            self.println(&style(format!("error: {error}")).red().to_string());
        }
    }
}
