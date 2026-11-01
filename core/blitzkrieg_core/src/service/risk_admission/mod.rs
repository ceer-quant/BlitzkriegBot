//! The order-admission pipeline, the risk gate's IPC knobs and the kernel
//! heartbeat, moved verbatim from `service.rs` (pure code motion, no logic
//! edits): `place` with its full refusal ladder (cross-account, cross-venue,
//! duration-subscription, freeze), the maker/taker fill state machines, the
//! cancel/flatten verbs, the risk-limit writers (`risk.limits`,
//! `systemic.limits`, `exit.set`, `round_duration.set`), `system.reset_trading`,
//! the error/daily-budget emitters, and `tick` with the escalation and exit
//! sweeps.

use crate::model::*;
use crate::ome::{FillDelta, SubmitParams};
use crate::risk::RiskConfig;
use crate::schema::Event;
use crate::sim::Book;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

#[cfg(test)]
use super::config::VirtualAccountLimit;
use super::config::{EscalationTarget, FlattenTarget, InflightTaker, PlaceOutcome};

use super::Core;
use super::helpers::{escalation_delay_ms, exit_intent_key, is_close_intent};
use super::parse_direction;
use super::{EXIT_REPORT_BEAT_MS, marketable_exit_price};

mod admission;
mod exits;
mod freeze;

// #834 cycle 1: the freeze/circuit-breaker state book lives beside its only
// manager; `service.rs` re-exports it for the struct field and the
// integration tree's handle.
pub(crate) use freeze::FreezeBook;
