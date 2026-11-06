//! Re-export of the shared password service in [`aaemu_commons::password`].
//!
//! The implementation (ASP.NET Core Identity V3 hashing, legacy SHA-256 support and its C#
//! parity tests) lives in `aaemu-commons`; `aaemu_login::services::password` keeps the old
//! paths.

pub use aaemu_commons::password::*;
