//! Local authenticated IPC for WrongIndex (ADR WI-ADR-007).
//!
//! Windows uses a named pipe whose DACL admits only the current user's SID and
//! LocalSystem, with remote clients rejected. Unix sockets are later work
//! (M8); on other platforms the transport functions return `Unsupported`.

pub mod client;
pub mod endpoint;
pub mod frame;
pub mod launch;
#[cfg(windows)]
pub mod win;

pub use client::{CallError, Client, EventStream};
pub use endpoint::{DataRoot, Discovery};
pub use frame::{FrameError, read_frame, write_frame};
