use std::net::SocketAddr;

use crate::runtime::servers::loopback_for;

#[test]
fn unspecified_addresses_are_probed_on_the_loopback_of_their_family() {
    let probe = |address: &str| loopback_for(address.parse::<SocketAddr>().unwrap());
    assert_eq!(probe("0.0.0.0:9000"), "127.0.0.1:9000".parse().unwrap());
    assert_eq!(probe("[::]:9000"), "[::1]:9000".parse().unwrap());
    assert_eq!(probe("192.0.2.1:9000"), "192.0.2.1:9000".parse().unwrap());
}
