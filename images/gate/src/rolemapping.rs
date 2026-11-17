//! Role map (GATE-010): which minimum [`Role`] an action requires, for the
//! pre-flight check before forwarding (task B8) and for reading the
//! registry's role names.
//!
//! The table is pinned to GATE-010 verbatim:
//!
//! | action                                        | min role |
//! |-----------------------------------------------|----------|
//! | read (GET/HEAD)                               | Read     |
//! | label / close / assign (issue work)           | Triage   |
//! | push branches, open/merge PRs, releases,      | Write    |
//! | push `.github/workflows/`                     |          |
//! | manage topics / Pages                         | Maintain |
//! | settings / collaborators / rulesets,          | Admin    |
//! | delete repo                                   |          |
//!
//! Git-side classes map from [`crate::types::ActionClass`] (GATE-005):
//! every ref-update class — push, force-push, branch/tag delete — requires
//! Write (deletion is never below the write tier).
//!
//! **P0 API classification is a heuristic table**, documented here so
//! contradictions are fixable: `classify_api(method, path)` matches on
//! `METHOD + /repos/{owner}/{repo}/...` path shape, first rule wins:
//!
//! 1. `GET`/`HEAD` anywhere ⇒ Read.
//! 2. Exact `/repos/{o}/{r}` with `DELETE` or `PATCH` ⇒ Admin
//!    (delete / edit repo settings).
//! 3. First repo sub-segment routes the rest:
//!    `collaborators` | `rulesets` | `hooks` ⇒ Admin;
//!    `branches` + a `/protection` suffix ⇒ Admin (branch protection);
//!    `pages` | `topics` ⇒ Maintain;
//!    `labels` ⇒ Triage; issue mutations (create, close/reopen via
//!    `PATCH /issues/{n}`, labels, assignees) ⇒ Triage.
//! 4. Every other repo mutation ⇒ Write (pulls, merges, releases,
//!    contents incl. `.github/workflows/` files, statuses, refs).
//!    Issue **comments** land here too — GitHub allows them at Read, so
//!    this over-classifies slightly (the safe direction: pre-flight may
//!    ask for elevation GitHub wouldn't need, never the reverse).
//! 5. Mutations outside `/repos/{o}/{r}` ⇒ Admin (account/org-level
//!    operations; conservative — GitHub remains the actual enforcer, and
//!    GATE-010 logs map-vs-reality contradictions for corrections).
//!
//! An over-classification delays an action behind an elevation ask; an
//! under-classification would let a doomed request through. The table is
//! deliberately written to err over-strict, never under.

use crate::types::{ActionClass, Role};

/// API-side action classes (P0 heuristic output; GATE-010 role-map rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiActionClass {
    /// Reads: `GET`/`HEAD` on anything.
    Read,
    /// Issue work: labels, close/reopen, assign.
    Triage,
    /// Ordinary mutations: pushes, PR open/merge, releases, workflow files.
    Write,
    /// Repo metadata that shapes the project: topics, Pages.
    Maintain,
    /// Settings: collaborators, rulesets, hooks, protection, repo edit/delete.
    Admin,
}

/// The action whose minimum role the map answers for: a git ref-update
/// class (GATE-005) or a classified API call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Git(ActionClass),
    Api(ApiActionClass),
}

/// The GATE-010 role map: minimum [`Role`] for one action class.
pub fn min_role(action: Action) -> Role {
    match action {
        // Git-side: every ref-update class — push, force-push, branch/tag
        // delete — is Write-tier (deletion is never below write).
        Action::Git(
            ActionClass::Push
            | ActionClass::ForcePush
            | ActionClass::BranchDelete
            | ActionClass::TagDelete,
        ) => Role::Write,
        Action::Api(class) => match class {
            ApiActionClass::Read => Role::Read,
            ApiActionClass::Triage => Role::Triage,
            ApiActionClass::Write => Role::Write,
            ApiActionClass::Maintain => Role::Maintain,
            ApiActionClass::Admin => Role::Admin,
        },
    }
}

