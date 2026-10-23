//! The host's network interfaces: which addresses this machine answers on.
//!
//! The interface is the [`Network`] trait; [`Host`] is its implementation. One body for every OS (`netdev`: netlink
//! on Linux, `getifaddrs` on the BSDs and macOS, `GetAdaptersAddresses` on Windows), so there are no per-OS files: an
//! OS would override [`Network::interface_ips`] only if it must. Callers import both:
//! `use crate::platform::network::{Host, Network};` then `Host::interface_ips()`.

use std::net::IpAddr;

/// What the host tells about its network.
pub trait Network {
    /// The addresses of the interfaces that are up and running and not loopback, IPv4 and IPv6; empty when the
    /// system cannot say.
    ///
    /// Loopback is the interface's own flag, not inferred from its addresses or name: `IFF_LOOPBACK` on Unix (so
    /// every address on `lo`, whatever its label, such as `lo:sv`, or its range), Windows' software-loopback interface
    /// type. A private address added to the loopback interface is never offered as a way in.
    fn interface_ips() -> Vec<IpAddr> {
        netdev::get_interfaces()
            .iter()
            .filter(|interface| {
                interface.is_up() && interface.is_running() && !interface.is_loopback()
            })
            .flat_map(|interface| interface.ip_addrs())
            .collect()
    }
}

/// This machine.
pub struct Host;

impl Network for Host {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_loopback_address_is_an_interface_address() {
        assert!(Host::interface_ips().iter().all(|ip| !ip.is_loopback()));
    }
}
