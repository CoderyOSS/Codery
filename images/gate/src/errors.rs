//! Agent error envelope (GATE-010): the exact JSON shapes the API proxy
//! answers with when it cannot or must not forward a request. These are
//! semantics the agent can act on — run the setup flow, request an
//! elevation, fix the token's scopes — never raw upstream leakage.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// One of the four agent-facing error semantics, with its standing status
/// code:
///
/// | variant                 | status | agent-visible meaning            |
/// |-------------------------|--------|----------------------------------|
/// | `AccessPending`         | 404    | repo invisible: run setup flow   |
/// | `PermissionPending`     | 403    | role gap: elevation flow opened  |
/// | `RoleUnavailable`       | 403    | token lacks a required OAuth scope |
/// | `CredentialUnavailable` | 401    | no verified machine credential   |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// The repo is invisible to the machine account (upstream 404 or a
    /// live no-access check): the gate refuses and the setup flow must
    /// grant access first.
    AccessPending { repo: String, message: String },
    /// The caller's role is insufficient for the operation; a placeholder
    /// elevation flow row (`kind='elevation'`) backs `elevation_id` until
    /// the control plane takes it over.
    PermissionPending {
        elevation_id: String,
        needs_role: String,
        message: String,
    },
    /// The machine token lacks one of the OAuth scopes the upstream named.
    RoleUnavailable { message: String },
    /// No verified machine credential at request time; nothing was
    /// forwarded anywhere.
    CredentialUnavailable { message: String },
}

impl AgentError {
    /// Stable machine spelling used in the `error` JSON field and the
    /// audit basis.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AccessPending { .. } => "access_pending",
            Self::PermissionPending { .. } => "permission_pending",
            Self::RoleUnavailable { .. } => "role_unavailable",
            Self::CredentialUnavailable { .. } => "credential_unavailable",
        }
    }

    /// The standing status code for the variant (GATE-010).
    pub fn status(&self) -> StatusCode {
        match self {
            Self::AccessPending { .. } => StatusCode::NOT_FOUND,
            Self::PermissionPending { .. } | Self::RoleUnavailable { .. } => {
                StatusCode::FORBIDDEN
            }
            Self::CredentialUnavailable { .. } => StatusCode::UNAUTHORIZED,
        }
    }

    /// The exact JSON body (GATE-010 shapes).
    pub fn payload(&self) -> serde_json::Value {
        match self {
            Self::AccessPending { repo, message } => json!({
                "error": "access_pending",
                "repo": repo,
                "message": message,
            }),
            Self::PermissionPending {
                elevation_id,
                needs_role,
                message,
            } => json!({
                "error": "permission_pending",
                "elevation_id": elevation_id,
                "needs_role": needs_role,
                "message": message,
            }),
            Self::RoleUnavailable { message } => json!({
                "error": "role_unavailable",
                "message": message,
            }),
            Self::CredentialUnavailable { message } => json!({
                "error": "credential_unavailable",
                "message": message,
            }),
        }
    }

    /// Repo invisible to the machine account (upstream 404 / no access).
    pub fn access_pending(repo: &str) -> Self {
        Self::AccessPending {
            repo: repo.to_string(),
            message: format!(
                "repo {repo} is not visible to the machine account — \
                 run the gate setup flow to grant access"
            ),
        }
    }

    /// Role gap with a backing elevation flow row.
    pub fn permission_pending(elevation_id: String, needs_role: &str) -> Self {
        Self::PermissionPending {
            needs_role: needs_role.to_string(),
            message: format!(
                "insufficient role: this operation needs '{needs_role}' — \
                 elevation flow {elevation_id} opened for human review"
            ),
            elevation_id,
        }
    }

    /// Token scope gap; the message names the missing scope.
    pub fn role_unavailable(scope: &str) -> Self {
        Self::RoleUnavailable {
            message: format!("machine token lacks required OAuth scope '{scope}'"),
        }
    }

    /// No verified machine credential at request time.
    pub fn credential_unavailable() -> Self {
        Self::CredentialUnavailable {
            message: "no verified machine credential available — \
                      complete the machine-account setup flow"
                .to_string(),
        }
    }
}

impl IntoResponse for AgentError {
    fn into_response(self) -> Response {
        (self.status(), Json(self.payload())).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_pending_shape_and_status() {
        let e = AgentError::access_pending("octo/hello");
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
        assert_eq!(e.kind(), "access_pending");
        let v = e.payload();
        assert_eq!(v["error"], "access_pending");
        assert_eq!(v["repo"], "octo/hello");
        assert!(v["message"].is_string());
        // Exactly the three shape keys.
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 3, "{v}");
    }

    #[test]
    fn permission_pending_shape_and_status() {
        let e = AgentError::permission_pending("abc-123".to_string(), "maintain");
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(e.kind(), "permission_pending");
        let v = e.payload();
        assert_eq!(v["error"], "permission_pending");
        assert_eq!(v["elevation_id"], "abc-123");
        assert_eq!(v["needs_role"], "maintain");
        assert!(v["message"].is_string());
        assert_eq!(v.as_object().unwrap().len(), 4, "{v}");
    }

    #[test]
    fn role_unavailable_shape_and_status() {
        let e = AgentError::role_unavailable("repo");
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(e.kind(), "role_unavailable");
        let v = e.payload();
        assert_eq!(v["error"], "role_unavailable");
        assert!(
            v["message"].as_str().unwrap().contains("'repo'"),
            "message names the scope: {v}"
        );
        assert_eq!(v.as_object().unwrap().len(), 2, "{v}");
    }

    #[test]
    fn credential_unavailable_shape_and_status() {
        let e = AgentError::credential_unavailable();
        assert_eq!(e.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(e.kind(), "credential_unavailable");
        let v = e.payload();
        assert_eq!(v["error"], "credential_unavailable");
        assert!(v["message"].is_string());
        assert_eq!(v.as_object().unwrap().len(), 2, "{v}");
    }
}
