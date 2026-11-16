//! `loopx chat-tool <action> [args]`: see `loopx_server::chat_tool`.

use loopx_core::runner_util::repo_root;
use loopx_server::chat_tool;
use loopx_server::ui::state::UiState;
use std::process::ExitCode;

pub fn run(args: &[String]) -> ExitCode {
    let Some((action, rest)) = args.split_first() else {
        eprintln!("Usage: loopx chat-tool <action> [args]");
        return ExitCode::from(1);
    };
    let state = state();
    let session = std::env::var("LOOP_CHAT_SESSION").unwrap_or_default();
    match chat_tool::run(&state, action, rest, &session) {
        Ok(result) => {
            println!("{}", chat_tool::render(&result));
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

/// The state `loopx chat-tool` runs with: rooted at [`repo_root`]. The Slack
/// listener builds its chat runtime from this too, so both see one root.
pub(crate) fn state() -> UiState {
    UiState::from_env(&repo_root())
}

/// `loopx _chat-eval-stub`: the chat eval's offline stand-in (see `chat_eval::stub_main`).
pub fn eval_stub(args: &[String]) -> ExitCode {
    match loopx_core::chat_eval::stub_main(args) {
        Ok(result) => {
            println!("{}", chat_tool::render(&result));
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}
