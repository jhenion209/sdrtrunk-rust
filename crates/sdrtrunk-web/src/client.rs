//! The visitor's address. `X-Forwarded-For` is believed only from the proxies in `SDRTRUNK_TRUSTED_PROXIES`; anyone
//! else could write it. (Ported from ScannerPulse's `web/client.rs`.)

use std::net::IpAddr;

use axum::http::HeaderMap;

use crate::web_config::Proxy;

/// The connection's address, or, when that is a trusted proxy, the last address in `X-Forwarded-For` that is not a
/// trusted proxy: each proxy appends the address it heard from, so the entries to its left may be anyone's invention.
/// Reading stops at the first entry that is not an address.
pub fn address(peer: IpAddr, headers: &HeaderMap, trusted: &[Proxy]) -> IpAddr {
    // A dual-stack listener shows an IPv4 peer as `::ffff:a.b.c.d`.
    let peer = peer.to_canonical();
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|t| t.contains(*ip));
    if !is_trusted(&peer) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect::<Vec<&str>>()
        .into_iter()
        .rev()
        .map_while(|entry| entry.parse::<IpAddr>().ok().map(|ip| ip.to_canonical()))
        .collect();
    forwarded.into_iter().find(|ip| !is_trusted(ip)).unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append("x-forwarded-for", value.parse().unwrap());
        }
        map
    }

    const PROXY: &str = "192.0.2.10";
    fn proxies(list: &[&str]) -> Vec<Proxy> { list.iter().map(|p| Proxy::parse(p).unwrap()).collect() }

    #[test]
    fn a_header_from_anyone_else_is_ignored() {
        let peer: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(address(peer, &headers(&["203.0.113.9"]), &proxies(&[PROXY])), peer);
    }

    #[test]
    fn the_proxy_gives_the_last_address_it_did_not_add_itself() {
        let peer: IpAddr = PROXY.parse().unwrap();
        let trusted = proxies(&[PROXY, "192.0.2.11"]);
        let want: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(address(peer, &headers(&["203.0.113.9"]), &trusted), want);
        // A visitor's own invented entry sits to the left and is not believed.
        assert_eq!(address(peer, &headers(&["198.51.100.1, 203.0.113.9"]), &trusted), want);
        assert_eq!(address(peer, &headers(&["198.51.100.1", "203.0.113.9, 192.0.2.11"]), &trusted), want);
    }

    #[test]
    fn a_trusted_range_covers_every_node() {
        let peer: IpAddr = "192.168.104.7".parse().unwrap();
        let want: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(address(peer, &headers(&["203.0.113.9"]), &proxies(&["192.168.104.0/24"])), want);
    }

    #[test]
    fn an_ipv4_proxy_is_recognised_behind_a_mapped_address() {
        let peer: IpAddr = format!("::ffff:{PROXY}").parse().unwrap();
        let trusted = proxies(&[PROXY]);
        let want: IpAddr = "203.0.113.9".parse().unwrap();
        assert_eq!(address(peer, &headers(&["203.0.113.9"]), &trusted), want);
        assert_eq!(address(peer, &headers(&["::ffff:203.0.113.9"]), &trusted), want);
    }

    #[test]
    fn a_missing_or_broken_header_leaves_the_proxy() {
        let peer: IpAddr = PROXY.parse().unwrap();
        let trusted = proxies(&[PROXY]);
        assert_eq!(address(peer, &headers(&[]), &trusted), peer);
        assert_eq!(address(peer, &headers(&["not an address"]), &trusted), peer);
        // Reading stops at the first entry that is not an address.
        assert_eq!(address(peer, &headers(&["203.0.113.9, junk"]), &trusted), peer);
    }

    #[test]
    fn junk_forwarded_entries_are_skipped() {
        let peer: IpAddr = PROXY.parse().unwrap();
        let trusted = proxies(&[PROXY]);
        // Junk to the left of the visitor's entry is never reached.
        assert_eq!(address(peer, &headers(&["nonsense, 203.0.113.9"]), &trusted), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(address(peer, &headers(&["nonsense", "more nonsense"]), &trusted), peer);
    }
}
