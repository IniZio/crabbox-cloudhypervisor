use std::cell::Cell;
use std::process::{Child, Command};
use std::time::Duration;

use super::terminate;

/// A child that `terminate` may signal; reaped on drop.
struct Sleeper(Child);

impl Sleeper {
    fn spawn() -> Self {
        Self(Command::new("sleep").arg("30").spawn().unwrap())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn terminate_returns_only_once_the_process_is_gone() {
    let child = Sleeper::spawn();
    let checks = Cell::new(0);
    let alive = || {
        checks.set(checks.get() + 1);
        checks.get() <= 5
    };
    terminate(child.0.id(), alive, Duration::ZERO, &mut Vec::new());
    assert!(checks.get() > 5, "returned while still alive");
}
