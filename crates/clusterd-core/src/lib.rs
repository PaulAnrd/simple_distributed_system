//! Agent runtime and supporting logic for `clusterd`. The `clusterd` bin
//! crate is a thin CLI shell over what's exposed here.

pub mod agent;
pub mod config;
pub mod error;
pub mod identity;
pub mod ipc;
pub mod nodes;
pub mod resources;
pub mod tls;

pub use error::{Error, Result};
