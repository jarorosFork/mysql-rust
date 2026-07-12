//! `mysql-rust`: a MySQL-compatible database server written in Rust.
//!
//! This crate is an early-stage **skeleton**. Every module currently holds
//! stubs that sketch the intended architecture; none of the MySQL wire
//! protocol, authentication, parsing, or storage is implemented yet. The
//! goal of this commit is a clean-compiling foundation to build on.
//!
//! Module map:
//! - [`config`]   runtime configuration
//! - [`server`]   TCP listener + connection accept loop
//! - [`protocol`] MySQL wire-protocol framing (packets, handshake)
//! - [`auth`]     client authentication
//! - [`query`]    SQL parsing and execution
//! - [`storage`]  pluggable storage engines
//! - [`error`]    crate-wide error type

// The skeleton intentionally defines items before they are wired up.
// Remove these allows as the modules get fleshed out.
#![allow(dead_code, unused_variables)]

pub mod auth;
pub mod config;
pub mod error;
pub mod observability;
pub mod protocol;
pub mod query;
pub mod server;
pub mod storage;

pub use error::{Error, Result};
