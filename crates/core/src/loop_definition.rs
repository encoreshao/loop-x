//! `loop.yaml` parsing and validation. Mirrors `bin/loop_definition.py`.
//!
//! YAML crate: `serde_norway`, the maintained fork of the (archived)
//! `serde_yaml`. `serde_yml` is deprecated/unsound and `yaml-rust2` has no serde
//! layer. Python's `yaml.safe_load` produces plain dict/list/scalars, which is
//! exactly what parsing into [`serde_json::Value`] gives, so the validation
//! below is a line-for-line port of `LoopDefinition.from_dict` working on that
//! dynamic value. Known difference: an explicit `null` for a stop-condition or
//! retry field falls back to its default instead of being stored as `None`.

use serde::Serialize;
use serde_json::Value;
use std::path::Path;

pub const DEFAULT_MAX_ITERATIONS: i64 = 3;
pub const DEFAULT_MAX_RUNTIME_MINUTES: f64 = 30.0;
pub const DEFAULT_MAX_COST_USD: f64 = 5.0;
pub const DEFAULT_NO_PROGRESS_ITERATIONS: i64 = 2;
pub const VERIFICATION_MODES: [&str; 2] = ["observe", "gate"];

#[derive(Debug, thiserror::Error)]
pub enum DefinitionError {
    /// Same texts as the Python `ValueError`s.
    #[error("{0}")]
    Invalid(String),
    #[error("cannot read {0}: {1}")]
    Io(String, std::io::Error),
    #[error("{0}: invalid YAML: {1}")]
    Yaml(String, String),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, DefinitionError> {
    Err(DefinitionError::Invalid(msg.into()))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriggerConfig {
    #[serde(rename = "type")]
    pub kind: String,
    pub schedule: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoalConfig {
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ContextConfig {
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerificationConfig {
    pub required: Vec<String>,
    /// `"observe"` or `"gate"`.
    pub mode: String,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            required: Vec::new(),
            mode: "observe".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StopConditions {
    pub max_iterations: i64,
    pub max_runtime_minutes: f64,
    pub max_cost_usd: f64,
    pub no_progress_iterations: i64,
}

impl Default for StopConditions {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_runtime_minutes: DEFAULT_MAX_RUNTIME_MINUTES,
            max_cost_usd: DEFAULT_MAX_COST_USD,
            no_progress_iterations: DEFAULT_NO_PROGRESS_ITERATIONS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetryConfig {
    pub enabled: bool,
    pub max_attempts: i64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 2,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PermissionsConfig {
    pub credentials_read: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LoopDefinition {
    pub name: String,
    pub version: i64,
    pub trigger: TriggerConfig,
    pub goal: GoalConfig,
    pub agent: AgentConfig,
    pub context: ContextConfig,
    pub actions: Vec<String>,
    pub verification: VerificationConfig,
    pub stop_conditions: StopConditions,
    pub human_gates: Vec<String>,
    pub retry: RetryConfig,
    /// Raw verifier specs, resolved by `loop_verifiers::build_verifiers`.
    pub verifiers: Vec<Value>,
    pub permissions: PermissionsConfig,
    pub observability_enabled: bool,
}

fn section<'a>(data: &'a Value, key: &str) -> Result<Option<&'a Value>, DefinitionError> {
    match data.get(key) {
        None => Ok(None),
        Some(v) if v.is_object() => Ok(Some(v)),
        Some(_) => invalid(format!("LoopDefinition: '{key}' must be a mapping")),
    }
}

fn get<'a>(sec: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    sec.and_then(|s| s.get(key)).filter(|v| !v.is_null())
}

fn string_list(
    sec: Option<&Value>,
    key: &str,
    label: &str,
) -> Result<Vec<String>, DefinitionError> {
    match get(sec, key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => Ok(s.clone()),
                _ => invalid(format!(
                    "LoopDefinition: '{label}' must be a list of strings"
                )),
            })
            .collect(),
        Some(_) => invalid(format!(
            "LoopDefinition: '{label}' must be a list of strings"
        )),
    }
}

fn opt_string(
    sec: Option<&Value>,
    key: &str,
    label: &str,
) -> Result<Option<String>, DefinitionError> {
    match get(sec, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => invalid(format!("LoopDefinition: '{label}' must be a string")),
    }
}

fn int_or(
    sec: Option<&Value>,
    key: &str,
    label: &str,
    default: i64,
) -> Result<i64, DefinitionError> {
    match get(sec, key) {
        None => Ok(default),
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .map_or_else(
                || invalid(format!("LoopDefinition: '{label}' must be a number")),
                Ok,
            ),
    }
}

fn float_or(
    sec: Option<&Value>,
    key: &str,
    label: &str,
    default: f64,
) -> Result<f64, DefinitionError> {
    match get(sec, key) {
        None => Ok(default),
        Some(v) => v.as_f64().map_or_else(
            || invalid(format!("LoopDefinition: '{label}' must be a number")),
            Ok,
        ),
    }
}

fn bool_or(
    sec: Option<&Value>,
    key: &str,
    label: &str,
    default: bool,
) -> Result<bool, DefinitionError> {
    match get(sec, key) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => invalid(format!("LoopDefinition: '{label}' must be a boolean")),
    }
}

fn required_str(sec: &Value, key: &str, label: &str) -> Result<String, DefinitionError> {
    match sec.get(key) {
        None => invalid(format!("LoopDefinition: missing required field '{label}'")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => invalid(format!("LoopDefinition: '{label}' must be a string")),
    }
}

impl LoopDefinition {
    /// `LoopDefinition.from_dict`: validate a parsed mapping.
    pub fn from_value(data: &Value) -> Result<Self, DefinitionError> {
        if !data.is_object() {
            return invalid("LoopDefinition: top level must be a mapping");
        }
        for required in ["name", "version", "trigger", "goal"] {
            if data.get(required).is_none() {
                return invalid(format!(
                    "LoopDefinition: missing required field '{required}'"
                ));
            }
        }
        let trigger = section(data, "trigger")?.expect("checked present");
        if trigger.get("type").is_none() {
            return invalid("LoopDefinition: missing required field 'trigger.type'");
        }
        let goal = section(data, "goal")?.expect("checked present");
        if goal.get("type").is_none() {
            return invalid("LoopDefinition: missing required field 'goal.type'");
        }
        let agent = section(data, "agent")?;
        let context = section(data, "context")?;
        let verification = section(data, "verification")?;
        let stop = section(data, "stop_conditions")?;
        let retry = section(data, "retry")?;
        let permissions = section(data, "permissions")?;

        let mode = match get(verification, "mode") {
            None => "observe".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        if !VERIFICATION_MODES.contains(&mode.as_str()) {
            return invalid(format!(
                "LoopDefinition: verification.mode must be one of ['gate', 'observe'], got '{mode}'"
            ));
        }

        let version = match data.get("version") {
            Some(v) => v.as_i64().map_or_else(
                || invalid("LoopDefinition: 'version' must be an integer"),
                Ok,
            )?,
            None => unreachable!("checked present"),
        };
        let stop_defaults = StopConditions::default();
        let retry_defaults = RetryConfig::default();
        Ok(LoopDefinition {
            name: required_str(data, "name", "name")?,
            version,
            trigger: TriggerConfig {
                kind: required_str(trigger, "type", "trigger.type")?,
                schedule: opt_string(Some(trigger), "schedule", "trigger.schedule")?,
            },
            goal: GoalConfig {
                kind: required_str(goal, "type", "goal.type")?,
            },
            agent: AgentConfig {
                provider: opt_string(agent, "provider", "agent.provider")?,
                model: opt_string(agent, "model", "agent.model")?,
            },
            context: ContextConfig {
                sources: string_list(context, "sources", "context.sources")?,
            },
            actions: string_list(Some(data), "actions", "actions")?,
            verification: VerificationConfig {
                required: string_list(verification, "required", "verification.required")?,
                mode,
            },
            stop_conditions: StopConditions {
                max_iterations: int_or(
                    stop,
                    "max_iterations",
                    "stop_conditions.max_iterations",
                    stop_defaults.max_iterations,
                )?,
                max_runtime_minutes: float_or(
                    stop,
                    "max_runtime_minutes",
                    "stop_conditions.max_runtime_minutes",
                    stop_defaults.max_runtime_minutes,
                )?,
                max_cost_usd: float_or(
                    stop,
                    "max_cost_usd",
                    "stop_conditions.max_cost_usd",
                    stop_defaults.max_cost_usd,
                )?,
                no_progress_iterations: int_or(
                    stop,
                    "no_progress_iterations",
                    "stop_conditions.no_progress_iterations",
                    stop_defaults.no_progress_iterations,
                )?,
            },
            human_gates: string_list(Some(data), "human_gates", "human_gates")?,
            retry: RetryConfig {
                enabled: bool_or(retry, "enabled", "retry.enabled", retry_defaults.enabled)?,
                max_attempts: int_or(
                    retry,
                    "max_attempts",
                    "retry.max_attempts",
                    retry_defaults.max_attempts,
                )?,
            },
            verifiers: match data.get("verifiers") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items.clone(),
                Some(_) => return invalid("LoopDefinition: 'verifiers' must be a list"),
            },
            permissions: PermissionsConfig {
                credentials_read: bool_or(
                    permissions,
                    "credentials_read",
                    "permissions.credentials_read",
                    false,
                )?,
            },
            observability_enabled: bool_or(
                Some(data),
                "observability_enabled",
                "observability_enabled",
                true,
            )?,
        })
    }

    /// Parse YAML text; `origin` names the source in error messages.
    pub fn from_yaml_str(text: &str, origin: &str) -> Result<Self, DefinitionError> {
        let value: Value = serde_norway::from_str(text)
            .map_err(|e| DefinitionError::Yaml(origin.to_string(), e.to_string()))?;
        Self::from_value(&value)
    }

    /// `LoopDefinition.from_yaml(path)`.
    pub fn from_yaml(path: &Path) -> Result<Self, DefinitionError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DefinitionError::Io(path.display().to_string(), e))?;
        Self::from_yaml_str(&text, &path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example() -> Value {
        json!({
            "name": "gitlab-issue-fixer",
            "version": 1,
            "trigger": {"type": "schedule", "schedule": "0 10 * * 1-5"},
            "goal": {"type": "issue_resolution"},
            "agent": {"provider": "claude", "model": "default"},
            "context": {"sources": ["issue", "repository", "project_memory", "task_memory"]},
            "actions": ["inspect_issue", "inspect_repository", "modify_code", "run_tests", "create_merge_request"],
            "verification": {"required": ["tests", "lint", "diff_scope"]},
            "stop_conditions": {"max_iterations": 3, "max_runtime_minutes": 30, "max_cost_usd": 5, "no_progress_iterations": 2},
            "human_gates": ["merge", "production_deploy"],
            "retry": {"enabled": true, "max_attempts": 2},
        })
    }

    fn minimal() -> Value {
        json!({"name": "minimal-loop", "version": 1, "trigger": {"type": "manual"}, "goal": {"type": "issue_resolution"}})
    }

    fn err(v: &Value) -> String {
        LoopDefinition::from_value(v).unwrap_err().to_string()
    }

    #[test]
    fn round_trips_every_field() {
        let d = LoopDefinition::from_value(&example()).unwrap();
        assert_eq!(d.name, "gitlab-issue-fixer");
        assert_eq!(d.version, 1);
        assert_eq!(d.trigger.kind, "schedule");
        assert_eq!(d.trigger.schedule.as_deref(), Some("0 10 * * 1-5"));
        assert_eq!(d.goal.kind, "issue_resolution");
        assert_eq!(d.agent.provider.as_deref(), Some("claude"));
        assert_eq!(d.agent.model.as_deref(), Some("default"));
        assert_eq!(d.context.sources.len(), 4);
        assert_eq!(d.actions[4], "create_merge_request");
        assert_eq!(d.verification.required, ["tests", "lint", "diff_scope"]);
        assert_eq!(d.stop_conditions.max_iterations, 3);
        assert_eq!(d.stop_conditions.max_runtime_minutes, 30.0);
        assert_eq!(d.stop_conditions.max_cost_usd, 5.0);
        assert_eq!(d.stop_conditions.no_progress_iterations, 2);
        assert_eq!(d.human_gates, ["merge", "production_deploy"]);
        assert!(d.retry.enabled);
        assert_eq!(d.retry.max_attempts, 2);
    }

    #[test]
    fn missing_top_level_required_fields_raise() {
        for key in ["name", "version", "trigger", "goal"] {
            let mut v = example();
            v.as_object_mut().unwrap().remove(key);
            assert!(err(&v).contains(key), "{key}");
        }
    }

    #[test]
    fn missing_trigger_type_and_goal_type_raise() {
        let mut v = example();
        v["trigger"] = json!({"schedule": "0 10 * * 1-5"});
        assert!(err(&v).contains("trigger.type"));
        let mut v = example();
        v["goal"] = json!({});
        assert!(err(&v).contains("goal.type"));
    }

    #[test]
    fn defaults_apply_when_optional_sections_omitted() {
        let d = LoopDefinition::from_value(&minimal()).unwrap();
        assert_eq!(d.stop_conditions, StopConditions::default());
        assert_eq!(d.stop_conditions.max_runtime_minutes, 30.0);
        assert!(d.retry.enabled && d.retry.max_attempts == 2);
        assert!(d.human_gates.is_empty() && d.actions.is_empty());
        assert!(d.context.sources.is_empty() && d.verification.required.is_empty());
        assert!(d.verifiers.is_empty());
        assert_eq!(d.verification.mode, "observe");
        assert!(!d.permissions.credentials_read);
        assert!(d.observability_enabled);
        assert_eq!(d.agent, AgentConfig::default());
    }

    #[test]
    fn verifiers_round_trip_raw_specs() {
        let mut v = example();
        v["verifiers"] = json!([
            {"name": "tests", "type": "command", "command": "pytest"},
            {"name": "diff", "type": "git_diff", "allowed_paths": ["src/"]},
        ]);
        let d = LoopDefinition::from_value(&v).unwrap();
        assert_eq!(d.verifiers, v["verifiers"].as_array().unwrap().clone());
    }

    #[test]
    fn permissions_and_observability_round_trip() {
        let mut v = example();
        v["permissions"] = json!({"credentials_read": true});
        v["observability_enabled"] = json!(false);
        let d = LoopDefinition::from_value(&v).unwrap();
        assert!(d.permissions.credentials_read);
        assert!(!d.observability_enabled);
    }

    #[test]
    fn verification_mode_is_validated() {
        let mut v = example();
        v["verification"] = json!({"required": [], "mode": "gate"});
        assert_eq!(
            LoopDefinition::from_value(&v).unwrap().verification.mode,
            "gate"
        );
        v["verification"] = json!({"mode": "strict"});
        assert_eq!(
            err(&v),
            "LoopDefinition: verification.mode must be one of ['gate', 'observe'], got 'strict'"
        );
    }

    #[test]
    fn fractional_cost_and_partial_stop_conditions() {
        let mut v = minimal();
        v["stop_conditions"] = json!({"max_cost_usd": 0.05, "max_iterations": 7});
        let d = LoopDefinition::from_value(&v).unwrap();
        assert_eq!(d.stop_conditions.max_cost_usd, 0.05);
        assert_eq!(d.stop_conditions.max_iterations, 7);
        assert_eq!(d.stop_conditions.max_runtime_minutes, 30.0);
    }

    #[test]
    fn wrong_types_are_rejected_not_panicked() {
        let mut v = minimal();
        v["actions"] = json!("modify_code");
        assert!(err(&v).contains("actions"));
        assert!(LoopDefinition::from_value(&json!([1])).is_err());
        assert!(LoopDefinition::from_value(&json!(null)).is_err());
    }

    #[test]
    fn parses_yaml_text_and_file() {
        let yaml = "name: x\nversion: 1\ntrigger:\n  type: manual\ngoal:\n  type: g\nretry:\n  enabled: false\n";
        let d = LoopDefinition::from_yaml_str(yaml, "inline").unwrap();
        assert!(!d.retry.enabled);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("loop.yaml");
        std::fs::write(&p, yaml).unwrap();
        assert_eq!(LoopDefinition::from_yaml(&p).unwrap(), d);
        assert!(LoopDefinition::from_yaml(&dir.path().join("nope.yaml")).is_err());
        assert!(LoopDefinition::from_yaml_str("a: [", "inline").is_err());
        assert!(LoopDefinition::from_yaml_str("", "inline").is_err());
    }

    /// Every loop.yaml this checkout ships must load.
    #[test]
    fn every_shipped_loop_yaml_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../loops");
        let mut seen = 0;
        for entry in std::fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path().join("loop.yaml");
            if path.exists() {
                let d = LoopDefinition::from_yaml(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(!d.name.is_empty());
                seen += 1;
            }
        }
        assert!(seen >= 10, "expected the shipped loops, found {seen}");
    }

    #[test]
    fn templates_loop_yamls_parse_too() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates");
        if let Ok(dirs) = std::fs::read_dir(&root) {
            for entry in dirs.flatten() {
                let path = entry.path().join("loop.yaml");
                if path.exists() {
                    LoopDefinition::from_yaml(&path)
                        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                }
            }
        }
    }
}
