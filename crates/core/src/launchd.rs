//! launchd agent management (mirrors the Python dashboard's `get_daemons_status`,
//! `enable_daemon`, `disable_daemon`, `update_daemon_schedule`): discover the
//! `*.plist` files of a checkout's `launchd/` directory, report which are loaded,
//! load/unload them through `~/Library/LaunchAgents`, and rewrite their schedule.
//!
//! The real `launchctl` is only ever reached through [`Launchctl`], whose command
//! is injectable: tests point it at a fake script, and an empty command means "no
//! launchd at all" (`list` answers from a fixture file, load/unload fail).

use crate::loops_config::{py_repr_str, py_truthy};
use crate::msg::{fail, ok, Msg, Outcome};
use crate::proc;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// ------------------------------------------------------------------ plist I/O

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Open(String),
    Close(String),
    SelfClose(String),
    Text(String),
}

impl<'a> Parser<'a> {
    fn next_tok(&mut self) -> Result<Option<Tok>, String> {
        loop {
            let rest = &self.text[self.pos..];
            if rest.is_empty() {
                return Ok(None);
            }
            if let Some(stripped) = rest.strip_prefix("<?") {
                let end = stripped
                    .find("?>")
                    .ok_or("unclosed processing instruction")?;
                self.pos += 2 + end + 2;
                continue;
            }
            if rest.starts_with("<!--") {
                let end = rest.find("-->").ok_or("unclosed comment")?;
                self.pos += end + 3;
                continue;
            }
            if rest.starts_with("<!") {
                let end = rest.find('>').ok_or("unclosed declaration")?;
                self.pos += end + 1;
                continue;
            }
            if let Some(stripped) = rest.strip_prefix('<') {
                let end = stripped.find('>').ok_or("unclosed tag")?;
                let inner = &stripped[..end];
                self.pos += end + 2;
                if let Some(name) = inner.strip_prefix('/') {
                    return Ok(Some(Tok::Close(name.trim().to_string())));
                }
                let self_close = inner.ends_with('/');
                let inner = inner.trim_end_matches('/');
                let name = inner.split_whitespace().next().unwrap_or("").to_string();
                return Ok(Some(if self_close {
                    Tok::SelfClose(name)
                } else {
                    Tok::Open(name)
                }));
            }
            let end = rest.find('<').unwrap_or(rest.len());
            let text = &rest[..end];
            self.pos += end;
            if text.trim().is_empty() {
                continue;
            }
            return Ok(Some(Tok::Text(xml_unescape(text))));
        }
    }

    fn text_until_close(&mut self, name: &str) -> Result<String, String> {
        let mut text = String::new();
        loop {
            match self.next_tok()? {
                Some(Tok::Text(t)) => text.push_str(&t),
                Some(Tok::Close(n)) if n == name => return Ok(text),
                _ => return Err(format!("malformed <{name}> element")),
            }
        }
    }

    fn value(&mut self, tok: Tok) -> Result<Value, String> {
        match tok {
            Tok::SelfClose(n) => match n.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "string" | "data" | "date" => Ok(Value::String(String::new())),
                "array" => Ok(Value::Array(Vec::new())),
                "dict" => Ok(Value::Object(Map::new())),
                other => Err(format!("unexpected element <{other}/>")),
            },
            Tok::Open(n) => match n.as_str() {
                "string" | "data" | "date" => Ok(Value::String(self.text_until_close(&n)?)),
                "integer" => {
                    let t = self.text_until_close(&n)?;
                    t.trim()
                        .parse::<i64>()
                        .map(|i| json!(i))
                        .map_err(|_| format!("invalid integer {t:?}"))
                }
                "real" => {
                    let t = self.text_until_close(&n)?;
                    t.trim()
                        .parse::<f64>()
                        .map(|f| json!(f))
                        .map_err(|_| format!("invalid real {t:?}"))
                }
                "array" => {
                    let mut items = Vec::new();
                    loop {
                        match self.next_tok()? {
                            Some(Tok::Close(c)) if c == "array" => return Ok(Value::Array(items)),
                            Some(t) => items.push(self.value(t)?),
                            None => return Err("unterminated <array>".into()),
                        }
                    }
                }
                "dict" => {
                    let mut map = Map::new();
                    loop {
                        match self.next_tok()? {
                            Some(Tok::Close(c)) if c == "dict" => return Ok(Value::Object(map)),
                            Some(Tok::Open(k)) if k == "key" => {
                                let key = self.text_until_close("key")?;
                                let t = self.next_tok()?.ok_or("missing value after <key>")?;
                                map.insert(key, self.value(t)?);
                            }
                            _ => return Err("malformed <dict>".into()),
                        }
                    }
                }
                other => Err(format!("unexpected element <{other}>")),
            },
            _ => Err("unexpected content".into()),
        }
    }
}

