//! networkcop — a terminal agent harness for front-end debugging.
//!
//! Drives Chrome over the DevTools Protocol, records the whole session to SQLite,
//! and answers questions strictly from what it captured.

pub mod agent;
pub mod app;
pub mod cdp;
pub mod db;
pub mod tui;
