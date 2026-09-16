//! UI Kit core — the shared data model, IPC client and event bus.

pub mod event_bus;
pub mod ipc_client;
pub mod notifier;
#[cfg(test)]
mod notifier_tests;
pub mod types;
