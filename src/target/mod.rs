//! Execution target management.
//!
//! Targets allow commands to execute remotely (e.g., in a Codespace) while
//! the agent stays local. This module handles target lifecycle, state
//! persistence, and remote execution.

pub mod codespace;
pub mod store;
pub mod types;

pub use store::TargetStore;
pub use types::{TargetState, TargetType};
