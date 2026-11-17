use serde::Deserialize;

/// Classification of a git receive-pack ref update (GATE-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    /// Ordinary push (including ref creation).
    Push,
    /// Non-fast-forward update.
    ForcePush,
    /// Deletion of a branch ref.
    BranchDelete,
    /// Deletion of a tag ref.
    TagDelete,
}

/// Policy outcome for an action (GATE-005). Unconfigured defaults to Deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decision {
    #[default]
    Deny,
    Allow,
    Approve,
}

impl<'de> Deserialize<'de> for Decision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        // Anything the config does not spell out explicitly fails closed.
        Ok(match s.as_str() {
            "allow" => Decision::Allow,
            "approve" => Decision::Approve,
            _ => Decision::Deny,
        })
    }
}
