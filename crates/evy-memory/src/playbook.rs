//! Playbook store — markdown files with YAML frontmatter.
//!
//! Playbooks are operator-editable, version-controlled procedures Evy
//! consults at decision time. The store snapshots a directory of `.md`
//! files at construction time; hot reload is deferred until the
//! feedback-ingest layer needs it.
//!
//! ADR 0020 §"Layer 4 — Playbooks".
//!
//! Frontmatter shape:
//!
//! ```yaml
//! ---
//! name: brownfield-first-day
//! description: Checklist for the first day of any brownfield project
//! triggers: ["new brownfield project", "inheriting a codebase"]
//! last_reviewed: 2026-05-26
//! ---
//! ```
//!
//! Any keys beyond `triggers` are flattened into [`Playbook::frontmatter`]
//! as `String → String` for caller inspection.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use evy_core::Result;
use gray_matter::engine::YAML;
use gray_matter::{Matter, ParsedEntityStruct};
use serde::{Deserialize, Serialize};

use crate::error;

// TODO: Phase 3 — hot reload via `notify`. The current snapshot model is
// fine while playbooks are operator-authored; auto-distillation (also
// Phase 3) will demand re-loading without a daemon restart.

/// Subset of frontmatter parsed via serde for typed access.
#[derive(Debug, Deserialize, Default)]
struct FrontmatterShape {
    #[serde(default)]
    triggers: Vec<String>,
}

/// A single playbook loaded from disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playbook {
    /// Filename without the `.md` extension.
    pub name: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Flattened frontmatter (string-keyed). `triggers` is parsed
    /// separately into [`Playbook::triggers`].
    pub frontmatter: HashMap<String, String>,
    /// Body of the markdown file with the frontmatter block stripped.
    pub body: String,
    /// Substring patterns that, when found inside a situation string,
    /// indicate this playbook is applicable.
    pub triggers: Vec<String>,
}

/// Snapshot of all playbooks under a directory.
#[derive(Debug, Clone)]
pub struct PlaybookStore {
    playbooks: Vec<Playbook>,
}

impl PlaybookStore {
    /// Read every `*.md` file directly under `dir` and parse its
    /// frontmatter + body. Files without frontmatter are still loaded —
    /// their `triggers` list is empty.
    ///
    /// Subdirectories are NOT recursed; flat layout keeps the operator's
    /// mental model simple.
    ///
    /// # Errors
    /// Returns [`evy_core::Error::Io`] if the directory cannot be read,
    /// [`evy_core::Error::InvalidMandate`] if a frontmatter block fails
    /// to parse as YAML.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut playbooks = Vec::new();
        let entries = fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| error::bad_playbook(&path, "non-utf8 filename"))?
                .to_owned();
            let raw = fs::read_to_string(&path)?;
            let playbook = parse_one(name, path, &raw)?;
            playbooks.push(playbook);
        }
        // Stable ordering — operators expect alphabetical listings.
        playbooks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { playbooks })
    }

    /// All loaded playbooks, alphabetised by name.
    #[must_use]
    pub fn list(&self) -> &[Playbook] {
        &self.playbooks
    }

    /// Find a playbook by exact name (filename without `.md`).
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Playbook> {
        self.playbooks.iter().find(|p| p.name == name)
    }

    /// Every playbook whose `triggers` list contains a substring that
    /// appears inside `situation`. Case-insensitive comparison.
    #[must_use]
    pub fn matching_trigger(&self, situation: &str) -> Vec<&Playbook> {
        let haystack = situation.to_lowercase();
        self.playbooks
            .iter()
            .filter(|p| {
                p.triggers
                    .iter()
                    .any(|t| haystack.contains(&t.to_lowercase()))
            })
            .collect()
    }

    /// Number of playbooks loaded. Cheap (`Vec::len`).
    #[must_use]
    pub fn count(&self) -> usize {
        self.playbooks.len()
    }
}

fn parse_one(name: String, path: PathBuf, raw: &str) -> Result<Playbook> {
    let matter = Matter::<YAML>::new();
    // `parse_with_struct` returns `Option`; `None` means "no frontmatter
    // block at all", which we tolerate (empty triggers, empty
    // frontmatter map).
    let (frontmatter_map, triggers, body): (HashMap<String, String>, Vec<String>, String) =
        match matter.parse_with_struct::<FrontmatterShape>(raw) {
            Some(ParsedEntityStruct {
                data,
                content,
                orig: _,
                matter: matter_text,
                excerpt: _,
            }) => {
                let mut map = HashMap::new();
                if !matter_text.is_empty() {
                    flatten_yaml_into(&matter_text, &mut map)
                        .map_err(|e| error::bad_playbook(&path, e))?;
                }
                (map, data.triggers, content)
            }
            None => (HashMap::new(), Vec::new(), raw.to_owned()),
        };
    Ok(Playbook {
        name,
        path,
        frontmatter: frontmatter_map,
        body,
        triggers,
    })
}

