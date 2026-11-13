//! Reads the files a plan names for one page, under the caps in the spec, and makes the
//! text safe to send: quoted literals under secret-like names are replaced, a file that
//! still looks like it holds a secret is dropped and reported, never sent.

use crate::guide_inventory::{is_schema_like, Inventory};
use crate::project_profile::{contains_secret, Source};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

pub const FILE_CAP: usize = 32 * 1024;
pub const PAGE_CAP: usize = 200 * 1024;

#[derive(Debug, Default)]
pub struct Loaded {
    /// `(path, text)` in the plan's order.
    pub shown: Vec<(String, String)>,
    /// The files whose text is in `shown`, hashed whole.
    pub sources: Vec<Source>,
    /// Named in the prompt as "not shown": over the page total, missing or unreadable.
    pub not_shown: Vec<String>,
    /// Dropped because the text still tripped the secret guard. Names only.
    pub skipped_secret: Vec<String>,
    /// Tables a reduced schema had to leave out.
    pub uncovered_tables: Vec<String>,
}

const SECRET_WORDS: &str = r"(?:key|token|secret|passw(?:or)?d|auth|credential|private)";
const LIT: &str = r#"(?:"(?:[^"\\\r\n]|\\.)*"|'(?:[^'\\\r\n]|\\.)*'|`(?:[^`\\\r\n]|\\.)*`)"#;

fn mask_literal(lit: &str) -> String {
    let q = &lit[..1];
    format!("{q}[redacted]{q}")
}

fn has_secret_word(text: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!("(?i){SECRET_WORDS}")).expect("static regex"))
        .is_match(text)
}

