//! crossterm input reader — runs on a dedicated thread and forwards terminal
//! events to the async main loop over a channel.

use crossterm::event::{self, Event};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Read terminal events until the channel closes or an error occurs, mapping each
/// event into the caller's message type.
///
/// A short poll timeout keeps the thread responsive to a receiver being dropped
/// (so the process can exit cleanly instead of hanging in `read`).
pub fn read_loop<T, F>(tx: UnboundedSender<T>, map: F)
where
    T: Send + 'static,
    F: Fn(Event) -> T + Send + 'static,
{
    loop {
        match event::poll(Duration::from_millis(250)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(map(ev)).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Ok(false) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}
