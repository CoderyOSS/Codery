use serde::Deserialize;

impl std::fmt::Display for Role {
    /// Capitalized role word for human text (notices, /status); storage
    /// names stay lowercase via `rolemapping::role_name`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let word = match self {
            Role::None => "None",
            Role::Read => "Read",
            Role::Triage => "Triage",
            Role::Write => "Write",
            Role::Maintain => "Maintain",
            Role::Admin => "Admin",
        };
        f.write_str(word)
    }
}

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

/// Ancestry-dependent refinement of an update class (GATE-005): the parser
/// can decide deletions and creations on oid shape alone; an ordinary update
/// needs the fast-forward check before it is Push or ForcePush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Class(ActionClass),
    NeedsAncestry,
}

/// Fast-forward verdict for an ordinary update (GATE-005). `Unknown` covers
/// every case the upstream could not answer definitively and fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// `new` is a strict descendant of `old`.
    FastForward,
    /// `new` is not a descendant of `old` (behind or diverged).
    Force,
    /// The upstream compare could not be trusted; deny.
    Unknown,
}

/// The authenticated caller's permission tier on a repo (GATE-008),
/// ordered ascending by privilege, so comparisons read as spelled:
/// `role >= Role::Write` grants writers and above, denies everyone below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    None,
    Read,
    Triage,
    Write,
    Maintain,
    Admin,
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn role_orders_ascending_by_privilege() {
        assert!(Role::None < Role::Read);
        assert!(Role::Read < Role::Triage);
        assert!(Role::Triage < Role::Write);
        assert!(Role::Write < Role::Maintain);
        assert!(Role::Maintain < Role::Admin);
        // The comparisons a policy will actually write.
        assert!(Role::Write > Role::Read);
        assert!(Role::Admin > Role::Write);
    }

    #[test]
    fn role_display_is_the_capitalized_word() {
        assert_eq!(Role::None.to_string(), "None");
        assert_eq!(Role::Read.to_string(), "Read");
        assert_eq!(Role::Triage.to_string(), "Triage");
        assert_eq!(Role::Write.to_string(), "Write");
        assert_eq!(Role::Maintain.to_string(), "Maintain");
        assert_eq!(Role::Admin.to_string(), "Admin");
    }
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
