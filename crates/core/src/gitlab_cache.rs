//! The gitlab-config skill's local cache (`~/.gitlab/cache/...`), in the exact
//! on-disk layout and formatting the Python skill uses so both can share it.

use crate::pyjson;
use serde_json::{Map, Value};
use std::path::PathBuf;

pub fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".gitlab").join("cache")
}

/// `project_id.replace('/', '__')`
fn sanitize(project_id: &str) -> String {
    project_id.replace('/', "__")
}

fn project_dir(instance: &str, project_id: &str) -> PathBuf {
    cache_root()
        .join(instance)
        .join("projects")
        .join(sanitize(project_id))
}

pub fn issue_path(instance: &str, project_id: &str, iid: &str) -> PathBuf {
    project_dir(instance, project_id)
        .join("issues")
        .join(format!("{iid}.json"))
}

pub fn project_path(instance: &str, project_id: &str) -> PathBuf {
    project_dir(instance, project_id).join("project.json")
}

/// Missing or unparseable files read as an empty object, like the Python skill.
pub fn read_json(path: &PathBuf) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| Value::Object(Map::new()))
}

pub fn write_json(path: &PathBuf, data: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pyjson::to_string_pretty_sorted(data))
}

/// The cached issue, or `None` when it was never synced.
pub fn get_issue_cache(instance: &str, project_id: &str, iid: &str) -> Option<Value> {
    let path = issue_path(instance, project_id, iid);
    path.exists().then(|| read_json(&path))
}

/// Record `key = value` under the issue's `_notes` annotations.
pub fn annotate_issue(
    instance: &str,
    project_id: &str,
    iid: &str,
    key: &str,
    value: Value,
) -> std::io::Result<()> {
    let path = issue_path(instance, project_id, iid);
    let mut cache = read_json(&path);
    set_under(&mut cache, "_notes", key, value);
    write_json(&path, &cache)
}

/// Record `key = value` under the project's `_memory`.
pub fn annotate_project(
    instance: &str,
    project_id: &str,
    key: &str,
    value: Value,
) -> std::io::Result<()> {
    let path = project_path(instance, project_id);
    let mut cache = read_json(&path);
    set_under(&mut cache, "_memory", key, value);
    write_json(&path, &cache)
}

fn set_under(cache: &mut Value, section: &str, key: &str, value: Value) {
    if !cache.is_object() {
        *cache = Value::Object(Map::new());
    }
    let obj = cache.as_object_mut().expect("just ensured object");
    let slot = obj
        .entry(section.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut()
        .expect("just ensured object")
        .insert(key.to_string(), value);
}

/// Recursively merge `new` onto `base`: new values win on leaf conflicts,
/// nothing in `base` disappears unless `new` replaces it.
pub fn deep_merge(base: &Value, new: &Value) -> Value {
    match (base, new) {
        (Value::Object(b), Value::Object(n)) => {
            let mut merged = b.clone();
            for (k, v) in n {
                let next = match merged.get(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        deep_merge(existing, v)
                    }
                    _ => v.clone(),
                };
                merged.insert(k.clone(), next);
            }
            Value::Object(merged)
        }
        _ => new.clone(),
    }
}

fn truthy(v: &Value) -> bool {
    crate::slack::is_truthy(v)
}

const USER_FIELDS: [&str; 6] = ["id", "username", "name", "public_email", "web_url", "state"];

fn instance_dir(instance: &str) -> PathBuf {
    cache_root().join(instance)
}

