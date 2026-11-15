//! How a command prints what it found: text for a person at a terminal,
//! JSON for everyone else, both from one report a test builds directly.

use std::io::{IsTerminal, Write};

use anyhow::Result;

/// What a command prints: text for a person at a terminal, JSON for
/// everyone else. Both describe the same value, so one fixture tests both.
pub trait Report {
    fn text(&self, out: &mut dyn Write) -> Result<()>;
    /// Written rather than returned as a `Value`, so a command printing a
    /// line per entry and one printing a single document both fit.
    fn json(&self, out: &mut dyn Write) -> Result<()>;
}

/// The report on stdout: JSON when asked or when stdout is not a terminal.
pub fn print(report: &impl Report, json: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    let json = json || !out.is_terminal();
    write(report, &mut out, json)
}

/// The half `json` picks, into `out`. `print` is this on a locked stdout,
/// so a test reaches everything but the terminal check.
fn write(report: &impl Report, out: &mut dyn Write, json: bool) -> Result<()> {
    match json {
        true => report.json(out),
        false => report.text(out),
    }
}

#[cfg(test)]
pub mod testing {
    use super::Report;

    /// The report's text as a snapshot takes it: colours stripped, trailing
    /// spaces cut from each line. A padded column with nothing after it
    /// ends in spaces, which an editor or a formatter hook would strip from
    /// the `.snap` file, failing the test for no change in output.
    pub fn text(report: &impl Report) -> String {
        let mut out = Vec::new();
        report.text(&mut out).expect("the text is written");
        let text = plain(&String::from_utf8(out).expect("the text is UTF-8"));
        text.split('\n')
            .map(|line| line.trim_end_matches(' '))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The report's JSON output as written.
    pub fn json(report: &impl Report) -> String {
        let mut out = Vec::new();
        report.json(&mut out).expect("the JSON is written");
        String::from_utf8(out).expect("the JSON is UTF-8")
    }

    /// The text without its colours.
    pub fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|c| *c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use owo_colors::OwoColorize;

    use super::*;

    /// A report whose two halves are told apart by what they write, with a
    /// padded column, a blank line and colours.
    struct Sample;

    impl Report for Sample {
        fn text(&self, out: &mut dyn Write) -> Result<()> {
            writeln!(out, "{}  ", "a".green().bold())?;
            writeln!(out)?;
            writeln!(out, "{:4}", "b".dimmed())?;
            Ok(())
        }

        fn json(&self, out: &mut dyn Write) -> Result<()> {
            writeln!(out, "{{\"a\":1}}  ")?;
            Ok(())
        }
    }

    #[test]
    fn plain_strips_sgr_sequences_and_leaves_the_rest() {
        let text = format!("{} · {}\t[x]\n", "red".red().bold(), "dim".dimmed());

        assert_eq!(testing::plain(&text), "red · dim\t[x]\n");
        assert_eq!(testing::plain("no colours → ☐"), "no colours → ☐");
    }

    #[test]
    fn the_text_cuts_trailing_spaces_and_keeps_blank_lines() {
        assert_eq!(testing::text(&Sample), "a\n\nb\n");
    }

    #[test]
    fn the_json_is_as_written() {
        assert_eq!(testing::json(&Sample), "{\"a\":1}  \n");
    }

    #[test]
    fn write_gives_the_json_half_for_true_and_the_text_half_for_false() {
        let written = |json| {
            let mut out = Vec::new();
            write(&Sample, &mut out, json).expect("written");
            String::from_utf8(out).expect("UTF-8")
        };

        assert_eq!(written(true), "{\"a\":1}  \n");
        let text = written(false);
        assert!(
            text.starts_with(&"a".green().bold().to_string()),
            "{text:?}"
        );
        assert_eq!(testing::plain(&text), "a  \n\nb   \n");
    }
}
