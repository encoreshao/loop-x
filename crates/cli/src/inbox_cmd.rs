//! `loopx inbox run <run_id>`: the Inbox Triage loop's entry point (replaces
//! `bin/inbox_triage_runner.py <run_id>`). `loopx run inbox-triage-loop` reaches
//! the same code through `loopx _entry`.

use std::process::ExitCode;

pub fn run(args: &[String]) -> ExitCode {
    let code = loopx_core::inbox_runner::main(&loopx_core::inbox_runner::default_ctx(), args, None);
    ExitCode::from(code.clamp(0, 255) as u8)
}
