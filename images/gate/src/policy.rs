use std::collections::HashMap;

use serde::Deserialize;

use crate::types::{ActionClass, Decision};

/// Per-class rules for the global defaults or a single repo.
/// An unset class maps to `None` and decides as `Deny` (fail closed).
#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct ClassPolicy {
    #[serde(default)]
    push: Option<Decision>,
    #[serde(rename = "force-push", default)]
    force_push: Option<Decision>,
    #[serde(rename = "branch-delete", default)]
    branch_delete: Option<Decision>,
    #[serde(rename = "tag-delete", default)]
    tag_delete: Option<Decision>,
}

impl ClassPolicy {
    fn for_class(&self, class: ActionClass) -> Option<Decision> {
        match class {
            ActionClass::Push => self.push,
            ActionClass::ForcePush => self.force_push,
            ActionClass::BranchDelete => self.branch_delete,
            ActionClass::TagDelete => self.tag_delete,
        }
    }
}

/// The bind-mounted policy file: global defaults plus per-repo overrides.
///
/// ```toml
/// [defaults]
/// push = "allow"
///
/// [repos."owner/repo"]
/// tag-delete = "deny"
/// ```
#[derive(Debug, Default, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    defaults: ClassPolicy,
    #[serde(default)]
    repos: HashMap<String, ClassPolicy>,
}

impl PolicyFile {
    pub fn parse(toml_src: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(toml_src)?)
    }
}

/// Fail-closed policy engine (GATE-005).
#[derive(Debug, Default, Clone)]
pub struct PolicyEngine {
    defaults: ClassPolicy,
    repos: HashMap<String, ClassPolicy>,
}

impl PolicyEngine {
    pub fn new(file: &PolicyFile) -> Self {
        Self {
            defaults: file.defaults,
            repos: file.repos.clone(),
        }
    }

    /// Mutations on third-party repos (`own_sphere = false`) always require
    /// approval, whatever the file says. Otherwise the repo override wins
    /// over the defaults class-by-class (a partial override block inherits
    /// the defaults for classes it does not mention), and anything not
    /// covered anywhere denies.
    pub fn decide(&self, repo: &str, class: ActionClass, own_sphere: bool) -> Decision {
        if !own_sphere {
            return Decision::Approve;
        }
        self.repos
            .get(repo)
            .copied()
            .unwrap_or_default()
            .for_class(class)
            .or_else(|| self.defaults.for_class(class))
            .unwrap_or(Decision::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ActionClass as Class;

    const SAMPLE: &str = "\
[defaults]
push = \"allow\"
force-push = \"approve\"
branch-delete = \"approve\"
tag-delete = \"approve\"

[repos.\"acme/widget\"]
tag-delete = \"deny\"
";

    // Everything allowed, so third-party approval can only come from the
    // own_sphere gate, never from the file.
    const ALLOW_ALL: &str = "\
[defaults]
push = \"allow\"
force-push = \"allow\"
branch-delete = \"allow\"
tag-delete = \"allow\"
";

    fn engine(toml_src: &str) -> PolicyEngine {
        PolicyEngine::new(&PolicyFile::parse(toml_src).expect("policy parse"))
    }

    #[test]
    fn defaults_allow_push_for_own_sphere() {
        let e = engine(SAMPLE);
        assert_eq!(e.decide("acme/widget", Class::Push, true), Decision::Allow);
    }

    #[test]
    fn defaults_approve_force_push_for_own_sphere() {
        let e = engine(SAMPLE);
        assert_eq!(
            e.decide("acme/widget", Class::ForcePush, true),
            Decision::Approve
        );
    }

    #[test]
    fn per_repo_override_wins_over_defaults() {
        let e = engine(SAMPLE);
        assert_eq!(
            e.decide("acme/widget", Class::TagDelete, true),
            Decision::Deny
        );
        // Sibling repo without an override still gets the default.
        assert_eq!(
            e.decide("acme/other", Class::TagDelete, true),
            Decision::Approve
        );
    }

    #[test]
    fn third_party_mutations_approve_even_when_file_allows() {
        let e = engine(ALLOW_ALL);
        for class in [
            Class::Push,
            Class::ForcePush,
            Class::BranchDelete,
            Class::TagDelete,
        ] {
            assert_eq!(
                e.decide("acme/widget", class, false),
                Decision::Approve,
                "{class:?}"
            );
        }
        // Contrast: own-sphere push passes through as allow.
        assert_eq!(e.decide("acme/widget", Class::Push, true), Decision::Allow);
    }

    #[test]
    fn unknown_repo_third_party_approves() {
        let e = engine(SAMPLE);
        assert_eq!(
            e.decide("stranger/repo", Class::TagDelete, false),
            Decision::Approve
        );
    }

    #[test]
    fn unset_class_denies_for_own_sphere() {
        // Empty policy file: nothing configured => fail closed.
        let e = engine("");
        assert_eq!(e.decide("acme/widget", Class::Push, true), Decision::Deny);
        assert_eq!(
            e.decide("acme/widget", Class::BranchDelete, true),
            Decision::Deny
        );
    }

    #[test]
    fn parses_sample_config() {
        let f = PolicyFile::parse(SAMPLE).expect("parse");
        assert_eq!(f.defaults.push, Some(Decision::Allow));
        assert_eq!(f.defaults.force_push, Some(Decision::Approve));
        assert_eq!(f.defaults.branch_delete, Some(Decision::Approve));
        assert_eq!(f.defaults.tag_delete, Some(Decision::Approve));
        let widget = f.repos.get("acme/widget").expect("override present");
        assert_eq!(widget.tag_delete, Some(Decision::Deny));
    }
}