/// Replace a quoted literal assigned to a secret-like name (plain, typed, `:=`, `||=`,
/// `??=`, `=>`, `:`) or given as the default of a call whose first string argument names a
/// secret-like variable; keep names, calls and expressions (`token = jwt.decode(h)` stays whole).
pub fn redact_source(text: &str) -> String {
    static ASSIGN: OnceLock<Regex> = OnceLock::new();
    static DEFAULT: OnceLock<Regex> = OnceLock::new();
    let assign = ASSIGN.get_or_init(|| {
        Regex::new(&format!(
            r#"(?i)((?:[A-Za-z0-9_.\-]*){SECRET_WORDS}(?:[A-Za-z0-9_.\-]*)["']?(?:\s*:\s*[A-Za-z_][\w.\[\]<>, |?]*\s*=\s*|\s*(?:\|\|=|\?\?=|:=|=>|=|:)\s*))({LIT})"#
        ))
        .expect("static regex")
    });
    let default = DEFAULT.get_or_init(|| {
        Regex::new(&format!(
            r#"([A-Za-z_][\w.:]*\(\s*)({LIT})(\s*,\s*)({LIT})"#
        ))
        .expect("static regex")
    });
    let once = assign.replace_all(text, |c: &regex::Captures| {
        format!("{}{}", &c[1], mask_literal(&c[2]))
    });
    default
        .replace_all(&once, |c: &regex::Captures| {
            if has_secret_word(&c[2]) {
                format!("{}{}{}{}", &c[1], &c[2], &c[3], mask_literal(&c[4]))
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_config_extension(path: &str) -> bool {
    let n = base_name(path).to_ascii_lowercase();
    n == ".env"
        || n.starts_with(".env.")
        || n.ends_with(".env")
        || [
            ".yml",
            ".yaml",
            ".properties",
            ".ini",
            ".toml",
            ".conf",
            ".cfg",
        ]
        .iter()
        .any(|e| n.ends_with(e))
}

fn is_config_like(path: &str) -> bool {
    let n = base_name(path);
    matches!(
        n,
        "docker-compose.yml"
            | "docker-compose.yaml"
            | "compose.yml"
            | "compose.yaml"
            | "Cargo.toml"
            | "package.json"
            | "Gemfile"
            | "go.mod"
            | "pyproject.toml"
            | "requirements.txt"
            | "composer.json"
            | "Makefile"
            | "Package.swift"
            | "pom.xml"
            | "build.gradle"
            | "mix.exs"
            | ".gitlab-ci.yml"
            | "Jenkinsfile"
    ) || is_config_extension(path)
        || path.starts_with("config/")
        || path.contains("/config/")
        || path.starts_with(".github/workflows/")
        || path.starts_with(".circleci/")
}

fn table_start(line: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?i)^\s*(?:create_table\s+["']([^"']+)["']|create\s+table\s+(?:if\s+not\s+exists\s+)?["`]?([\w.]+)|model\s+(\w+)\s*\{|enum\s+(\w+)\s*\{)"#)
            .expect("static regex")
    });
    let c = re.captures(line)?;
    (1..=4)
        .find_map(|i| c.get(i))
        .map(|m| m.as_str().to_string())
}

fn dropped_schema_line(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)^\s*(?:--|#|//|/\*|\*|create\s+(?:unique\s+)?index|insert\s+into|t\.index|add_index|set\s+|select\s+|comment\s+on)")
            .expect("static regex")
    });
    line.trim().is_empty() || re.is_match(line)
}

/// Structure lines of a schema file: tables, columns, types, keys and constraints; no
/// comments, index tuning or data. Cut at a table boundary to fit `max` bytes; the
/// tables left out are named.
pub fn reduce_schema(text: &str, max: usize) -> (String, Vec<String>) {
    // Group the kept lines into table blocks; lines before the first table are a header.
    let mut header = String::new();
    let mut blocks: Vec<(String, String)> = Vec::new();
    // Data is never structure: skip a COPY block through `\.` and an INSERT through its `;`.
    // A new table start always ends a skip, so a malformed block cannot swallow later tables.
    #[derive(PartialEq)]
    enum Skip {
        None,
        Copy,
        Insert,
    }
    static COPY_RE: OnceLock<Regex> = OnceLock::new();
    static INSERT_RE: OnceLock<Regex> = OnceLock::new();
    let copy_re = COPY_RE
        .get_or_init(|| Regex::new(r"(?i)^\s*copy\s+\S+.*\bfrom\s+stdin\b").expect("static regex"));
    let insert_re =
        INSERT_RE.get_or_init(|| Regex::new(r"(?i)^\s*insert\s+into\b").expect("static regex"));
    let ends_statement = |l: &str| {
        let code = l.find("--").map_or(l, |i| &l[..i]);
        code.trim_end().ends_with(';')
    };
    let mut skip = Skip::None;
    for line in text.lines() {
        if skip != Skip::None {
            if table_start(line).is_some() {
                skip = Skip::None;
            } else {
                let done = match skip {
                    Skip::Copy => line.trim() == "\\.",
                    _ => ends_statement(line),
                };
                if done {
                    skip = Skip::None;
                }
                continue;
            }
        }
        if copy_re.is_match(line) {
            skip = Skip::Copy;
            continue;
        }
        if insert_re.is_match(line) {
            if !ends_statement(line) {
                skip = Skip::Insert;
            }
            continue;
        }
        if dropped_schema_line(line) {
            continue;
        }
        if let Some(name) = table_start(line) {
            blocks.push((name, String::new()));
        }
        let target = match blocks.last_mut() {
            Some((_, b)) => b,
            None => &mut header,
        };
        target.push_str(line.trim_end());
        target.push('\n');
    }
    let mut out = header;
    let mut left = Vec::new();
    let mut full = false;
    for (name, block) in blocks {
        if !full && out.len() + block.len() <= max {
            out.push_str(&block);
        } else {
            full = true;
            left.push(name);
        }
    }
    if out.len() > max {
        let mut end = max;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    (out, left)
}

fn read_capped(path: &Path, cap: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]).into_owned())
}

fn cut_at_line(mut s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    if let Some(nl) = s.rfind('\n') {
        s.truncate(nl + 1);
    }
    s
}

