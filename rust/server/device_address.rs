//! Direct private addresses a device may use when this Core actually listens on the network.

use std::net::IpAddr;

use crate::platform::network::{Host, Network};

const MAX_LAN_URLS: usize = 7; // The web client reads at most eight code URLs, including loopback.

pub(super) fn code_urls(bind_host: &str, port: u16) -> Vec<String> {
    let bound = bind_host.parse::<IpAddr>().ok();
    let candidates = match bound {
        Some(IpAddr::V4(address)) if address.is_unspecified() => Host::interface_ips(),
        Some(IpAddr::V6(address)) if address.is_unspecified() => Host::interface_ips(),
        Some(address) => vec![address],
        None => Vec::new(),
    };
    code_urls_from(bound, port, candidates, super::trust::advertised_ip_allowed)
}

fn code_urls_from(
    bound: Option<IpAddr>,
    port: u16,
    candidates: impl IntoIterator<Item = IpAddr>,
    allowed: impl Fn(IpAddr) -> bool,
) -> Vec<String> {
    let mut urls = vec![format!("http://127.0.0.1:{port}")];
    let Some(bound) = bound else { return urls };
    for ip in candidates {
        if urls.len() > MAX_LAN_URLS || !same_family(bound, ip) || !private_ip(ip) {
            continue;
        }
        if !bound.is_unspecified() && bound != ip {
            continue;
        }
        if !allowed(ip) {
            continue;
        }
        let url = match ip {
            IpAddr::V4(ip) => format!("http://{ip}:{port}"),
            IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
        };
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

fn same_family(left: IpAddr, right: IpAddr) -> bool {
    matches!(
        (left, right),
        (IpAddr::V4(_), IpAddr::V4(_)) | (IpAddr::V6(_), IpAddr::V6(_))
    )
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            a == 10
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(ip) => ip.segments()[0] & 0xfe00 == 0xfc00,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_network_bind_advertises_only_private_addresses_on_its_family() {
        let candidates = [
            "192.168.1.7",
            "10.0.0.2",
            "127.0.0.1",
            "203.0.113.5",
            "fc00::1",
        ]
        .map(|value| value.parse().unwrap());
        assert_eq!(
            code_urls_from(Some("0.0.0.0".parse().unwrap()), 10829, candidates, |_| {
                true
            }),
            [
                "http://127.0.0.1:10829",
                "http://192.168.1.7:10829",
                "http://10.0.0.2:10829"
            ]
        );
        assert_eq!(
            code_urls_from(
                Some("127.0.0.1".parse().unwrap()),
                10829,
                candidates,
                |_| true
            ),
            ["http://127.0.0.1:10829"]
        );
    }

    #[test]
    fn explicit_network_bind_and_ipv6_keep_the_actual_port() {
        assert_eq!(
            code_urls_from(
                Some("192.168.1.7".parse().unwrap()),
                9000,
                ["192.168.1.7".parse().unwrap()],
                |_| true
            ),
            ["http://127.0.0.1:9000", "http://192.168.1.7:9000"]
        );
        assert_eq!(
            code_urls_from(
                Some("::".parse().unwrap()),
                10829,
                ["fc00::1".parse().unwrap()],
                |_| true
            ),
            ["http://127.0.0.1:10829", "http://[fc00::1]:10829"]
        );
        assert_eq!(
            code_urls_from(
                Some("0.0.0.0".parse().unwrap()),
                10829,
                ["192.168.1.7".parse().unwrap()],
                |_| false
            ),
            ["http://127.0.0.1:10829"]
        );
    }
}
