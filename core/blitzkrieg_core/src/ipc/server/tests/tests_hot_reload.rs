#![cfg(test)]

use super::*;

use crate::account::{AccountId, AccountStatus};
use crate::ipc::schema::*;
use crate::model::{CoreError, CoreErrorCode, Mode};
use crate::service::{Core, CoreConfig};
use blitzkrieg_market_api::net::now_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};

// ── #191: the limited hot reload, at the wire ───────────────────────────
//
// The issue is about what a client may change LIVE, so both tests below
// travel the road a panel/CLI does — `handle_line` with the params bytes.
// Between them they pin the three facts that make the feature safe: the
// change lands (with its audit record), the NEXT order is judged by the new
// value, and everything outside the entry limits is refused with nothing
// applied.
