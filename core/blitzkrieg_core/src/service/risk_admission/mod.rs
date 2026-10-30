//! The order-admission pipeline, the risk gate's IPC knobs and the kernel
//! heartbeat, moved verbatim from `service.rs` (pure code motion, no logic
//! edits): `place` with its full refusal ladder (cross-account, cross-venue,
//! duration-subscription, freeze), the maker/taker fill state machines, the
//! cancel/flatten verbs, the risk-limit writers (`risk.limits`,
//! `systemic.limits`, `exit.set`, `round_duration.set`), `system.reset_trading`,
//! the error/daily-budget emitters, and `tick` with the escalation and exit
//! sweeps.

use crate::ipc::schema::Event;
use crate::model::*;
use crate::ome::{FillDelta, SubmitParams};
use crate::risk::RiskConfig;
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
