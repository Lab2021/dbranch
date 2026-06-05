//! Library surface of dbranch, exposed so integration tests and external
//! callers can exercise the same modules used by the CLI binary.

pub mod cli;
pub mod config;
pub mod copy_ref;
pub mod database_operator;
pub mod docker_stats;
pub mod dump;
pub mod error;
pub mod fiemap;
pub mod logbuf;
pub mod query;
pub mod schema;
pub mod schema_diff;
pub mod snapshot;
pub mod web;
