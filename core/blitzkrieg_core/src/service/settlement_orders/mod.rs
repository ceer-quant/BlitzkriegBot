//! Order lifecycle, fill application and the settlement/redemption family,
//! moved verbatim from `service.rs` (pure code motion, no logic edits):
//! placement bookkeeping (place/confirm/reject), fill projection into the
//! ledger and position book, the pending-close exit-reason table, the
//! close-credential unwind journal, market resolution → redemption/merge
//! plumbing, the expiry sweeps and the dry settlement ladder.

use crate::model::*;
use crate::ome::{FillDelta, FillOutcome, LateFill, SubmitParams};
use crate::position::OpenParams;
use crate::schema::Event;
use crate::settlement::{
    EXPIRY_FALLBACK_AFTER_MS, SETTLEMENT_EXIT_REASON, UNVERIFIED_EXPIRY_EXIT_REASON, booking_for,
    settlement_key,
};
use crate::sim::{Book, rests_on_book};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashSet;

use super::config::COUNTER_DECISIVE_HIGH;
use super::config::EXIT_REASON_TABLE_MAX;
use super::helpers::{
    BENIGN_CLOSE_DIVERGENCE_ATTEMPTS, FREEZE_ON_REJECTS, MAX_PLACE_ATTEMPTS, dec_json,
    escalation_delay_ms, is_close_intent, is_position_already_gone, reject_backoff_ms,
    strategy_acc_key,
};
use super::{CloseCredential, CloseCredentialLine, Core, parse_direction};

mod closing;
mod orders;
mod settlement;
