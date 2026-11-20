//! GitAuthGate core library: policy, git smart-HTTP proxy, GitHub adapter,
//! approvals, state. The `gate` binary in `main.rs` wires these into one
//! axum server.
//
// The matrix-sdk crypto stack nests generics deep enough that the default
// trait-solver recursion limit overflows on Send-ness checks (E0275 on the
// sync future) — see GATE-012.
#![recursion_limit = "512"]

pub mod approval;
pub mod apiproxy;
pub mod audit;
pub mod config;
pub mod custody;
pub mod errors;
pub mod github;
pub mod gitproxy;
pub mod matrix;
pub mod onepage;
pub mod pktline;
pub mod policy;
pub mod registry;
pub mod rolemapping;
pub mod routes;
pub mod sphere;
pub mod state;
pub mod tasks;
pub mod control;
pub mod tokens;
pub mod types;
