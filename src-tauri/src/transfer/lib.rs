// ------------ Transfer Core ------------
// The download core every game path shares: connections, retries, disk writes, verification and durability.
// It sits beside the backend that uses it but is its own crate (see Cargo.toml here, imported as `peebify_transfer`),
// so release builds compile it for speed while the rest of the launcher is built for size.

pub mod chunks;
pub mod control;
pub mod files;
pub mod fsx;
pub mod ledger;
pub mod limiter;
pub mod net;
pub mod part;
pub mod progress;
pub mod ranges;
mod retry;
pub mod runtime;
pub mod stream;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use chunks::{fetch_chunks, ChunkError, ChunkReport, ChunkSink, ChunkSpec};
pub use control::{check_cancel, wait_while_paused, Control, FileEvent, FileHooks, RunGate, GAMING_HOLD};
pub use files::{download_files, FileSpec, JobHooks, TransferReport};
pub use ledger::{Ledger, Sealed};
pub use progress::{FileCounter, ProgressTracker, Publisher};
pub use ranges::{RangeBody, RangeReader};
pub use runtime::{cores, RuntimeConfig, Settings, TransferRuntime, WhileGaming};
pub use stream::download_stream;
