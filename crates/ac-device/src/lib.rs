//! Device management: discovery routing, pairing, connection lifecycle,
//! heartbeat and telemetry for up to `general.max_slots` slots (default 4, at most 5).

mod api;
mod manager;
pub mod probe;
pub mod qr;
pub mod slot;
pub mod stream;
pub mod transfer;

pub use api::*;
pub use manager::{ManagerHandle, ManagerOptions, normalize_addr, parse_inet, spawn};
pub use slot::StreamProfile;
pub use stream::{SlotIo, StreamInfo};
pub use ac_direct::PORT as DIRECT_PORT;