/// Classify one API call from its method and path (P0 heuristic, see the
/// module docs). `path` is the upstream path (`/repos/...`); an
/// `/api/v3` proxy prefix is tolerated and stripped.
pub fn classify_api(method: &str, path: &str) -> ApiActionClass {
    let method = method.to_ascii_uppercase();
    // Rule 1: reads anywhere, before any path shape is inspected.
    if method == "GET" || method == "HEAD" {
        return ApiActionClass::Read;
    }
    let path = path.strip_prefix("/api/v3").unwrap_or(path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() >= 3 && segs[0] == "repos" {
        let rest = &segs[3..];
        if rest.is_empty() {
            // Rule 2: exact /repos/{o}/{r} — edit settings or delete repo.
            return match method.as_str() {
                "DELETE" | "PATCH" => ApiActionClass::Admin,
                _ => ApiActionClass::Write,
            };
        }
        // Rule 3: first repo sub-segment routes the rest.
        return match rest[0] {
            "collaborators" | "rulesets" | "hooks" => ApiActionClass::Admin,
            "branches" if rest.contains(&"protection") => ApiActionClass::Admin,
            "pages" | "topics" => ApiActionClass::Maintain,
            "labels" => ApiActionClass::Triage,
            "issues" => classify_issues(rest),
            // Rule 4: every other repo mutation (pulls, merges, releases,
            // contents incl. workflow files, statuses, refs…).
            _ => ApiActionClass::Write,
        };
    }
    // Rule 5: non-repo mutations are account/org-level ⇒ conservative Admin.
    ApiActionClass::Admin
}

/// Issue-work sub-classification (rest starts with `issues`): create,
/// close/reopen/assign via the issue PATCH, and label/assignee paths are
/// Triage; comments and anything else fall through to the Write default
/// (documented over-classification — see module docs).
fn classify_issues(rest: &[&str]) -> ApiActionClass {
    match rest.len() {
        1 | 2 => ApiActionClass::Triage, // POST /issues, PATCH /issues/{n}
        _ => match rest[2] {
            "labels" | "assignees" => ApiActionClass::Triage,
            _ => ApiActionClass::Write,
        },
    }
}

/// Storage name for a role — the lowercase registry strings GATE-010 pins:
/// `admin|maintain|write|triage|read|none`.
pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::None => "none",
        Role::Read => "read",
        Role::Triage => "triage",
        Role::Write => "write",
        Role::Maintain => "maintain",
        Role::Admin => "admin",
    }
}

