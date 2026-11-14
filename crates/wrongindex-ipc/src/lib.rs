//! Local authenticated IPC for WrongIndex (ADR WI-ADR-007).
//!
//! Windows uses a named pipe whose DACL admits only the current user's SID and
//! LocalSystem, with remote clients rejected. Unix uses a stream socket in an
//! owner-only directory, guarded by a lock file, with the peer's uid checked
//! on both ends (WI-ADR-024).

pub mod client;
pub mod endpoint;
pub mod frame;
pub mod launch;
#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod win;

pub use client::{CallError, Client, EventStream};
pub use endpoint::{DataRoot, Discovery};
pub use frame::{FrameError, read_frame, write_frame};
