use std::net::SocketAddr;

use crate::runtime::servers::loopback_for;

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

#[test]
fn unspecified_addresses_are_probed_on_the_loopback_of_their_family() {
    let probe = |text: &str| loopback_for(address(text));
    assert_eq!(probe("0.0.0.0:9000"), address("127.0.0.1:9000"));
    assert_eq!(probe("[::]:9000"), address("[::1]:9000"));
    assert_eq!(probe("192.0.2.1:9000"), address("192.0.2.1:9000"));
}
