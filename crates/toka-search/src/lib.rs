//! Toka search and indexing, without the app around it.
//!
//! The owned SQLite index, the search engine on top of it, filename tags,
//! thumbnail generation, the search log, and the background indexer daemon.
//! The Toka app uses this crate for everything it searches or indexes; the
//! `ts` shell utility will do the same.

pub mod config;
pub mod daemon_state;
pub mod index_stats;
pub mod indexer_daemon;
pub mod lossy_path;
pub mod owned_index;
pub mod providers;
pub mod reset;
pub mod search;
pub mod search_log;
pub mod sidecars;
pub mod tags;
pub mod thumbnails;

mod index_visibility;
mod timestamp;