/// Inverse of [`role_name`]; `None` on anything unparsable (callers treat
/// an unparsable stored role as `Role::None` — fail conservative).
pub fn role_from_name(name: &str) -> Option<Role> {
    match name {
        "none" => Some(Role::None),
        "read" => Some(Role::Read),
        "triage" => Some(Role::Triage),
        "write" => Some(Role::Write),
        "maintain" => Some(Role::Maintain),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the six GATE-010 table rows, exact ---------------------------------

    #[test]
    fn row1_reads_need_read_only() {
        let class = classify_api("GET", "/repos/acme/api/issues");
        assert_eq!(class, ApiActionClass::Read);
        assert_eq!(min_role(Action::Api(class)), Role::Read);
        assert_eq!(classify_api("HEAD", "/repos/acme/api"), ApiActionClass::Read);
    }

    #[test]
    fn row2_label_close_assign_need_triage() {
        assert_eq!(
            classify_api("POST", "/repos/acme/api/issues/7/labels"),
            ApiActionClass::Triage
        );
        assert_eq!(
            classify_api("DELETE", "/repos/acme/api/issues/7/labels/bug"),
            ApiActionClass::Triage
        );
        assert_eq!(
            classify_api("PATCH", "/repos/acme/api/issues/7"),
            ApiActionClass::Triage,
            "close/reopen/assign ride the issue PATCH"
        );
        assert_eq!(
            classify_api("POST", "/repos/acme/api/issues/7/assignees"),
            ApiActionClass::Triage
        );
        for path in [
            "/repos/acme/api/issues/7/labels",
            "/repos/acme/api/issues/7",
            "/repos/acme/api/issues/7/assignees",
        ] {
            assert_eq!(min_role(Action::Api(classify_api("POST", path))), Role::Triage);
        }
    }

    #[test]
    fn row3_push_prs_releases_workflows_need_write() {
        // Git-side: every ref-update class is Write.
        for class in [
            ActionClass::Push,
            ActionClass::ForcePush,
            ActionClass::BranchDelete,
            ActionClass::TagDelete,
        ] {
            assert_eq!(min_role(Action::Git(class)), Role::Write);
        }
        // API-side: PR open/merge, releases, contents (incl. workflow files).
        assert_eq!(classify_api("POST", "/repos/acme/api/pulls"), ApiActionClass::Write);
        assert_eq!(
            classify_api("PUT", "/repos/acme/api/pulls/12/merge"),
            ApiActionClass::Write
        );
        assert_eq!(
            classify_api("POST", "/repos/acme/api/releases"),
            ApiActionClass::Write
        );
        assert_eq!(
            classify_api(
                "PUT",
                "/repos/acme/api/contents/.github/workflows/ci.yml"
            ),
            ApiActionClass::Write,
            "pushing .github/workflows/ is a Write action"
        );
        assert_eq!(
            min_role(Action::Api(classify_api("POST", "/repos/acme/api/pulls"))),
            Role::Write
        );
    }

    #[test]
    fn row4_topics_and_pages_need_maintain() {
        assert_eq!(
            classify_api("PUT", "/repos/acme/api/topics"),
            ApiActionClass::Maintain
        );
        assert_eq!(
            classify_api("POST", "/repos/acme/api/pages"),
            ApiActionClass::Maintain
        );
        assert_eq!(
            min_role(Action::Api(classify_api("PUT", "/repos/acme/api/topics"))),
            Role::Maintain
        );
    }

    #[test]
    fn row5_settings_collaborators_rulesets_need_admin() {
        assert_eq!(
            classify_api("PATCH", "/repos/acme/api"),
            ApiActionClass::Admin,
            "editing repo settings"
        );
        assert_eq!(
            classify_api("PUT", "/repos/acme/api/collaborators/bob"),
            ApiActionClass::Admin
        );
        assert_eq!(
            classify_api("POST", "/repos/acme/api/rulesets"),
            ApiActionClass::Admin
        );
        assert_eq!(
            classify_api("PUT", "/repos/acme/api/branches/main/protection"),
            ApiActionClass::Admin,
            "branch protection is settings-tier"
        );
        assert_eq!(
            min_role(Action::Api(classify_api("PUT", "/repos/acme/api/collaborators/bob"))),
            Role::Admin
        );
    }

    #[test]
    fn row6_delete_repo_needs_admin() {
        assert_eq!(
            classify_api("DELETE", "/repos/acme/api"),
            ApiActionClass::Admin
        );
        assert_eq!(
            min_role(Action::Api(classify_api("DELETE", "/repos/acme/api"))),
            Role::Admin
        );
    }

    // -- heuristic-table edges, pinned so drift is visible -------------------

    #[test]
    fn classify_api_tolerates_the_api_v3_proxy_prefix() {
        assert_eq!(
            classify_api("GET", "/api/v3/repos/acme/api/issues"),
            ApiActionClass::Read
        );
        assert_eq!(
            classify_api("PUT", "/api/v3/repos/acme/api/topics"),
            ApiActionClass::Maintain
        );
    }

    #[test]
    fn classify_api_unknown_repo_mutation_defaults_to_write() {
        // Documented P0 default (see module docs): over-strict, never loose.
        assert_eq!(
            classify_api("POST", "/repos/acme/api/statuses/abc123"),
            ApiActionClass::Write
        );
        assert_eq!(
            classify_api("POST", "/repos/acme/api/issues/7/comments"),
            ApiActionClass::Write,
            "comments over-classify to Write on purpose (GitHub allows Read)"
        );
    }

    #[test]
    fn classify_api_non_repo_mutation_is_conservatively_admin() {
        assert_eq!(classify_api("POST", "/user/repos"), ApiActionClass::Admin);
        assert_eq!(classify_api("GET", "/user/repos"), ApiActionClass::Read);
    }

    // -- registry storage names ----------------------------------------------

    #[test]
    fn role_names_roundtrip_through_the_g010_strings() {
        for role in [
            Role::None,
            Role::Read,
            Role::Triage,
            Role::Write,
            Role::Maintain,
            Role::Admin,
        ] {
            assert_eq!(role_from_name(role_name(role)), Some(role));
        }
        assert_eq!(role_name(Role::None), "none");
        assert_eq!(role_name(Role::Admin), "admin");
        assert_eq!(role_from_name("garbage"), None);
    }
}
