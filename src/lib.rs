//! Library API for the `hm` command-line tool.
//!
//! The binary should stay a thin adapter over this crate. Agent hooks and
//! future integrations need the same policy and storage behavior as the CLI
//! without reimplementing context detection or shelling out to `hm`.

pub mod classify;
pub mod config;
pub mod context;
pub mod curated;
pub mod curation;
pub mod doctor;
pub mod event;
pub mod hook;
pub mod id;
pub mod index;
pub mod inject;
pub mod llm;
pub mod memory;
pub mod note;
pub mod outbox;
pub mod path;
pub mod project;
pub mod search;
pub mod secret;
pub mod signals;
pub mod store;
pub mod version;
pub mod visibility;
pub mod write;
pub mod write_classify;
