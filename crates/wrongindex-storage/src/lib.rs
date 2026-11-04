//! WrongStack index storage contract.
//!
//! M1 scope: compatible content hashing and a strictly read-only
//! schema/profile inspector. Nothing in this crate writes to an `index.db` yet;
//! write primitives arrive with WI-108 behind ownership and profile admission.

pub mod backup;
pub mod content_hash;
pub mod git_containment;
pub mod inspect;
pub mod integrity;
pub mod owner_lock;
pub mod profile;
pub mod runtime;
pub mod wal;
pub mod wrongstack_paths;

pub use content_hash::{file_content_hash, xxhash64_hex};
pub use inspect::{Finding, Inspection, Severity, Verdict, inspect_index, inspect_index_checked};
pub use integrity::Integrity;
pub use profile::{Profile, baseline_profile};
pub use runtime::{SqliteRuntime, sqlite_runtime};
pub use wrongstack_paths::{ResolvedIndexDir, resolve_index_dir, wstack_global_root};
