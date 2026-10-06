//! Existing room rendezvous link, with Socket.IO and loopback forwarding.
//!
//! The link reaches the paired room either by dialling out to its `/nodes`
//! namespace (`client`) or by accepting the room's dial on `/room` (`dial`).
//! Both carry the same relay events, forwarded to this Core over loopback
//! (`relay`).

mod client;
mod dial;
mod link;
mod packet;
mod pairing;
mod relay;
mod state;
#[cfg(test)]
mod test_support;

pub use dial::layer;
pub use link::Rendezvous;
