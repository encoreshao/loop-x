//! `loopx screenshot`: start the project's dev server, wait for it, and clean
//! it up. Only a short allowlist of dev-server commands may be started, since
//! the command comes from an agent handoff.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Dev-server commands the capture may start: the command must equal one of
/// these or continue with a space-separated argument (`-p 3001`).
const SERVE_COMMANDS: &[&str] = &[
    "yarn dev",
    "pnpm dev",
    "npm run dev",
    "next dev",
    "RAILS_ENV=test bin/rails server",
];

/// Environment pairs and argv of an allowed dev-server command.
pub type ServeCommand = (Vec<(String, String)>, Vec<String>);

/// Splits an allowed dev-server command into its environment pairs and argv,
/// or says why it is refused. It runs without a shell, so operators are
/// refused, and only `RAILS_ENV=test` is accepted as an environment prefix.
pub fn check_serve_command(cmd: &str) -> Result<ServeCommand, String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Err("empty command".into());
    }
    crate::verify_commands::refuse_shell_operators(cmd)?;
    let allowed = SERVE_COMMANDS.iter().any(|p| match cmd.strip_prefix(p) {
        Some(tail) => tail.is_empty() || tail.starts_with(' '),
        None => false,
    });
    if !allowed {
        return Err(format!("{cmd:?} is not an allowed dev-server command"));
    }
    let mut env = Vec::new();
    let mut argv = Vec::new();
    for word in cmd.split_whitespace() {
        match word.split_once('=') {
            Some((k, v)) if argv.is_empty() => env.push((k.to_string(), v.to_string())),
            _ => argv.push(word.to_string()),
        }
    }
    Ok((env, argv))
}

/// A running dev server in its own process group. Dropping it stops the whole
/// group, so a `yarn dev` that forked node workers leaves nothing behind.
pub struct DevServer {
    child: Child,
}

impl DevServer {
    /// Spawns `argv` in `cwd` and polls `url` every 250 ms until any HTTP
    /// status answers. The group is already killed when this returns `Err`.
    pub fn start(
        argv: &[String],
        env: &[(String, String)],
        cwd: &Path,
        url: &str,
        timeout: Duration,
    ) -> Result<DevServer, String> {
        let (prog, args) = argv.split_first().ok_or("empty command")?;
        let child = Command::new(prog)
            .args(args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("could not start {prog}: {e}"))?;
        // From here Drop owns cleanup, including on the error returns below.
        let mut server = DevServer { child };
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(5))
            .build();
        let deadline = Instant::now() + timeout;
        loop {
            match server.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!(
                        "dev server exited early ({status}) before {url} answered"
                    ))
                }
                Ok(None) => {}
                Err(e) => return Err(format!("could not poll the dev server: {e}")),
            }
            match agent.get(url).call() {
                Ok(_) | Err(ureq::Error::Status(..)) => return Ok(server),
                Err(ureq::Error::Transport(_)) => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "dev server did not answer {url} within {}s",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for DevServer {
    fn drop(&mut self) {
        let pgid = self.child.id() as i32;
        // SAFETY: signalling the group this struct created; no memory involved.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Leftover group members (workers that ignored SIGTERM) die here even
        // when the leader already exited.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn serve_allowlist_accepts_only_known_dev_servers() {
        for c in [
            "yarn dev",
            "npm run dev -- -p 3001",
            "next dev -p 3001",
            "RAILS_ENV=test bin/rails server -p 3100",
        ] {
            assert!(check_serve_command(c).is_ok(), "{c}");
        }
        for c in [
            "rails server",
            "yarn devx",
            "npm run dev; rm -rf /",
            "node server.js",
            "RAILS_ENV=development bin/rails server",
            "",
        ] {
            assert!(check_serve_command(c).is_err(), "{c}");
        }
        let (env, argv) = check_serve_command("RAILS_ENV=test bin/rails server -p 3100").unwrap();
        assert_eq!(env, vec![("RAILS_ENV".to_string(), "test".to_string())]);
        assert_eq!(argv, ["bin/rails", "server", "-p", "3100"]);
    }

    #[test]
    fn dev_server_waits_for_the_url_then_kills_its_group() {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        let script = format!(
            "import http.server,subprocess,sys\n\
             c=subprocess.Popen(['sleep','60'])\n\
             open({:?},'w').write(str(c.pid))\n\
             http.server.HTTPServer(('127.0.0.1',{port}),http.server.SimpleHTTPRequestHandler).serve_forever()\n",
            pidfile.to_str().unwrap()
        );
        let argv = vec!["python3".to_string(), "-c".to_string(), script];
        let server = DevServer::start(
            &argv,
            &[],
            dir.path(),
            &format!("http://127.0.0.1:{port}/"),
            Duration::from_secs(20),
        )
        .unwrap();
        let parent = server.child.id() as i32;
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().parse().unwrap();
        assert!(alive(parent) && alive(grandchild));
        drop(server);
        let deadline = Instant::now() + Duration::from_secs(5);
        while (alive(parent) || alive(grandchild)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(parent), "server still running");
        assert!(!alive(grandchild), "its child still running");
    }

    #[test]
    fn dev_server_that_exits_or_never_answers_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("http://127.0.0.1:{}/", free_port());
        let started = Instant::now();
        let argv = vec!["sh".to_string(), "-c".into(), "exit 3".into()];
        let err = DevServer::start(&argv, &[], dir.path(), &url, Duration::from_secs(20))
            .err()
            .unwrap();
        assert!(err.contains("exited"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));

        let argv = vec!["sleep".to_string(), "30".into()];
        let err = DevServer::start(&argv, &[], dir.path(), &url, Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(err.contains("did not answer"), "{err}");
    }
}
