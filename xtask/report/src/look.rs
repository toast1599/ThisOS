//! What the output looks like: glyph sets and the text of each kind of line.

use std::fmt::Write;
use std::str::FromStr;
use std::time::Duration;

use anyhow::bail;
use console::style;

use crate::parse::Progress;

/// The characters interactive mode draws with. Machine mode is ASCII only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyphs {
    Ascii,
    Unicode,
}

impl FromStr for Glyphs {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Glyphs> {
        match s {
            "ascii" => Ok(Glyphs::Ascii),
            "unicode" => Ok(Glyphs::Unicode),
            _ => bail!("the glyph set is {s:?}; it takes unicode or ascii"),
        }
    }
}

impl Glyphs {
    /// A guess from the environment, `var` returning a variable's value
    /// when set and not empty: Unicode with a UTF-8 locale, except on the
    /// Linux console, which draws only its own small character set.
    pub fn detect(var: &dyn Fn(&str) -> Option<String>) -> Glyphs {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .find_map(var)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let utf8 = locale.contains("utf-8") || locale.contains("utf8");
        if utf8 && var("TERM").as_deref() != Some("linux") {
            Glyphs::Unicode
        } else {
            Glyphs::Ascii
        }
    }

    /// The spinner's frames, then what it shows once stopped.
    pub fn spinner(self) -> &'static [&'static str] {
        match self {
            Glyphs::Ascii => &["|", "/", "-", "\\", " "],
            Glyphs::Unicode => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "],
        }
    }

    /// indicatif's bar characters: done, the edge, to do.
    pub fn bar(self) -> &'static str {
        match self {
            Glyphs::Ascii => "#>-",
            Glyphs::Unicode => "━╸─",
        }
    }

    /// What separates a running step's name from its phase.
    pub fn arrow(self) -> &'static str {
        match self {
            Glyphs::Ascii => ">",
            Glyphs::Unicode => "›",
        }
    }

    /// What starts each line of a running step's tail.
    pub fn gutter(self) -> &'static str {
        match self {
            Glyphs::Ascii => "|",
            Glyphs::Unicode => "│",
        }
    }

    fn mark(self, outcome: Outcome) -> &'static str {
        match (self, outcome) {
            (Glyphs::Ascii, Outcome::Ok) => "ok  ",
            (Glyphs::Ascii, Outcome::Failed) => "FAIL",
            (Glyphs::Ascii, Outcome::Skipped) => "skip",
            (Glyphs::Ascii, Outcome::Interrupted) => "INTR",
            (Glyphs::Unicode, Outcome::Ok) => "✓",
            (Glyphs::Unicode, Outcome::Failed) => "✗",
            (Glyphs::Unicode, Outcome::Skipped) => "↷",
            (Glyphs::Unicode, Outcome::Interrupted) => "⊘",
        }
    }
}

/// How a step ended, or that it never ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Ok,
    Failed,
    Skipped,
    Interrupted,
}

impl Outcome {
    /// Its word in machine mode.
    fn word(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "FAIL",
            Outcome::Skipped => "skip",
            Outcome::Interrupted => "INTR",
        }
    }
}

