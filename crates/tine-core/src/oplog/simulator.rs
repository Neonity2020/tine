//! Deterministic scenario compatibility surface.
//!
//! The shared-provider protocol and its deterministic transport live in the
//! production `wire` module. Existing scenario tests import this module so the
//! test vocabulary stays stable, but production must import `wire` directly.

pub use super::wire::*;
