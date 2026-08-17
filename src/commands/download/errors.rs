//! Per-unit download failures that do not abort the whole ferry job.

use std::fmt;

/// One failed package / module / install pass.
#[derive(Debug, Clone)]
pub struct UnitError {
    /// What failed, e.g. `npm pack xlsx@0.20.2` or `go get github.com/x/y@v1.2.3`
    pub unit: String,
    pub reason: String,
}

impl UnitError {
    pub fn new(unit: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            unit: unit.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.unit, self.reason)
    }
}

/// Print a failure summary to stderr. Returns `true` if `errors` is non-empty.
pub fn print_error_summary(ecosystem: &str, errors: &[UnitError]) -> bool {
    if errors.is_empty() {
        return false;
    }
    eprintln!(
        "=== {} download: {} failure(s) (skipped; ferry zip still written if anything succeeded) ===",
        ecosystem,
        errors.len()
    );
    for (i, e) in errors.iter().enumerate() {
        let reason = e.reason.trim();
        let short = if reason.len() > 400 {
            format!("{}…", &reason[..400])
        } else {
            reason.to_string()
        };
        eprintln!("  {}. {}", i + 1, e.unit);
        for line in short.lines() {
            eprintln!("     {line}");
        }
    }
    true
}