/// Merge a batch of GitLab user objects into the instance's user directory.
pub fn upsert_users(instance: &str, users: &[Value]) -> std::io::Result<Value> {
    let path = instance_dir(instance).join("users.json");
    let mut cache = read_json(&path);
    if !cache.is_object() {
        cache = Value::Object(Map::new());
    }
    for user in users {
        let Some(id) = user.get("id").filter(|i| truthy(i)) else {
            continue;
        };
        let uid = match id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let entry: Map<String, Value> = USER_FIELDS
            .iter()
            .filter_map(|k| user.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect();
        let merged = deep_merge(
            cache.get(&uid).unwrap_or(&Value::Object(Map::new())),
            &Value::Object(entry),
        );
        cache.as_object_mut().expect("object").insert(uid, merged);
    }
    write_json(&path, &cache)?;
    Ok(cache)
}

pub fn get_users(instance: &str) -> Value {
    read_json(&instance_dir(instance).join("users.json"))
}

fn group_path(instance: &str, group: &str) -> PathBuf {
    instance_dir(instance)
        .join("groups")
        .join(sanitize(group))
        .join("group.json")
}

pub fn upsert_group(instance: &str, group: &str, data: &Value) -> std::io::Result<Value> {
    let path = group_path(instance, group);
    let merged = deep_merge(&read_json(&path), data);
    write_json(&path, &merged)?;
    Ok(merged)
}

pub fn get_group(instance: &str, group: &str) -> Value {
    read_json(&group_path(instance, group))
}

pub fn upsert_project(instance: &str, project_id: &str, data: &Value) -> std::io::Result<Value> {
    let path = project_path(instance, project_id);
    let merged = deep_merge(&read_json(&path), data);
    write_json(&path, &merged)?;
    Ok(merged)
}

pub fn get_project(instance: &str, project_id: &str) -> Value {
    read_json(&project_path(instance, project_id))
}

fn key_of(note: &Value) -> String {
    match note.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Merge a freshly fetched issue (with `notes`) into the cached copy. Notes
/// merge by id (nothing already cached disappears), keep `created_at` order,
/// and our own `_notes` annotations survive.
pub fn sync_issue(
    instance: &str,
    project_id: &str,
    iid: &str,
    fresh: &Value,
) -> std::io::Result<Value> {
    let path = issue_path(instance, project_id, iid);
    let cache = read_json(&path);

    let mut notes: Vec<(String, Value)> = Vec::new();
    let mut add = |note: &Value| {
        let key = key_of(note);
        match notes.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = note.clone(),
            None => notes.push((key, note.clone())),
        }
    };
    for n in cache
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        add(n);
    }
    for n in fresh
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        add(n);
    }

    let mut merged = deep_merge(&cache, fresh);
    notes.sort_by(|a, b| created_at(&a.1).cmp(created_at(&b.1)));
    let notes: Vec<Value> = notes.into_iter().map(|(_, n)| n).collect();
    merged["notes"] = Value::Array(notes.clone());
    write_json(&path, &merged)?;

    let mut seen: Vec<Value> = vec![
        merged.get("author").cloned().unwrap_or(Value::Null),
        merged.get("assignee").cloned().unwrap_or(Value::Null),
    ];
    seen.extend(
        merged
            .get("assignees")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    );
    seen.extend(
        notes
            .iter()
            .map(|n| n.get("author").cloned().unwrap_or(Value::Null)),
    );
    let seen: Vec<Value> = seen.into_iter().filter(truthy).collect();
    upsert_users(instance, &seen)?;
    Ok(merged)
}

fn created_at(note: &Value) -> &str {
    note.get("created_at").and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_under_creates_and_replaces() {
        let mut v = json!({});
        set_under(&mut v, "_notes", "k", json!("1"));
        set_under(&mut v, "_notes", "k", json!("2"));
        assert_eq!(v, json!({"_notes": {"k": "2"}}));
    }

    #[test]
    fn deep_merge_layers_without_dropping() {
        let base = json!({"a": {"x": 1, "y": 2}, "k": 1});
        let new = json!({"a": {"y": 3, "z": 4}, "n": [1]});
        assert_eq!(
            deep_merge(&base, &new),
            json!({"a": {"x": 1, "y": 3, "z": 4}, "k": 1, "n": [1]})
        );
    }

    #[test]
    fn created_at_orders_notes() {
        let a = json!({"id": 2, "created_at": "2026-01-02"});
        let b = json!({"id": 1, "created_at": "2026-01-01"});
        assert!(created_at(&b) < created_at(&a));
    }

    #[test]
    fn sanitize_replaces_slashes() {
        assert_eq!(sanitize("a/b/c"), "a__b__c");
    }
}
