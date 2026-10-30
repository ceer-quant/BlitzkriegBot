//! .env file watcher for hot reload.
//!
//! Monitors the .env file for changes and triggers configuration reloads
//! (accounts, policy, shadow, runtime) via a tokio task.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use tokio::sync::mpsc;

/// Reload request sent from the watcher task to the main loop.
#[derive(Debug, Clone)]
pub enum ReloadRequest {
    /// Reload account configuration (ACCOUNT_* keys)
    Accounts,
    /// Reload execution policy (POLICY_* keys)
    Policy,
    /// Reload shadow evolution switches (SHADOW_* keys)
    Shadow,
    /// Reload runtime configuration (RUNTIME_* keys)
    Runtime,
}

/// Runs the file watcher task.
///
/// Monitors `env_path` for changes and sends reload requests via `sender`.
/// Returns when the sender channel is closed or watching fails.
pub async fn run(env_path: &Path, sender: mpsc::UnboundedSender<ReloadRequest>) {
    tracing::info!("env watcher: starting to monitor {}", env_path.display());

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<()>(1);

    let env_path = env_path.to_path_buf();

    // Spawn the watcher in a blocking task
    tokio::task::spawn_blocking(move || {
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                if let Ok(_event) = res {
                    let _ = event_tx.blocking_send(());
                }
            },
            notify::Config::default(),
        )
        .expect("failed to create watcher");

        watcher
            .watch(&env_path, RecursiveMode::NonRecursive)
            .expect("failed to watch .env");

        // Block forever to keep the watcher alive
        std::thread::park();
    });

    // Wait for file change events
    while event_rx.recv().await.is_some() {
        tracing::info!("env watcher: .env changed, triggering reload");
        // Send reload requests for all config types
        if sender.send(ReloadRequest::Accounts).is_err() {
            tracing::info!("env watcher: sender closed, stopping");
            break;
        }
        if sender.send(ReloadRequest::Policy).is_err() {
            break;
        }
        if sender.send(ReloadRequest::Shadow).is_err() {
            break;
        }
        if sender.send(ReloadRequest::Runtime).is_err() {
            break;
        }
    }

    tracing::info!("env watcher: stopped");
}
