//! EncoderCraft client backend: SSH provisioning and tunnelling to remote nodes.
//!
//! The Tauri UI layer will depend on this crate; nothing here is GUI-specific.

pub mod provision;
mod shell;
pub mod ssh;
pub mod remote;