/// Flatten a single-level YAML mapping into `String → String`. Lists and
/// nested maps are stringified via their YAML representation so callers
/// still see something — but the rich `triggers` list is read separately
/// via the typed `FrontmatterShape`.
fn flatten_yaml_into(
    yaml_text: &str,
    out: &mut HashMap<String, String>,
) -> std::result::Result<(), String> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml_text).map_err(|e| format!("yaml parse failed: {e}"))?;
    let serde_yaml::Value::Mapping(map) = value else {
        return Err("frontmatter must be a YAML mapping".to_owned());
    };
    for (k, v) in map {
        let key = match k {
            serde_yaml::Value::String(s) => s,
            other => format!("{other:?}"),
        };
        let value_str = match v {
            serde_yaml::Value::String(s) => s,
            serde_yaml::Value::Number(n) => n.to_string(),
            serde_yaml::Value::Bool(b) => b.to_string(),
            other => serde_yaml::to_string(&other)
                .unwrap_or_default()
                .trim()
                .to_owned(),
        };
        out.insert(key, value_str);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_md(dir: &Path, name: &str, body: &str) {
        let path = dir.join(format!("{name}.md"));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    const BROWNFIELD: &str = r#"---
name: brownfield-first-day
description: Checklist for the first day of any brownfield project
triggers: ["new brownfield project", "inheriting a codebase"]
last_reviewed: 2026-05-26
---

# Brownfield First Day Checklist

1. Read the README.
2. Run the tests.
"#;

    const RATE_LIMIT: &str = r#"---
name: drain-rate-limited-worker
triggers: ["rate limit hit", "429"]
---

# Drain and re-route
"#;

    const NO_FRONTMATTER: &str = "# Unstructured note\n\nNothing here yet.\n";

    #[test]
    fn loads_three_playbooks_alphabetised() {
        let dir = tempdir().unwrap();
        write_md(dir.path(), "brownfield-first-day", BROWNFIELD);
        write_md(dir.path(), "drain-rate-limited-worker", RATE_LIMIT);
        write_md(dir.path(), "zz-no-frontmatter", NO_FRONTMATTER);
        let store = PlaybookStore::load(dir.path()).unwrap();
        assert_eq!(store.count(), 3);
        assert_eq!(store.list()[0].name, "brownfield-first-day");
        assert_eq!(store.list()[2].name, "zz-no-frontmatter");
    }

    #[test]
    fn frontmatter_extracted_and_body_isolated() {
        let dir = tempdir().unwrap();
        write_md(dir.path(), "brownfield-first-day", BROWNFIELD);
        let store = PlaybookStore::load(dir.path()).unwrap();
        let pb = store.find("brownfield-first-day").expect("found");
        assert_eq!(
            pb.frontmatter.get("name").map(String::as_str),
            Some("brownfield-first-day")
        );
        assert!(pb.frontmatter.contains_key("last_reviewed"));
        assert_eq!(pb.triggers.len(), 2);
        assert!(pb.body.contains("Brownfield First Day Checklist"));
        assert!(!pb.body.contains("---"), "frontmatter must be stripped");
    }

    #[test]
    fn no_frontmatter_loads_with_empty_triggers() {
        let dir = tempdir().unwrap();
        write_md(dir.path(), "scratch", NO_FRONTMATTER);
        let store = PlaybookStore::load(dir.path()).unwrap();
        let pb = store.find("scratch").unwrap();
        assert!(pb.triggers.is_empty());
        assert!(pb.frontmatter.is_empty());
        assert!(pb.body.contains("Unstructured note"));
    }

    #[test]
    fn matching_trigger_finds_relevant_playbook() {
        let dir = tempdir().unwrap();
        write_md(dir.path(), "brownfield-first-day", BROWNFIELD);
        write_md(dir.path(), "drain-rate-limited-worker", RATE_LIMIT);
        let store = PlaybookStore::load(dir.path()).unwrap();
        let hits =
            store.matching_trigger("operator says: I'm inheriting a codebase from a contractor");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "brownfield-first-day");

        let rate = store.matching_trigger("Got a 429 again");
        assert_eq!(rate.len(), 1);
        assert_eq!(rate[0].name, "drain-rate-limited-worker");
    }

    #[test]
    fn matching_trigger_is_case_insensitive() {
        let dir = tempdir().unwrap();
        write_md(dir.path(), "drain-rate-limited-worker", RATE_LIMIT);
        let store = PlaybookStore::load(dir.path()).unwrap();
        let hits = store.matching_trigger("RATE LIMIT HIT in worker 3");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn non_md_files_are_ignored() {
        let dir = tempdir().unwrap();
        write_md(dir.path(), "real-playbook", BROWNFIELD);
        fs::write(dir.path().join("README.txt"), "not a playbook").unwrap();
        fs::write(dir.path().join("notes.markdown"), "wrong extension").unwrap();
        let store = PlaybookStore::load(dir.path()).unwrap();
        assert_eq!(store.count(), 1);
    }
}
