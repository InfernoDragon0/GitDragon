//! Startup timing: phase marks from `main` to the first painted frame.
//!
//! Every mark is logged under the `startup` target. With `GITDRAGON_STARTUP_EXIT=1` the app
//! prints one `STARTUP ...` line to stdout after the first frame and quits, which is what
//! `tools/startup-bench.ps1` measures.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::Window;

const EXIT_ENV: &str = "GITDRAGON_STARTUP_EXIT";

static START: OnceLock<Instant> = OnceLock::new();
static MARKS: Mutex<Vec<(&'static str, Duration)>> = Mutex::new(Vec::new());

/// Call first thing in `main`.
pub fn begin() {
    START.get_or_init(Instant::now);
}

/// Records the time since [`begin`] for `phase`.
pub fn mark(phase: &'static str) {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    tracing::info!(target: "startup", phase, ms = ms(elapsed));
    if let Ok(mut marks) = MARKS.lock() {
        marks.push((phase, elapsed));
    }
}

/// Schedules the `first_frame` mark; call while the main window is being built.
pub fn on_first_frame(window: &Window) {
    window.on_next_frame(|_, cx| {
        mark("first_frame");
        if std::env::var_os(EXIT_ENV).is_some_and(|v| v == "1") {
            println!("{}", summary());
            cx.quit();
        }
    });
}

/// `STARTUP tracing=0.1 platform=90.2 ... first_frame=145.2` (milliseconds since `main`).
fn summary() -> String {
    let marks = MARKS.lock().map(|m| m.clone()).unwrap_or_default();
    let parts: Vec<String> = marks
        .iter()
        .map(|(phase, t)| format!("{phase}={:.1}", ms(*t)))
        .collect();
    format!("STARTUP {}", parts.join(" "))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.
}
