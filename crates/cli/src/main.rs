mod accounting_cmds;
mod approvals_cmd;
mod auth_cmd;
mod chat_tool_cmd;
mod config_cmd;
mod doctor_cmd;
mod events_cmd;
mod git_cmds;
mod gitlab_cmd;
mod inbox_cmd;
mod issues_cmd;
mod loop_cmd;
mod loopkit_cmd;
mod loops_cmd;
mod memory_cmd;
mod messages_cmd;
mod metrics_cmd;
mod notify_cmd;
mod run_cmds;
mod secrets_cmd;
mod serve_cmd;
mod status_cmd;
mod topic_cmd;
mod update_cmd;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// LoopX: the loop-engineering CLI, scheduler and dashboard in one binary.
#[derive(Parser)]
#[command(name = "loopx", version = loopx_core::VERSION, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Loop definitions: init, validate, audit, run, status, inspect, replay, cost, doctor, eval (replaces bin/loop_cli.py)
    Loop {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Performance report from the event log (replaces bin/metrics.py)
    Metrics {
        /// `[--days N] [--project NAME] [--events-dir DIR]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Update check: `check|status|notification`
    Update {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// OAuth sign-ins: `<google|linkedin> start|status|refresh|logout`
    Auth {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Secret store: `migrate [--dry-run] [--yes]`, `list`, `set`, `delete`
    Secrets {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a LoopKit plugin (replaces bin/loop_plugins/*.py's CLI)
    Loopkit {
        /// `<plugin> <run_id> [--force]` or `list`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The loops registry for run-loop-now.sh (replaces bin/loops_config.py's CLI)
    Loops {
        /// `names` or `<entry-point|timeout-seconds|log-suffix|emit-run-events|bash-env> <name>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Which AI CLI the loops use (replaces bin/ai_cli_config.py's CLI)
    AiCli {
        /// `get` or `set <claude|codex>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Keyword risk score for issue text (replaces bin/risk.py)
    Risk {
        /// `score --title T --description D`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// AI cost extraction and report (replaces bin/cost.py)
    Cost {
        /// `extract-result-text|usage-json --cli-output-file F` or `report [--days N] [--events-dir P]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Lesson reuse report (replaces bin/learning.py)
    Learning {
        /// `[--days N] [--project P] [--events-dir P]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// File-based per-issue task memory (replaces bin/memory_store.py)
    MemoryStore {
        /// `add <alias> <iid> <lesson> [tags] [category]`, `get <alias> <iid>` or `list <alias>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Route a loop notification to its connectors (replaces bin/notify.py)
    NotifyLoop {
        /// `<loop_name> <text>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Append to / read the event ledger (replaces bin/events.py)
    Events {
        /// `emit --type T --run-id R [--data JSON ...]` or `list (--date D | --run-id R) [...]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Event ledger maintenance (replaces `loop_cli.py ledger`)
    Ledger {
        /// `backfill [--results-dir DIR] [--events-dir DIR]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Read the per-machine project config (replaces bin/loop_config.py)
    Config {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Post a message to the Slack webhook (replaces bin/slack_notify.py)
    #[command(disable_help_flag = true)]
    Notify {
        /// `[--bundle=<name>] [--blocks=<json>] <message text>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// GitLab REST helpers (replaces the gitlab-config skill's gitlab_api.py / gitlab_cache.py)
    #[command(disable_help_flag = true)]
    Gitlab {
        /// `[--instance=<name>] <command> [args...]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Write run status files (replaces dashboard_server.py write-status / write-topic-status)
    Status {
        /// `write <state> [--loop NAME] ...` or `topic <name> <state> [--current-step TEXT]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run one registered loop now (replaces run-loop-now.sh)
    Run {
        /// `<loop_name> [extra_args...]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// First-launch checks: AI CLIs, credentials, config files, backend
    Doctor {
        /// `[--json]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Serve the dashboard backend and the Mac app's API
    Serve {
        /// `[--port N] [--no-legacy]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start every loop that is due (what launchd runs; replaces loop_scheduler.py)
    Scheduler,
    /// Internal: the supervised process behind `loopx run`
    #[command(name = "_entry", hide = true)]
    Entry {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// See and answer the loop's pending approvals
    Approvals {
        /// `list [--all]`, `show <id>`, `approve <id>`, `deny <id>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Per-project lessons learned (replaces bin/project_memory.py)
    Memory {
        /// `get <instance> <project_id>` or `add <instance> <project_id> <lesson> [issue_iid] [tags]`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The dashboard chat inbox: `read` (unseen user messages, marked seen) or `add <user|loop> <text>`
    Messages {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Offline stand-in for `chat-tool` used only by the chat eval
    #[command(name = "_chat-eval-stub", hide = true)]
    ChatEvalStub {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The command the dashboard chat assistant runs: `<action> [args]` (status, history-*, run-now, topic-save, call <tool> <json>, ...)
    ChatTool {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Per-issue git worktrees
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Merge requests
    Mr {
        #[command(subcommand)]
        command: MrCommand,
    },
    /// Inbox Triage loop (replaces bin/inbox_triage_runner.py)
    Inbox {
        /// `run <run_id>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Topic monitor registry and dedup state
    Topic {
        #[command(subcommand)]
        command: TopicCommand,
    },
    /// GitLab issue helpers
    Issues {
        #[command(subcommand)]
        command: IssuesCommand,
    },
}

#[derive(Subcommand)]
enum WorktreeCommand {
    /// Create or sync the worktree for an issue; prints its path (replaces new_worktree.sh)
    New {
        /// `<repo_path> <default_branch> <issue_iid> <worktree_root>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum MrCommand {
    /// Push a loop/issue-* branch and open a merge request (replaces open_merge_request.sh)
    Open {
        /// `<repo_path> <branch> <target_branch> <title>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TopicCommand {
    /// Run the topic monitor loop now
    Run,
    /// Read topics.json (replaces bin/topic_config.py)
    Config {
        /// `names` or `topic <name>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Items already reported per topic (replaces bin/topic_seen.py)
    Seen {
        /// `get <topic>` or `add <topic> <url> <title>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum IssuesCommand {
    /// List open issues assigned to a user (replaces bin/list_assigned_issues.py)
    List {
        /// `[username] [project_alias...]`; defaults come from projects.json
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// New issue notes since the loop last looked (replaces bin/track_new_comments.py)
    Comments {
        /// `<instance> <project_id> <issue_iid> [exclude_author]` or `mark-seen <instance> <project_id> <issue_iid>`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Loop { args } => loop_cmd::run(&args),
        Command::Metrics { args } => metrics_cmd::run(&args),
        Command::Update { args } => update_cmd::run(&args),
        Command::Auth { args } => auth_cmd::run(&args),
        Command::Secrets { args } => secrets_cmd::run(&args),
        Command::Loopkit { args } => loopkit_cmd::run(&args),
        Command::Loops { args } => loops_cmd::loops(&args),
        Command::AiCli { args } => loops_cmd::ai_cli(&args),
        Command::Risk { args } => accounting_cmds::risk(&args),
        Command::Cost { args } => accounting_cmds::cost(&args),
        Command::Learning { args } => accounting_cmds::learning(&args),
        Command::MemoryStore { args } => accounting_cmds::memory_store(&args),
        Command::NotifyLoop { args } => accounting_cmds::notify_loop(&args),
        Command::Events { args } => events_cmd::run(&args),
        Command::Ledger { args } => events_cmd::ledger(&args),
        Command::Config { args } => config_cmd::run(&args),
        Command::Memory { args } => memory_cmd::run(&args),
        Command::Messages { args } => messages_cmd::run(&args),
        Command::ChatTool { args } => chat_tool_cmd::run(&args),
        Command::ChatEvalStub { args } => chat_tool_cmd::eval_stub(&args),
        Command::Approvals { args } => approvals_cmd::run(&args),
        Command::Run { args } => run_cmds::run(&args),
        Command::Scheduler => run_cmds::scheduler(&[]),
        Command::Doctor { args } => doctor_cmd::run(&args),
        Command::Serve { args } => serve_cmd::run(&args),
        Command::Entry { args } => run_cmds::entry(&args),
        Command::Status { args } => status_cmd::run(&args),
        Command::Gitlab { args } => gitlab_cmd::run(&args),
        Command::Worktree {
            command: WorktreeCommand::New { args },
        } => git_cmds::worktree_new(&args),
        Command::Mr {
            command: MrCommand::Open { args },
        } => git_cmds::mr_open(&args),
        Command::Notify { args } => notify_cmd::run(&args),
        Command::Inbox { args } => match args.split_first() {
            Some((sub, rest)) if sub == "run" => inbox_cmd::run(rest),
            _ => {
                eprintln!("Usage: loopx inbox run <run_id>");
                ExitCode::from(2)
            }
        },
        Command::Topic {
            command: TopicCommand::Run,
        } => run_cmds::topic_run(&[]),
        Command::Topic {
            command: TopicCommand::Config { args },
        } => topic_cmd::config(&args),
        Command::Topic {
            command: TopicCommand::Seen { args },
        } => topic_cmd::seen(&args),
        Command::Issues {
            command: IssuesCommand::List { args },
        } => issues_cmd::list(&args),
        Command::Issues {
            command: IssuesCommand::Comments { args },
        } => issues_cmd::comments(&args),
    }
}
