//! Elapsed time of one step of a lease's start, on a line a reader can grep and sum.

use std::io::Write;
use std::time::Duration;

/// Writes `timing <step> ms=<elapsed>`, then `detail` if it is not empty.
pub(crate) fn step(diag: &mut dyn Write, step: &str, elapsed: Duration, detail: &str) {
    let line = format!("timing {step} ms={} {detail}", elapsed.as_millis());
    let _ = writeln!(diag, "{}", line.trim_end());
}
