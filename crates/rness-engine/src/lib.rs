//! The agent engine. Depends on kernel + protocol only.
//!
//! Core invariant (runtime-asserted in [`invariants`]):
//!   MODEL-VISIBLE MEANS LOGGED — anything that reaches a model request
//!   must be reconstructable from the append-only session log.
//!
//! - [`session`]: JSONL event log, branching (fork points), projections, migrations
//! - [`turn`]: turn loop, steps, attempt preservation
//! - [`inbox`]: single inbox with followup / steer / inject intents
//! - [`tools`]: registry + parallel dispatch with model-order commits
//! - [`interaction`]: UI-agnostic approval / ask-user contracts
//! - [`service`]: SessionService — the ONLY public API frontends use

pub mod sandbox;

pub mod images;
pub mod file_references;
pub mod plan;
pub mod tasks;
pub mod questions;
pub mod approval;
pub mod config;
pub mod inbox;
pub mod instructions;
pub mod interaction;
pub mod presentation;
pub mod invariants;
pub mod service;
pub mod session;
pub mod subagent;
pub mod tools;
pub mod turn;
