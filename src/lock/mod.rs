//! Security-critical building blocks for `helium-locker`.
//!
//! `auth`, `secret`, `state` and `system` are vendored from Amorphis
//! (<https://github.com/dani-77/amorphis>, MIT, same author), which has used
//! them in production: a PAM worker thread so authentication never blocks the
//! Wayland event loop, an `mlock`ed password buffer that is wiped on clear and
//! drop, a fail-closed lock state machine, and process hardening plus a
//! passwd-database identity lookup. Keep them in sync with Amorphis rather
//! than diverging here.

// Vendored verbatim, so a few helpers helium-locker itself doesn't call
// (e.g. SecretBuffer::capacity) are kept rather than trimmed.
#![allow(dead_code)]

pub mod auth;
pub mod secret;
pub mod state;
pub mod system;
