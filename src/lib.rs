//! Library API for the `hm` command-line tool.
//!
//! The binary should stay a thin adapter over this crate. Agent hooks, render
//! adapters, and future integrations need the same policy and storage behavior
//! as the CLI without reimplementing context detection or shelling out to `hm`.

pub mod config;