/// `0.2s`, `12.0s`, `3m05s`.
pub fn time(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// `[ 3/31]`: a step's number, padded to the width of the total.
pub fn counter(number: usize, total: usize) -> String {
    let digits = total.to_string().len();
    format!("[{number:>digits$}/{total}]")
}

/// A finished or skipped step in machine mode:
/// `[ 3/31] FAIL  clippy x86_64 kernel tests     5.1s`, the name padded to
/// `width`.
pub fn machine_line(
    number: usize,
    total: usize,
    outcome: Outcome,
    name: &str,
    width: usize,
    elapsed: Option<Duration>,
) -> String {
    let line = format!(
        "{} {:<5} {name:<width$}  {}",
        counter(number, total),
        outcome.word(),
        elapsed.map(time).unwrap_or_default()
    );
    line.trim_end().to_owned()
}

/// A step's mark in interactive mode, in its outcome's color.
pub fn mark(glyphs: Glyphs, outcome: Outcome) -> String {
    colored(outcome, glyphs.mark(outcome)).to_string()
}

/// How long a step took, in its outcome's color.
pub fn time_styled(outcome: Outcome, elapsed: Duration) -> String {
    colored(outcome, time(elapsed)).to_string()
}

fn colored<D>(outcome: Outcome, text: D) -> console::StyledObject<D> {
    match outcome {
        Outcome::Ok => style(text).green(),
        Outcome::Failed => style(text).red().bold(),
        Outcome::Skipped => style(text).dim(),
        Outcome::Interrupted => style(text).yellow(),
    }
}

/// A line of a tool's output with the words that say how things went
/// colored: cargo's and rustc's status words, libtest's and the kernel's
/// results.
pub fn highlight(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if let Some((test, result)) = trimmed.rsplit_once(" ... ")
        && test.starts_with("test ")
    {
        return format!("{indent}{test} ... {}", result_word(result));
    }
    if let Some(rest) = trimmed.strip_prefix("test result: ") {
        let (word, rest) = rest.split_once('.').unwrap_or((rest, ""));
        return format!("{indent}test result: {}.{rest}", result_word(word));
    }
    let (word, rest) = match trimmed.split_once(' ') {
        Some((word, rest)) => (word, format!(" {rest}")),
        None => (trimmed, String::new()),
    };
    let word = match word {
        "Compiling" | "Checking" | "Finished" | "Running" | "Fresh" | "Downloading"
        | "Downloaded" | "Updating" | "Locking" | "Adding" | "Documenting" | "Doc-tests"
        | "Blocking" => style(word).green().bold(),
        word if word.starts_with("error") => style(word).red().bold(),
        word if word.starts_with("warning") => style(word).yellow().bold(),
        "ok" | "PASS" => style(word).green(),
        "FAILED" | "FAIL" => style(word).red().bold(),
        _ => style(word),
    };
    format!("{indent}{word}{rest}")
}

/// libtest's word for a result, colored.
fn result_word(word: &str) -> console::StyledObject<&str> {
    match word {
        "ok" => style(word).green(),
        "FAILED" => style(word).red().bold(),
        "ignored" => style(word).yellow(),
        _ => style(word),
    }
}

/// The last line of a run: `ci failed in 6.0s: 2 ok, 1 failed, 28 skipped`.
pub fn summary(title: &str, result: &str, elapsed: Duration, counts: &[(usize, &str)]) -> String {
    let mut line = format!("{title} {result} in {}:", time(elapsed));
    let mut first = true;
    for &(count, what) in counts {
        if count > 0 || what == "ok" {
            let comma = if first { "" } else { "," };
            write!(line, "{comma} {count} {what}").expect("writing to a String");
            first = false;
        }
    }
    line
}

/// What a running step shows after its name.
pub fn progress(progress: Progress) -> String {
    let mib = |bytes: u64| bytes as f64 / f64::from(1u32 << 20);
    match progress {
        Progress::Tests { done, total } => format!("{done}/{total} tests"),
        Progress::Bytes {
            done,
            total: Some(total),
        } => format!(
            "{:>3}% of {:.1} MiB",
            done.saturating_mul(100) / total.max(1),
            mib(total)
        ),
        Progress::Bytes { done, total: None } => format!("{:.1} MiB", mib(done)),
    }
}

/// A step's name as part of its log file's name: `clippy-x86_64`.
pub fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// `text` escaped for a GitHub Actions workflow command, as a property
/// value when `property` is true and as the message otherwise.
pub fn github_escape(text: &str, property: bool) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        match c {
            '%' => escaped.push_str("%25"),
            '\r' => escaped.push_str("%0D"),
            '\n' => escaped.push_str("%0A"),
            ':' if property => escaped.push_str("%3A"),
            ',' if property => escaped.push_str("%2C"),
            c => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{Glyphs, Outcome, highlight, machine_line, slug, summary, time};

    fn detect(vars: &[(&str, &str)]) -> Glyphs {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        Glyphs::detect(&|name| vars.get(name).map(|value| (*value).to_owned()))
    }

    #[test]
    fn detects_glyphs_from_locale_and_terminal() {
        let utf8 = ("LANG", "en_US.UTF-8");
        assert_eq!(detect(&[utf8, ("TERM", "xterm-256color")]), Glyphs::Unicode);
        assert_eq!(detect(&[utf8, ("TERM", "linux")]), Glyphs::Ascii);
        assert_eq!(detect(&[("TERM", "xterm-kitty")]), Glyphs::Ascii);
        // LC_ALL wins over LANG.
        assert_eq!(detect(&[("LC_ALL", "C"), utf8]), Glyphs::Ascii);
    }

    #[test]
    fn machine_lines_line_up() {
        let elapsed = Some(Duration::from_millis(5100));
        assert_eq!(
            machine_line(3, 31, Outcome::Failed, "clippy x86_64", 15, elapsed),
            "[ 3/31] FAIL  clippy x86_64    5.1s"
        );
        assert_eq!(
            machine_line(4, 31, Outcome::Skipped, "clippy aarch64", 15, None),
            "[ 4/31] skip  clippy aarch64"
        );
        assert_eq!(time(Duration::from_secs(185)), "3m05s");
    }

    #[test]
    fn summaries_leave_out_empty_counts_but_ok() {
        let elapsed = Duration::from_secs(6);
        assert_eq!(
            summary(
                "ci",
                "failed",
                elapsed,
                &[
                    (2, "ok"),
                    (1, "failed"),
                    (0, "interrupted"),
                    (28, "skipped")
                ]
            ),
            "ci failed in 6.0s: 2 ok, 1 failed, 28 skipped"
        );
        assert_eq!(
            summary("flash", "failed", elapsed, &[(0, "ok"), (1, "failed")]),
            "flash failed in 6.0s: 0 ok, 1 failed"
        );
    }

    #[test]
    fn highlights_results_and_keeps_the_text() {
        console::set_colors_enabled(true);
        let green = "\x1b[32m";
        let red = "\x1b[31m";
        for (line, color) in [
            ("test tests::a ... ok", green),
            ("test tests::b ... FAILED", red),
            ("test result: FAILED. 1 passed; 1 failed", red),
            ("   Compiling thisos-elf v0.0.0", green),
            ("error[E0308]: mismatched types", red),
            ("  ok thisos_kernel::wait::tests::a", green),
        ] {
            let lit = highlight(line);
            assert!(lit.contains(color), "{line:?} became {lit:?}");
            assert_eq!(console::strip_ansi_codes(&lit), line);
        }
        assert_eq!(highlight("plain words"), "plain words");
    }

    #[test]
    fn slugs_are_file_name_safe() {
        assert_eq!(slug("host tests lib/console"), "host-tests-lib-console");
        assert_eq!(slug("clippy x86_64 (kernel)"), "clippy-x86_64-kernel");
    }
}
