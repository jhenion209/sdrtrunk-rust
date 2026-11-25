//! Where the web UI listens and with which certificate: the `SDRTRUNK_*` variables of the settings file
//! (`/etc/sdrtrunk-rust/sdrtrunk-rust.env`, read by the unit). HTTPS on `0.0.0.0:443` with no settings at all.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::acme;

/// The default HTTPS address: every interface, port 443.
pub const DEFAULT_HTTPS: &str = "0.0.0.0:443";

/// What the web UI serves, and where.
#[derive(Debug)]
pub struct WebConfig {
    /// HTTPS address; None when `SDRTRUNK_HTTPS=off`.
    pub https: Option<SocketAddr>,
    /// Plain HTTP address; None unless `SDRTRUNK_HTTP` is set.
    pub http: Option<SocketAddr>,
    pub cert: CertSource,
    /// Proxies whose `X-Forwarded-For` and `X-Forwarded-Proto` are believed.
    pub trusted: Vec<Proxy>,
}

/// Which certificate HTTPS serves.
#[derive(Debug)]
pub enum CertSource {
    /// The program's own, made once beside the database.
    Own,
    /// The owner's PEM certificate (with its chain) and key.
    Files { cert: PathBuf, key: PathBuf },
    /// Let's Encrypt's, got and renewed by the program.
    Acme(acme::Settings),
}

impl WebConfig {
    /// From the settings file's variables (`get`). Values are trimmed; an empty one counts as unset.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<WebConfig, String> {
        let get = |name: &str| get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let address = |name: &str, value: &str| {
            value.parse::<SocketAddr>().map_err(|_| format!("{name} is {value:?}: give an address and port, such as 0.0.0.0:443"))
        };
        let https = match get("SDRTRUNK_HTTPS") {
            Some(v) if v.eq_ignore_ascii_case("off") => None,
            Some(v) => Some(address("SDRTRUNK_HTTPS", &v)?),
            None => Some(DEFAULT_HTTPS.parse().expect("default address")),
        };
        let http = get("SDRTRUNK_HTTP").map(|v| address("SDRTRUNK_HTTP", &v)).transpose()?;
        if https.is_none() && http.is_none() {
            return Err("SDRTRUNK_HTTPS is off and SDRTRUNK_HTTP is not set: nothing would serve the web UI".into());
        }
        if let (Some(s), Some(p)) = (https, http) {
            if s.port() == p.port() {
                return Err(format!("SDRTRUNK_HTTP uses port {}, as SDRTRUNK_HTTPS does: give each its own", p.port()));
            }
        }
        let files = match (get("SDRTRUNK_TLS_CERT"), get("SDRTRUNK_TLS_KEY")) {
            (Some(cert), Some(key)) => Some((PathBuf::from(cert), PathBuf::from(key))),
            (None, None) => None,
            (Some(_), None) => return Err("SDRTRUNK_TLS_CERT is set without SDRTRUNK_TLS_KEY: set both, or neither".into()),
            (None, Some(_)) => return Err("SDRTRUNK_TLS_KEY is set without SDRTRUNK_TLS_CERT: set both, or neither".into()),
        };
        let acme = acme::Settings::read(get)?;
        let cert = match (files, acme) {
            (Some(_), Some(_)) => {
                return Err("SDRTRUNK_ACME is set with SDRTRUNK_TLS_CERT and SDRTRUNK_TLS_KEY: choose Let's Encrypt or your own certificate".into())
            }
            (None, Some(_)) if https.is_none() => {
                return Err("SDRTRUNK_ACME is set with SDRTRUNK_HTTPS=off: Let's Encrypt's certificate is for HTTPS".into())
            }
            (Some((cert, key)), None) => CertSource::Files { cert, key },
            (None, Some(a)) => CertSource::Acme(a),
            (None, None) => CertSource::Own,
        };
        let trusted = get("SDRTRUNK_TRUSTED_PROXIES")
            .map(|list| {
                list.split(',').map(str::trim).filter(|e| !e.is_empty())
                    .map(|e| Proxy::parse(e).map_err(|why| format!("SDRTRUNK_TRUSTED_PROXIES: {why}")))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(WebConfig { https, http, cert, trusted })
    }

    pub fn from_env() -> Result<WebConfig, String> {
        WebConfig::read(|name| std::env::var(name).ok())
    }
}

/// A trusted proxy: one address, or a range written `address/prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proxy {
    net: IpAddr,
    prefix: u8,
}

