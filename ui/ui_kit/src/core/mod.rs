//! UI Kit core — the shared data model, IPC client and event bus.

pub mod event_bus;
#[cfg(test)]
mod evolution_tests;
pub mod ipc_client;
#[cfg(test)]
mod ipc_client_tests;
pub mod notifier;
#[cfg(test)]
mod notifier_tests;
pub mod types;
#[cfg(test)]
mod types_tests;
