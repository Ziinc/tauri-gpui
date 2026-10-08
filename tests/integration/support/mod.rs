//! Shared helpers for the integration suite.

mod ctx;
mod probe;
mod runner;

pub use ctx::{Ctx, Fail, TestResult, WAIT, xdotool};
pub use probe::{Probe, attach_probe, probe};
pub use runner::{Counter, INIT_ERRORS, QUIT_CODES, TauriHandle, Test, main};