pub fn load_files(work_dir: &Path, inv: &Inventory, files: &[String], page_cap: usize) -> Loaded {
    let mut out = Loaded::default();
    let mut total = 0usize;
    for rel in files {
        let Some(entry) = inv.get(rel).filter(|e| e.readable) else {
            out.not_shown.push(rel.clone());
            continue;
        };
        let full = work_dir.join(rel);
        let schema = is_schema_like(rel);
        let cap = if schema {
            crate::guide_inventory::MAX_SCHEMA_BYTES as usize
        } else {
            entry.size as usize
        };
        let Some(raw) = read_capped(&full, cap) else {
            out.not_shown.push(rel.clone());
            continue;
        };
        let text = if schema {
            let (reduced, left) = reduce_schema(&raw, FILE_CAP);
            out.uncovered_tables.extend(left);
            redact_source(&reduced)
        } else if is_config_like(rel) {
            redact_source(&cut_at_line(crate::profile_scan::redact(&raw).0, FILE_CAP))
        } else {
            redact_source(&cut_at_line(raw.clone(), FILE_CAP))
        };
        if contains_secret(&text).is_some() {
            out.skipped_secret.push(rel.clone());
            continue;
        }
        if total + text.len() > page_cap {
            out.not_shown.push(rel.clone());
            continue;
        }
        total += text.len();
        let whole = std::fs::read(&full).unwrap_or_default();
        out.sources.push(Source {
            path: rel.clone(),
            sha256: crate::sha256::hex(&crate::sha256::sha256(&whole)),
        });
        out.shown.push((rel.clone(), text));
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::guide_inventory::inventory;
    use std::fs;
    use std::path::Path;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn only_a_quoted_literal_under_a_secret_like_name_is_replaced() {
        let src = "api_key = \"abcd1234efgh5678\"\ntoken = jwt.decode(header)\nAUTH_URL: 'https://x.example/login'\nsecret_key_base: ENV[\"SECRET_KEY_BASE\"]\nname = \"Order\"\n";
        let out = redact_source(src);
        assert!(out.contains("api_key = \"[redacted]\""), "{out}");
        assert!(out.contains("token = jwt.decode(header)"), "{out}");
        assert!(out.contains("AUTH_URL: '[redacted]'"), "{out}");
        assert!(
            out.contains("secret_key_base: ENV[\"SECRET_KEY_BASE\"]"),
            "{out}"
        );
        assert!(out.contains("name = \"Order\""), "{out}");
        assert!(!out.contains("abcd1234efgh5678"));
    }

    #[test]
    fn a_file_that_still_looks_like_it_holds_a_secret_is_dropped_and_named() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "ok.rb", "class Ok; end\n");
        write(
            d.path(),
            "leak.rb",
            "# glpat-AAAAAAAAAAAAAAAAAAAA\nclass Leak; end\n",
        );
        write(
            d.path(),
            "pem.rb",
            "k = <<~X\n-----BEGIN RSA PRIVATE KEY-----\nabc\nX\n",
        );
        let inv = inventory(d.path());
        let l = load_files(
            d.path(),
            &inv,
            &["ok.rb".into(), "leak.rb".into(), "pem.rb".into()],
            PAGE_CAP,
        );
        assert_eq!(l.shown.len(), 1);
        assert_eq!(l.shown[0].0, "ok.rb");
        assert_eq!(l.skipped_secret, ["leak.rb", "pem.rb"]);
        let all: String = l.shown.iter().map(|(_, t)| t.clone()).collect();
        assert!(!all.contains("glpat") && !all.contains("PRIVATE KEY"));
        assert_eq!(l.sources.len(), 1);
        assert_eq!(l.sources[0].path, "ok.rb");
    }

    #[test]
    fn sources_hash_the_whole_file_not_the_capped_text() {
        let d = tempfile::tempdir().unwrap();
        let text = "x = 1\n".repeat(10_000); // 60 KB
        write(d.path(), "big.rb", &text);
        let inv = inventory(d.path());
        let l = load_files(d.path(), &inv, &["big.rb".into()], PAGE_CAP);
        assert!(l.shown[0].1.len() <= FILE_CAP);
        let want = crate::sha256::hex(&crate::sha256::sha256(text.as_bytes()));
        assert_eq!(l.sources[0].sha256, want);
    }

    #[test]
    fn the_page_total_drops_files_from_the_end_and_names_them() {
        let d = tempfile::tempdir().unwrap();
        for n in ["a", "b", "c"] {
            write(d.path(), &format!("{n}.rb"), &"y".repeat(20_000));
        }
        let inv = inventory(d.path());
        let files: Vec<String> = ["a.rb", "b.rb", "c.rb"].map(String::from).to_vec();
        let l = load_files(d.path(), &inv, &files, 50_000);
        assert_eq!(l.shown.len(), 2);
        assert_eq!(l.not_shown, ["c.rb"]);
        assert_eq!(l.sources.len(), 2);
    }

    #[test]
    fn missing_unreadable_and_non_utf8_files_do_not_panic() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("latin.rb"), [b'a', 0xE9, b'b']).unwrap();
        let inv = inventory(d.path());
        let l = load_files(
            d.path(),
            &inv,
            &["latin.rb".into(), "gone.rb".into()],
            PAGE_CAP,
        );
        assert_eq!(l.shown.len(), 1);
        assert!(l.not_shown.contains(&"gone.rb".to_string()));
    }

    #[test]
    fn manifests_keep_the_scan_redaction() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "docker-compose.yml",
            "services:\n  web:\n    environment:\n      DB_PASSWORD: hunter2hunter2\n",
        );
        let inv = inventory(d.path());
        let l = load_files(d.path(), &inv, &["docker-compose.yml".into()], PAGE_CAP);
        assert!(!l.shown[0].1.contains("hunter2"));
    }

    #[test]
    fn a_schema_is_reduced_to_structure() {
        let schema = "# comment\nActiveRecord::Schema.define do\n  create_table \"orders\" do |t|\n    t.integer \"total_cents\"\n    t.index [\"user_id\"]\n  end\n  create_table \"users\" do |t|\n    t.string \"email\"\n  end\n  add_foreign_key \"orders\", \"users\"\nend\n";
        let (out, left) = reduce_schema(schema, 10_000);
        assert!(
            out.contains("create_table \"orders\"") && out.contains("t.integer \"total_cents\"")
        );
        assert!(out.contains("add_foreign_key"));
        assert!(!out.contains("t.index") && !out.contains("# comment"));
        assert!(left.is_empty());
        let sql = "-- c\nCREATE TABLE a (\n  id int\n);\nCREATE INDEX i ON a(id);\nINSERT INTO a VALUES (1);\n";
        let (out, _) = reduce_schema(sql, 10_000);
        assert!(
            out.contains("CREATE TABLE a") && !out.contains("INDEX") && !out.contains("INSERT")
        );
    }

    #[test]
    fn a_schema_over_budget_is_cut_at_a_table_boundary_and_names_what_is_left() {
        let mut s = String::new();
        for i in 0..50 {
            s.push_str(&format!(
                "create_table \"t{i}\" do |t|\n  t.string \"name\"\nend\n"
            ));
        }
        let (out, left) = reduce_schema(&s, 500);
        assert!(out.len() <= 500);
        assert!(out.trim_end().ends_with("end"));
        assert!(!left.is_empty() && left.contains(&"t49".to_string()));
    }

    #[test]
    fn a_schema_file_up_to_two_megabytes_is_read_and_reduced_before_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let mut s = String::new();
        for i in 0..20_000 {
            s.push_str(&format!(
                "create_table \"t{i}\" do |t|\n  t.string \"n\"\nend\n"
            ));
        }
        write(d.path(), "db/schema.rb", &s);
        let inv = inventory(d.path());
        assert!(inv.readable("db/schema.rb"));
        let l = load_files(d.path(), &inv, &["db/schema.rb".into()], PAGE_CAP);
        assert!(l.shown[0].1.len() <= FILE_CAP);
        assert!(!l.uncovered_tables.is_empty());
    }

    #[test]
    fn a_path_not_in_the_inventory_is_never_read() {
        let d = tempfile::tempdir().unwrap();
        let checkout = d.path().join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        write(&checkout, "ok.rb", "class Ok; end\n");
        fs::write(d.path().join("outside.rb"), "class Outside; end\n").unwrap();
        let inv = inventory(&checkout);
        let l = load_files(
            &checkout,
            &inv,
            &["../outside.rb".into(), "ok.rb".into()],
            PAGE_CAP,
        );
        assert_eq!(l.shown.len(), 1);
        assert_eq!(l.shown[0].0, "ok.rb");
        assert_eq!(l.not_shown, ["../outside.rb"]);
        assert!(l.sources.iter().all(|s| s.path != "../outside.rb"));
    }

    #[test]
    fn typed_compound_and_default_argument_secrets_are_replaced() {
        let secret = "Zq8vN2kLm4Pp9Rt1Xy7W";
        let cases = [
            ("const apiKey: string = \"Zq8vN2kLm4Pp9Rt1Xy7W\";", "apiKey"),
            ("api_key: str = \"Zq8vN2kLm4Pp9Rt1Xy7W\"", "api_key"),
            ("token := \"Zq8vN2kLm4Pp9Rt1Xy7W\"", "token"),
            ("password ||= \"Zq8vN2kLm4Pp9Rt1Xy7W\"", "password"),
            ("password ??= \"Zq8vN2kLm4Pp9Rt1Xy7W\"", "password"),
            (
                "os.getenv(\"SECRET_KEY\", \"Zq8vN2kLm4Pp9Rt1Xy7W\")",
                "SECRET_KEY",
            ),
            (
                "ENV.fetch(\"API_KEY\", \"Zq8vN2kLm4Pp9Rt1Xy7W\")",
                "API_KEY",
            ),
        ];
        for (src, name) in cases {
            let out = redact_source(src);
            assert!(!out.contains(secret), "{src} -> {out}");
            assert!(
                out.contains(name) && out.contains("[redacted]"),
                "{src} -> {out}"
            );
        }
        let out = redact_source("os.getenv(\"HOME\", \"/root\")\ntoken = jwt.decode(header)\n");
        assert!(
            out.contains("\"/root\"") && out.contains("jwt.decode(header)"),
            "{out}"
        );
    }

    #[test]
    fn mixed_and_escaped_quotes_do_not_leak_the_tail() {
        let out = redact_source("password = \"it's-Sup3rS3cret\"\n");
        assert!(!out.contains("Sup3rS3cret"), "{out}");
        let out = redact_source("password = \"abc\\\"Sup3rS3cretTail\"\n");
        assert!(!out.contains("Sup3rS3cretTail"), "{out}");
        assert_eq!(out.trim_end(), "password = \"[redacted]\"");
    }

    #[test]
    fn unquoted_values_in_other_config_files_are_redacted() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "config/database.yml",
            "production:\n  password: Tr0ub4dor&3\n",
        );
        write(
            d.path(),
            "src/main/resources/application.properties",
            "spring.datasource.password=hunter2x\n",
        );
        write(
            d.path(),
            "config/secrets.yml",
            "production:\n  secret_key_base: Tr0ub4dor9876xyz\n",
        );
        write(d.path(), "settings.ini", "[db]\npassword = Tr0ub4dor&3\n");
        write(d.path(), "other.toml", "password = \"Tr0ub4dor&3\"\n");
        let inv = inventory(d.path());
        let files: Vec<String> = [
            "config/database.yml",
            "src/main/resources/application.properties",
            "config/secrets.yml",
            "settings.ini",
            "other.toml",
        ]
        .map(String::from)
        .to_vec();
        let l = load_files(d.path(), &inv, &files, PAGE_CAP);
        let all: String = l.shown.iter().map(|(_, t)| t.clone()).collect();
        for leak in ["Tr0ub4dor", "hunter2x"] {
            assert!(!all.contains(leak), "{leak} leaked: {all}");
        }
    }

    #[test]
    fn schema_data_rows_are_dropped_and_later_tables_still_counted() {
        let sql = "CREATE TABLE a (\n  id int\n);\nCOPY a (id, email) FROM stdin;\n1\tbob@example.com\t$2a$10$abcdefghij\n2\tamy@example.com\t$2a$10$klmnopqrst\n\\.\nINSERT INTO a VALUES\n  (3, 'carol@example.com'),\n  (4, 'dave@example.com');\nCREATE TABLE b (\n  id int\n);\n";
        let (out, left) = reduce_schema(sql, 10_000);
        for leak in ["example.com", "$2a$", "INSERT", "COPY"] {
            assert!(!out.contains(leak), "{leak}: {out}");
        }
        assert!(
            out.contains("CREATE TABLE a") && out.contains("CREATE TABLE b"),
            "{out}"
        );
        assert!(left.is_empty());
        let (out, left) = reduce_schema(sql, 30);
        assert!(
            out.contains("CREATE TABLE a") && !out.contains("CREATE TABLE b"),
            "{out}"
        );
        assert_eq!(left, ["b"]);
    }

    #[test]
    fn a_column_named_copy_or_a_commented_insert_does_not_swallow_later_tables() {
        let sql = "CREATE TABLE a (\n  id int,\n  copy text\n);\nCREATE TABLE b (\n  id int\n);\n";
        let (out, left) = reduce_schema(sql, 10_000);
        assert!(
            out.contains("CREATE TABLE b") && out.contains("copy text"),
            "{out}"
        );
        assert!(left.is_empty());
        let sql = "CREATE TABLE a (\n  id int\n);\nINSERT INTO a VALUES (1,'x'); -- seed\nCREATE TABLE b (\n  id int\n);\n";
        let (out, _) = reduce_schema(sql, 10_000);
        assert!(
            out.contains("CREATE TABLE b") && !out.contains("INSERT"),
            "{out}"
        );
    }

    #[test]
    fn an_unterminated_insert_or_copy_never_swallows_a_following_table() {
        for head in [
            "INSERT INTO a VALUES\n  (1, 'x@example.com')",
            "COPY a (id) FROM stdin;\n1\tx@example.com",
        ] {
            let sql = format!("{head}\nCREATE TABLE b (\n  id int\n);\n");
            let (out, _) = reduce_schema(&sql, 10_000);
            assert!(out.contains("CREATE TABLE b"), "{out}");
            assert!(!out.contains("example.com"), "{out}");
        }
    }
}
