//! Report lines and the summary.

use super::{Check, Level};

/// One report line: `<tag> <name>: <detail>`.
pub(super) fn line(c: &Check) -> String {
    let tag = match c.level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "FAIL",
    };
    format!("{tag} {}: {}", c.name, c.detail)
}

/// Formats the checks plus the summary line.
pub(super) fn render(checks: &[Check]) -> String {
    let count = |level| checks.iter().filter(|c| c.level == level).count();
    let mut lines: Vec<String> = checks.iter().map(line).collect();
    lines.push(format!(
        "doctor: {} ok, {} warn, {} fail",
        count(Level::Ok),
        count(Level::Warn),
        count(Level::Fail)
    ));
    lines.join("\n")
}
