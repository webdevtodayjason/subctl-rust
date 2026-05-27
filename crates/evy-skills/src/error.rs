//! Skill-loading error helpers.
//!
//! Like `evy-memory`, this crate funnels its failures into the workspace
//! [`evy_core::Error`] enum rather than introducing a parallel `Error`
//! type — keeping a single `Result<T>` shape across the workspace.
//!
//! - I/O failures (missing dir, unreadable file) → [`Error::Io`].
//! - Frontmatter/YAML parse failures → [`Error::InvalidMandate`] (the
//!   closest-fit variant until `evy-core` grows a dedicated one, same
//!   convention `evy-memory` already established).

use std::path::Path;

use evy_core::Error;

/// Bad data found inside a skill frontmatter block or filename.
pub(crate) fn bad_skill(path: &Path, reason: impl std::fmt::Display) -> Error {
    Error::InvalidMandate(format!("skill {}: {reason}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bad_skill_carries_path_and_reason() {
        let p = PathBuf::from("/tmp/foo/SKILL.md");
        let s = bad_skill(&p, "missing triggers").to_string();
        assert!(s.contains("SKILL.md"), "got `{s}`");
        assert!(s.contains("missing triggers"), "got `{s}`");
    }
}
