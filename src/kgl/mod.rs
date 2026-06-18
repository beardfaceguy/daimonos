//! KGL — the code + OS knowledge-graph layer (v0).
//!
//! KGL is NOT a language. It is an enforced, queryable graph of relationships,
//! intent, and provenance over an agent-first language substrate and (later)
//! daimonos live OS state. The MCP layer wires in the `kgl_query` / `kgl_assert`
//! tools and the (gated, off-by-default) observed-provenance hook; with those
//! gates off, KGL does not alter existing daimonos behavior.
//!
//! Build phases (see project tracker): v0 = substrate extraction (this module)
//! -> SQLite store -> reads/mutates analysis -> `kgl_query` MCP tool ->
//! completeness enforcement -> orient demo.
//!
//! `dead_code` is allowed crate-locally here because v0 lands the data model and
//! extraction before their consumers (store / MCP tool) exist — mirroring the
//! `plugins::x07` scaffolding precedent.
#![allow(dead_code)]

pub mod assert;
pub mod autoindex;
pub mod model;
pub mod observe;
pub mod query;
pub mod store;
pub mod substrate;
pub mod substrate_graphify;
pub mod substrate_x07;

#[cfg(test)]
mod demo;
