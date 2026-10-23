//! The host's network interfaces: which addresses this machine answers on.
//!
//! The interface is the [`Network`] trait; [`Host`] is its implementation. One body for every OS (`if-addrs`:
//! `getifaddrs` on Unix, `GetAdaptersAddresses` on Windows), so there are no per-OS files: an OS would override
//! [`Network::interface_ips`] only if it must. Callers import both: `use crate::platform::network::{Host, Network};`
//! then `Host::interface_ips()`.

use std::net::IpAddr;

/// What the host tells about its network.
pub trait Network {
    /// The addresses of the interfaces that are up and not loopback, IPv4 and IPv6; empty when the system cannot say.
    fn interface_ips() -> Vec<IpAddr> {
        if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|interface| interface.is_oper_up() && !interface.is_loopback())
            .map(|interface| interface.ip())
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
