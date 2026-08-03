//! Shared engine for the OpenCode family of hosts (OpenCode and Kilo).
//!
//! Kilo is a fork of OpenCode and shares its command surface, config shape, and
//! plugin model. One engine therefore serves both. The engine must **not** make
//! the two hosts aliases of each other: each has its own XDG roots, its own
//! environment prefix, its own project directory names, and its own config file
//! names. A value belonging to one host must never be read for the other.
//!
//! Every fact encoded here was verified against the pinned runtimes
//! (`opencode 1.18.11`, `kilo 7.4.17`) rather than taken from documentation.

pub mod layers;
