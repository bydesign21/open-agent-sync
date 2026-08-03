//! Generated hook bridges for the OpenCode family.
//!
//! Each host gets its own module. The two module *shapes* are deliberately not
//! interchangeable — see the per-host docs — so a bridge generated for one host
//! cannot load in the other.
//!
//! Every fact these modules encode was measured against the pinned runtimes
//! (`opencode 1.18.11`, `kilo 7.4.17`). See `docs/open-work.md`.

pub mod kilo;
pub mod opencode;