/// Parse an XML property list into JSON (`date`/`data` become strings). The error
/// text imitates Python's expat message for input that is not XML at all.
pub fn parse_plist(text: &str) -> Result<Value, String> {
    if !text.trim_start().starts_with('<') {
        return Err("syntax error: line 1, column 0".to_string());
    }
    let mut p = Parser { text, pos: 0 };
    match p.next_tok()? {
        Some(Tok::Open(n)) if n == "plist" => {}
        _ => return Err("not a property list".into()),
    }
    let first = p.next_tok()?.ok_or("empty plist")?;
    p.value(first)
}

fn write_value(out: &mut String, v: &Value, depth: usize) {
    let pad = "\t".repeat(depth);
    match v {
        Value::Bool(true) => out.push_str(&format!("{pad}<true/>\n")),
        Value::Bool(false) => out.push_str(&format!("{pad}<false/>\n")),
        Value::Number(n) if n.is_f64() => out.push_str(&format!(
            "{pad}<real>{:?}</real>\n",
            n.as_f64().unwrap_or(0.0)
        )),
        Value::Number(n) => out.push_str(&format!("{pad}<integer>{n}</integer>\n")),
        Value::String(s) => out.push_str(&format!("{pad}<string>{}</string>\n", xml_escape(s))),
        Value::Null => out.push_str(&format!("{pad}<string></string>\n")),
        Value::Array(a) if a.is_empty() => out.push_str(&format!("{pad}<array/>\n")),
        Value::Array(a) => {
            out.push_str(&format!("{pad}<array>\n"));
            for item in a {
                write_value(out, item, depth + 1);
            }
            out.push_str(&format!("{pad}</array>\n"));
        }
        Value::Object(m) if m.is_empty() => out.push_str(&format!("{pad}<dict/>\n")),
        Value::Object(m) => {
            out.push_str(&format!("{pad}<dict>\n"));
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                out.push_str(&format!("{pad}\t<key>{}</key>\n", xml_escape(k)));
                write_value(out, &m[k], depth + 1);
            }
            out.push_str(&format!("{pad}</dict>\n"));
        }
    }
}

/// `plistlib.dumps(value)` (XML, keys sorted, tab indented).
pub fn dump_plist(v: &Value) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n",
    );
    write_value(&mut out, v, 0);
    out.push_str("</plist>\n");
    out
}

/// Replace every `{{KEY}}` of an installer template with its value.
pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

// ------------------------------------------------------------------ launchctl

/// How `launchctl` is reached.
#[derive(Debug, Clone, Default)]
pub struct Launchctl {
    /// The command prefix (`["launchctl"]`, or a fake script). Empty: no launchd.
    pub command: Vec<String>,
    /// With an empty command, what `list` prints is read from this file.
    pub list_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub code: i32,
    pub stderr: String,
}

impl Launchctl {
    pub fn real() -> Self {
        Self {
            command: vec!["launchctl".to_string()],
            list_file: None,
        }
    }

