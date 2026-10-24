//! `loopx loopkit`: run a LoopKit plugin (mirrors `bin/loopkit.py`'s CLI,
//! which each `bin/loop_plugins/*.py` exposed as `<plugin> <run_id> [--force]`).

use loopx_core::loop_plugins;
use std::process::ExitCode;

const USAGE: &str = "Usage: loopx loopkit <plugin> <run_id> [--force]\n       loopx loopkit list";

pub fn run(args: &[String]) -> ExitCode {
    match args.split_first() {
        Some((cmd, _)) if cmd == "list" => {
            for (module, loop_name) in loop_plugins::PLUGINS {
                println!("{module}\t{loop_name}");
            }
            ExitCode::SUCCESS
        }
        Some((plugin, rest)) => {
            if loop_plugins::module_for_entry(plugin).is_none() {
                eprintln!("loopx loopkit: unknown plugin {plugin:?}\n{USAGE}");
                return ExitCode::from(2);
            }
            match loop_plugins::run_entry(plugin, rest) {
                Some(code) => ExitCode::from(code.clamp(0, 255) as u8),
                None => {
                    eprintln!("loopx loopkit: {plugin} cannot run natively in this build");
                    ExitCode::from(1)
                }
            }
        }
        None => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
