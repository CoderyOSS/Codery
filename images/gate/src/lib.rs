//! GitAuthGate core library: policy, git smart-HTTP proxy, GitHub adapter,
//! approvals, state. The `gate` binary in `main.rs` wires these into one
//! axum server.


pub mod approval;
pub mod apiproxy;
pub mod audit;
pub mod config;
pub mod custody;
pub mod errors;
pub mod github;
pub mod gitproxy;
pub mod ntfy;
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