    /// `launchctl <args>` with `timeout`; `Err` when it could not run.
    pub fn run(&self, args: &[&str], timeout: Duration) -> Result<RunResult, String> {
        let Some((program, prefix)) = self.command.split_first() else {
            return Err("launchctl is not available".to_string());
        };
        let mut cmd = Command::new(program);
        cmd.args(prefix).args(args);
        let out = proc::run(&mut cmd, None, Some(timeout)).map_err(|e| e.to_string())?;
        if out.timed_out {
            return Err(format!(
                "Command '['launchctl', '{}']' timed out after {} seconds",
                args.join("', '"),
                timeout.as_secs()
            ));
        }
        Ok(RunResult {
            code: out.code.unwrap_or(-1),
            stderr: out.stderr,
        })
    }

    /// `launchctl list` output ("" on any failure, like Python).
    pub fn list_output(&self) -> String {
        let Some((program, prefix)) = self.command.split_first() else {
            return self
                .list_file
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
        };
        let mut cmd = Command::new(program);
        cmd.args(prefix).arg("list");
        proc::run(&mut cmd, None, Some(Duration::from_secs(5)))
            .map(|o| o.stdout)
            .unwrap_or_default()
    }
}

/// `{label: (pid, status)}` from `launchctl list` output.
pub fn loaded_by_label(output: &str) -> HashMap<String, (String, String)> {
    let mut loaded = HashMap::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 3 {
            continue;
        }
        loaded.insert(
            fields[fields.len() - 1].to_string(),
            (fields[0].to_string(), fields[1].to_string()),
        );
    }
    loaded
}

// ----------------------------------------------------------------- discovery

#[derive(Debug, Clone, PartialEq)]
pub struct Daemon {
    pub file: String,
    pub error: Option<String>,
    pub label: Option<String>,
    pub loaded: bool,
    pub pid: Option<String>,
    pub program_arguments: Vec<Value>,
    pub run_at_load: bool,
    pub keep_alive: bool,
    pub schedule: Option<Value>,
    pub start_interval: Option<Value>,
}

/// Every `*.plist` under `launchd_dir`, sorted by name.
pub fn get_daemons_status(launchd_dir: &Path, launchctl_output: &str) -> Vec<Daemon> {
    let Ok(read) = std::fs::read_dir(launchd_dir) else {
        return Vec::new();
    };
    let loaded = loaded_by_label(launchctl_output);
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "plist"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut d = Daemon {
            file,
            error: None,
            label: None,
            loaded: false,
            pid: None,
            program_arguments: Vec::new(),
            run_at_load: false,
            keep_alive: false,
            schedule: None,
            start_interval: None,
        };
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| parse_plist(&t));
        let data = match parsed {
            Ok(Value::Object(m)) => m,
            Ok(other) => {
                let kind = match other {
                    Value::Array(_) => "list",
                    Value::String(_) => "str",
                    Value::Bool(_) => "bool",
                    _ => "int",
                };
                d.error = Some(format!("plist root is a {kind}, expected a dict"));
                out.push(d);
                continue;
            }
            Err(e) => {
                d.error = Some(e);
                out.push(d);
                continue;
            }
        };
        let label = data.get("Label").cloned();
        if let Some(label) = label.as_ref().and_then(Value::as_str) {
            d.label = Some(label.to_string());
            if let Some((pid, _)) = loaded.get(label) {
                d.loaded = true;
                d.pid = (!pid.is_empty() && pid != "-").then(|| pid.clone());
            }
        }
        d.program_arguments = match data.get("ProgramArguments") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        d.run_at_load = data.get("RunAtLoad").is_some_and(py_truthy);
        d.keep_alive = data.get("KeepAlive").is_some_and(py_truthy);
        d.schedule = data.get("StartCalendarInterval").cloned();
        d.start_interval = data.get("StartInterval").cloned();
        out.push(d);
    }
    out
}

const WEEKDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// `_describe_schedule`.
pub fn describe_schedule(schedule: &Value) -> Option<String> {
    if !py_truthy(schedule) {
        return None;
    }
    let fallback = || Some("scheduled (see plist)".to_string());
    let entries: Vec<&Value> = match schedule {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut hours: Vec<Option<&Value>> = Vec::new();
    let mut minutes: Vec<Option<&Value>> = Vec::new();
    for e in &entries {
        let Some(m) = e.as_object() else {
            return fallback();
        };
        let (h, mi) = (m.get("Hour"), m.get("Minute"));
        if !hours.contains(&h) {
            hours.push(h);
        }
        if !minutes.contains(&mi) {
            minutes.push(mi);
        }
    }
    if hours.len() != 1 || minutes.len() != 1 {
        return fallback();
    }
    let (Some(hour), Some(minute)) = (
        hours[0].and_then(Value::as_i64),
        minutes[0].and_then(Value::as_i64),
    ) else {
        return fallback();
    };
    let mut days: Vec<i64> = entries
        .iter()
        .filter_map(|e| e.get("Day").and_then(Value::as_i64))
        .collect();
    days.sort_unstable();
    days.dedup();
    if let Some(day) = days.first() {
        return Some(format!("Monthly on day {day} {hour:02}:{minute:02}"));
    }
    let mut weekdays: Vec<i64> = entries
        .iter()
        .filter_map(|e| e.get("Weekday").and_then(Value::as_i64))
        .collect();
    weekdays.sort_unstable();
    weekdays.dedup();
    if weekdays.is_empty() {
        return Some(format!("Every day {hour:02}:{minute:02}"));
    }
    if weekdays == [1, 2, 3, 4, 5] {
        return Some(format!("Mon–Fri {hour:02}:{minute:02}"));
    }
    let mut labels = Vec::new();
    for d in weekdays {
        match usize::try_from(d).ok().and_then(|i| WEEKDAY_ABBR.get(i)) {
            Some(l) => labels.push(*l),
            None => return fallback(),
        }
    }
    Some(format!("{} {hour:02}:{minute:02}", labels.join(", ")))
}

/// `_describe_trigger`.
pub fn describe_trigger(d: &Daemon) -> String {
    if let Some(s) = d.schedule.as_ref().and_then(describe_schedule) {
        return s;
    }
    if let Some(interval) = d.start_interval.as_ref().filter(|v| py_truthy(v)) {
        let secs = interval.as_i64().unwrap_or(0);
        return format!("every {} minutes", secs.div_euclid(60));
    }
    if d.run_at_load && d.keep_alive {
        return "always-on (RunAtLoad + KeepAlive)".to_string();
    }
    if d.run_at_load {
        return "runs at load".to_string();
    }
    "manual/on-demand".to_string()
}

// -------------------------------------------------------------------- actions

fn plist_name(filename: &str) -> String {
    Path::new(filename)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn invalid_name(filename: &str) -> Outcome {
    fail(Msg::tra(
        "Invalid plist filename: {name}",
        &[("name", &py_repr_str(filename))],
    ))
}

/// `~/Library/LaunchAgents` of `user_home`.
pub fn launch_agents_dir(user_home: &Path) -> PathBuf {
    user_home.join("Library").join("LaunchAgents")
}

fn stderr_or(result: &RunResult, key: &str) -> Msg {
    let s = result.stderr.trim();
    if s.is_empty() {
        Msg::tra(key, &[("code", &result.code.to_string())])
    } else {
        Msg::raw(s)
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// Copy `launchd_dir/<filename>` to `agents_dir` and `launchctl load -w` it.
pub fn enable_daemon(
    filename: &str,
    launchd_dir: &Path,
    agents_dir: &Path,
    ctl: &Launchctl,
) -> Outcome {
    let safe = plist_name(filename);
    if !safe.ends_with(".plist") {
        return invalid_name(filename);
    }
    let src = launchd_dir.join(&safe);
    if !src.exists() {
        return fail(Msg::tra(
            "{name} not found in {directory}",
            &[
                ("name", &safe),
                ("directory", &launchd_dir.display().to_string()),
            ],
        ));
    }
    let dest = agents_dir.join(&safe);
    let _ = std::fs::create_dir_all(agents_dir);
    if let Err(e) = std::fs::copy(&src, &dest) {
        return fail(Msg::raw(e.to_string()));
    }
    match ctl.run(&["load", "-w", &dest.display().to_string()], TIMEOUT) {
        Err(e) => {
            let _ = std::fs::remove_file(&dest);
            fail(Msg::tra(
                "launchctl load failed to run: {error}",
                &[("error", &e)],
            ))
        }
        Ok(r) if r.code != 0 => {
            let _ = std::fs::remove_file(&dest);
            fail(stderr_or(&r, "launchctl load exited {code}"))
        }
        Ok(_) => ok(Msg::tra("Loaded {name}", &[("name", &safe)])),
    }
}

/// `launchctl unload -w` the installed copy (the file stays).
pub fn disable_daemon(
    filename: &str,
    launchd_dir: &Path,
    agents_dir: &Path,
    ctl: &Launchctl,
) -> Outcome {
    let safe = plist_name(filename);
    if !safe.ends_with(".plist") {
        return invalid_name(filename);
    }
    if !launchd_dir.join(&safe).exists() {
        return fail(Msg::tra(
            "{name} is not a known project daemon",
            &[("name", &safe)],
        ));
    }
    let dest = agents_dir.join(&safe);
    if !dest.exists() {
        return ok(Msg::tra("{name} was not loaded", &[("name", &safe)]));
    }
    match ctl.run(&["unload", "-w", &dest.display().to_string()], TIMEOUT) {
        Err(e) => fail(Msg::tra(
            "launchctl unload failed to run: {error}",
            &[("error", &e)],
        )),
        Ok(r) if r.code != 0 => fail(stderr_or(&r, "launchctl unload exited {code}")),
        Ok(_) => ok(Msg::tra("Unloaded {name}", &[("name", &safe)])),
    }
}

/// `build_calendar_interval`.
pub fn build_calendar_interval(
    hour: i64,
    minute: i64,
    weekdays: &[i64],
    day_of_month: Option<i64>,
) -> Value {
    if let Some(day) = day_of_month.filter(|d| *d != 0) {
        return json!({"Day": day, "Hour": hour, "Minute": minute});
    }
    let mut days: Vec<i64> = weekdays.to_vec();
    days.sort_unstable();
    days.dedup();
    if days.is_empty() || days == [0, 1, 2, 3, 4, 5, 6] {
        return json!({"Hour": hour, "Minute": minute});
    }
    Value::Array(
        days.into_iter()
            .map(|d| json!({"Weekday": d, "Hour": hour, "Minute": minute}))
            .collect(),
    )
}

/// Rewrite the source plist's `StartCalendarInterval`; when the installed copy is
/// currently loaded (per `launchctl list`), reload it and roll back on failure.
#[allow(clippy::too_many_arguments)]
pub fn update_daemon_schedule(
    filename: &str,
    hour: i64,
    minute: i64,
    weekdays: &[i64],
    day_of_month: Option<i64>,
    launchd_dir: &Path,
    agents_dir: &Path,
    ctl: &Launchctl,
) -> Outcome {
    let safe = plist_name(filename);
    if !safe.ends_with(".plist") {
        return invalid_name(filename);
    }
    let src = launchd_dir.join(&safe);
    if !src.exists() {
        return fail(Msg::tra(
            "{name} not found in {directory}",
            &[
                ("name", &safe),
                ("directory", &launchd_dir.display().to_string()),
            ],
        ));
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return fail(Msg::tr("Hour must be 0-23 and minute must be 0-59"));
    }
    if day_of_month.is_some_and(|d| !(1..=31).contains(&d)) {
        return fail(Msg::tr("Day of month must be 1-31"));
    }
    let mut data = match std::fs::read_to_string(&src)
        .map_err(|e| e.to_string())
        .and_then(|t| parse_plist(&t))
    {
        Ok(Value::Object(m)) => m,
        Ok(_) => return fail(Msg::raw("plist root is not a dict")),
        Err(e) => return fail(Msg::raw(e)),
    };
    data.insert(
        "StartCalendarInterval".into(),
        build_calendar_interval(hour, minute, weekdays, day_of_month),
    );
    let rendered = dump_plist(&Value::Object(data.clone()));
    if let Err(e) = std::fs::write(&src, &rendered) {
        return fail(Msg::raw(e.to_string()));
    }

    let dest = agents_dir.join(&safe);
    if !dest.exists() {
        return ok(Msg::tra("Updated schedule for {name}", &[("name", &safe)]));
    }
    let previous = std::fs::read(&dest).unwrap_or_default();
    let _ = std::fs::write(&dest, &rendered);

    let label = data.get("Label").and_then(Value::as_str);
    let is_loaded = label.is_some_and(|l| loaded_by_label(&ctl.list_output()).contains_key(l));
    if !is_loaded {
        return ok(Msg::tra(
            "Updated schedule for {name} (it is currently disabled — the new schedule takes effect once re-enabled)",
            &[("name", &safe)],
        ));
    }
    let dest_s = dest.display().to_string();
    let reload = ctl
        .run(&["unload", "-w", &dest_s], TIMEOUT)
        .and_then(|_| ctl.run(&["load", "-w", &dest_s], TIMEOUT));
    let load_result = match reload {
        Ok(r) => r,
        Err(e) => {
            return fail(Msg::tra(
                "Schedule saved, but reloading launchd failed to run: {error}",
                &[("error", &e)],
            ))
        }
    };
    if load_result.code != 0 {
        let _ = std::fs::write(&dest, previous);
        let reason = if load_result.stderr.trim().is_empty() {
            load_result.code.to_string()
        } else {
            load_result.stderr.trim().to_string()
        };
        let restored = ctl
            .run(&["load", "-w", &dest_s], TIMEOUT)
            .is_ok_and(|r| r.code == 0);
        return fail(if restored {
            Msg::tra(
                "Schedule saved, but launchctl load failed: {reason} — restored and reloaded the previous schedule for {name}",
                &[("reason", &reason), ("name", &safe)],
            )
        } else {
            Msg::tra(
                "Schedule saved, but launchctl load failed: {reason} — {name} is now stopped; re-enable it from the Daemons page",
                &[("reason", &reason), ("name", &safe)],
            )
        });
    }
    ok(Msg::tra(
        "Updated schedule for {name} and reloaded it",
        &[("name", &safe)],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake(dir: &Path, list: &str, fail_load: bool) -> Launchctl {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("calls.log");
        let script = dir.join("fake-launchctl");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\nif [ \"$1\" = list ]; then printf '%s' '{list}'; exit 0; fi\nif [ \"$1\" = load ] && [ {fl} = 1 ]; then echo 'Load failed: 5' >&2; exit 5; fi\nexit 0\n",
                log = log.display(),
                fl = u8::from(fail_load)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Launchctl {
            command: vec![script.display().to_string()],
            list_file: None,
        }
    }

    const PLIST: &str = "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>com.x.a</string>\n\t<key>ProgramArguments</key>\n\t<array><string>/bin/a</string><string>b &amp; c</string></array>\n\t<key>RunAtLoad</key>\n\t<false/>\n\t<key>StartInterval</key>\n\t<integer>900</integer>\n</dict>\n</plist>\n";

    #[test]
    fn plist_roundtrip_sorts_keys_and_escapes() {
        let v = parse_plist(PLIST).unwrap();
        assert_eq!(v["ProgramArguments"][1], "b & c");
        let again = parse_plist(&dump_plist(&v)).unwrap();
        assert_eq!(v, again);
        assert!(dump_plist(&v).contains("<string>b &amp; c</string>"));
        assert_eq!(
            parse_plist("garbage").unwrap_err(),
            "syntax error: line 1, column 0"
        );
    }

    #[test]
    fn schedule_descriptions() {
        let d = |v: Value| describe_schedule(&v);
        assert_eq!(
            d(json!({"Hour": 9, "Minute": 5})).unwrap(),
            "Every day 09:05"
        );
        assert_eq!(
            d(json!({"Day": 3, "Hour": 9, "Minute": 0})).unwrap(),
            "Monthly on day 3 09:00"
        );
        let weekdays = build_calendar_interval(8, 0, &[1, 2, 3, 4, 5], None);
        assert_eq!(describe_schedule(&weekdays).unwrap(), "Mon–Fri 08:00");
        assert_eq!(
            describe_schedule(&build_calendar_interval(8, 0, &[0, 6], None)).unwrap(),
            "Sun, Sat 08:00"
        );
        assert_eq!(
            build_calendar_interval(1, 2, &[0, 1, 2, 3, 4, 5, 6], None),
            json!({"Hour": 1, "Minute": 2})
        );
    }

    #[cfg(unix)]
    #[test]
    fn enable_disable_and_reload_through_the_fake_launchctl() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("launchd");
        let agents = d.path().join("agents");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.plist"), PLIST).unwrap();
        let ctl = fake(d.path(), "123\t0\tcom.x.a\n", false);

        let (okk, _) = enable_daemon("a.plist", &src, &agents, &ctl);
        assert!(okk && agents.join("a.plist").exists());
        let (okk, msg) = enable_daemon("../etc/passwd", &src, &agents, &ctl);
        assert!(!okk, "{msg:?}");
        assert!(!enable_daemon("nope.plist", &src, &agents, &ctl).0);

        let (okk, msg) =
            update_daemon_schedule("a.plist", 7, 30, &[], Some(4), &src, &agents, &ctl);
        assert!(okk, "{msg:?}");
        assert_eq!(
            msg.english(),
            "Updated schedule for a.plist and reloaded it"
        );
        let written = parse_plist(&std::fs::read_to_string(src.join("a.plist")).unwrap()).unwrap();
        assert_eq!(written["StartCalendarInterval"]["Day"], 4);
        let log = std::fs::read_to_string(d.path().join("calls.log")).unwrap();
        assert!(log.contains("unload -w") && log.matches("load -w").count() >= 3);

        let (okk, _) = disable_daemon("a.plist", &src, &agents, &ctl);
        assert!(okk);
        assert!(!update_daemon_schedule("a.plist", 24, 0, &[], None, &src, &agents, &ctl).0);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_load_removes_the_copy_and_a_failed_reload_restores() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("launchd");
        let agents = d.path().join("agents");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.plist"), PLIST).unwrap();
        let ctl = fake(d.path(), "1\t0\tcom.x.a\n", true);
        let (okk, msg) = enable_daemon("a.plist", &src, &agents, &ctl);
        assert!(!okk);
        assert_eq!(msg.english(), "Load failed: 5");
        assert!(!agents.join("a.plist").exists());

        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("a.plist"), PLIST).unwrap();
        let (okk, msg) = update_daemon_schedule("a.plist", 7, 0, &[1], None, &src, &agents, &ctl);
        assert!(!okk);
        assert!(msg.english().contains("now stopped"));
        assert_eq!(
            std::fs::read_to_string(agents.join("a.plist")).unwrap(),
            PLIST
        );
    }

    #[test]
    fn status_reads_loaded_state_and_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.plist"), PLIST).unwrap();
        std::fs::write(d.path().join("b.plist"), "garbage").unwrap();
        let all = get_daemons_status(d.path(), "42\t0\tcom.x.a\n");
        assert_eq!(all[0].pid.as_deref(), Some("42"));
        assert!(all[0].loaded);
        assert_eq!(describe_trigger(&all[0]), "every 15 minutes");
        assert!(all[1].error.is_some());
        assert!(get_daemons_status(&d.path().join("missing"), "").is_empty());
    }
}