impl Proxy {
    pub fn parse(text: &str) -> Result<Proxy, String> {
        let bad = || format!("{text:?} is not an address or a range such as 192.168.104.0/24");
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p.parse::<u8>().map_err(|_| bad())?)),
            None => (text, None),
        };
        let net = addr.parse::<IpAddr>().map_err(|_| bad())?.to_canonical();
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max { return Err(bad()); }
        Ok(Proxy { net, prefix })
    }

    /// True when `ip` is this address, or inside this range. An IPv4-mapped IPv6 address counts as IPv4.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let bits = |a: IpAddr| match a { IpAddr::V4(v) => u128::from(u32::from(v)) << 96, IpAddr::V6(v) => u128::from(v) };
        let ip = ip.to_canonical();
        if ip.is_ipv4() != self.net.is_ipv4() { return false; }
        let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(self.prefix)) };
        bits(ip) & mask == bits(self.net) & mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<WebConfig, String> {
        WebConfig::read(|name| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn defaults_are_https_on_443_and_no_http() {
        let c = read(&[]).unwrap();
        assert_eq!(c.https, Some("0.0.0.0:443".parse().unwrap()));
        assert_eq!(c.http, None);
        assert!(matches!(c.cert, CertSource::Own));
        assert!(c.trusted.is_empty());
    }

    #[test]
    fn https_off_and_http_on() {
        let c = read(&[("SDRTRUNK_HTTPS", "off"), ("SDRTRUNK_HTTP", "127.0.0.1:8080")]).unwrap();
        assert_eq!(c.https, None);
        assert_eq!(c.http, Some("127.0.0.1:8080".parse().unwrap()));
    }

    #[test]
    fn the_owners_pair_and_lets_encrypt_are_read() {
        let c = read(&[("SDRTRUNK_TLS_CERT", "/c.pem"), ("SDRTRUNK_TLS_KEY", "/c.key")]).unwrap();
        assert!(matches!(c.cert, CertSource::Files { ref cert, ref key } if cert.as_os_str() == "/c.pem" && key.as_os_str() == "/c.key"));
        let c = read(&[("SDRTRUNK_ACME", "web"), ("SDRTRUNK_ACME_NAMES", "radio.example.org")]).unwrap();
        assert!(matches!(c.cert, CertSource::Acme(_)));
    }

    #[test]
    fn refusals_name_the_variable() {
        let refused = |pairs: &[(&str, &str)], variable: &str| {
            let e = read(pairs).err().unwrap_or_else(|| panic!("{pairs:?} accepted"));
            assert!(e.contains(variable), "{pairs:?}: {e}");
        };
        refused(&[("SDRTRUNK_HTTPS", "off")], "SDRTRUNK_HTTP");
        refused(&[("SDRTRUNK_TLS_CERT", "/c.pem")], "SDRTRUNK_TLS_KEY");
        refused(&[("SDRTRUNK_TLS_KEY", "/c.key")], "SDRTRUNK_TLS_CERT");
        refused(&[("SDRTRUNK_TLS_CERT", "/c.pem"), ("SDRTRUNK_TLS_KEY", "/c.key"), ("SDRTRUNK_ACME", "web"),
                  ("SDRTRUNK_ACME_NAMES", "radio.example.org")], "SDRTRUNK_ACME");
        refused(&[("SDRTRUNK_ACME", "web")], "SDRTRUNK_ACME_NAMES");
        refused(&[("SDRTRUNK_ACME", "dns-cloudflare"), ("SDRTRUNK_ACME_NAMES", "radio.example.org")], "SDRTRUNK_CLOUDFLARE_TOKEN");
        refused(&[("SDRTRUNK_HTTPS", "off"), ("SDRTRUNK_HTTP", "0.0.0.0:8080"), ("SDRTRUNK_ACME", "web"),
                  ("SDRTRUNK_ACME_NAMES", "radio.example.org")], "SDRTRUNK_ACME");
        refused(&[("SDRTRUNK_HTTP", "nonsense")], "SDRTRUNK_HTTP");
        refused(&[("SDRTRUNK_HTTPS", "nonsense")], "SDRTRUNK_HTTPS");
        refused(&[("SDRTRUNK_TRUSTED_PROXIES", "10.0.0.1,bad")], "SDRTRUNK_TRUSTED_PROXIES");
        refused(&[("SDRTRUNK_HTTPS", "0.0.0.0:8443"), ("SDRTRUNK_HTTP", "127.0.0.1:8443")], "SDRTRUNK_HTTP");
    }

    #[test]
    fn empty_values_count_as_unset() {
        let c = read(&[("SDRTRUNK_HTTP", " "), ("SDRTRUNK_TRUSTED_PROXIES", "")]).unwrap();
        assert_eq!(c.http, None);
        assert!(c.trusted.is_empty());
    }

    #[test]
    fn proxy_ranges() {
        let range = Proxy::parse("192.168.104.0/24").unwrap();
        assert!(range.contains("192.168.104.7".parse().unwrap()));
        assert!(!range.contains("192.168.105.7".parse().unwrap()));
        assert!(range.contains("::ffff:192.168.104.7".parse().unwrap()), "an IPv4-mapped address is IPv4");
        let one = Proxy::parse("10.0.0.1").unwrap();
        assert!(one.contains("10.0.0.1".parse().unwrap()));
        assert!(!one.contains("10.0.0.2".parse().unwrap()));
        let v6 = Proxy::parse("fd00::/8").unwrap();
        assert!(v6.contains("fd12::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));
        for bad in ["10.0.0.0/33", "fd00::/129", "10.0.0.0/x", "host.example"] {
            assert!(Proxy::parse(bad).is_err(), "{bad}");
        }
    }
}
