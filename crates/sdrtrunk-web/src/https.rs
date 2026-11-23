//! HTTPS: the program's own certificate (made once beside the database), the server that serves it (the certificate
//! swapped in place when Let's Encrypt renews it), and what the Connected hosts page shows of it.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use rustls::sign::CertifiedKey;

use crate::acme::{Certs, Renewal};
use crate::app::Https;

/// What the web UI listens on and serves, for the Connected hosts page.
#[derive(Default)]
pub struct WebStatus {
    pub https: Option<SocketAddr>,
    pub http: Option<SocketAddr>,
    /// "own", "yours" or "Let's Encrypt"; empty without HTTPS.
    pub source: &'static str,
    /// The certificates HTTPS serves (None without HTTPS).
    pub certs: Option<Arc<Certs>>,
    /// Let's Encrypt's last request and its outcome.
    pub renewal: Arc<Mutex<Renewal>>,
}

/// rustls's cryptography (ring), installed once for the process; a second install is harmless.
pub fn provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The program's own certificate in `dir/tls/` (`cert.pem` 0644, `key.pem` 0600): made the first time for this
/// machine's names and addresses, then the same one on every start. Deleting both files makes a new one.
pub fn own_certificate(dir: &Path) -> Result<CertifiedKey, String> {
    provider();
    let tls = dir.join("tls");
    let (cert, key) = (tls.join("cert.pem"), tls.join("key.pem"));
    if !(cert.exists() && key.exists()) {
        std::fs::create_dir_all(&tls).map_err(|e| format!("cannot make {}: {e}", tls.display()))?;
        crate::cert::make(&machine_names(), &cert, &key)?;
        eprintln!("made a certificate for this machine in {}", tls.display());
    }
    crate::cert::certified(&cert, &key)
}

/// The names a browser may use for this machine: its host name, `localhost`, and its addresses (loopback and
/// link-local ones aside, but `127.0.0.1`).
fn machine_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: String| if crate::cert::host_or_address(&n) && !names.contains(&n) { names.push(n) };
    if let Some(host) = host_name() { add(host); }
    add("localhost".into());
    add("127.0.0.1".into());
    for ip in addresses() {
        let link_local = match ip { IpAddr::V4(v) => v.is_link_local(), IpAddr::V6(v) => (v.segments()[0] & 0xffc0) == 0xfe80 };
        if !ip.is_loopback() && !link_local { add(ip.to_string()); }
    }
    names
}

fn host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 { return None; }
    let end = buf.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase()).filter(|h| !h.is_empty())
}

/// Every interface address of this machine (empty if they cannot be read).
fn addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list freed below with freeifaddrs; each `ifa_addr` is read
    // as the socket address type its family names.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 { return out; }
        let mut at = list;
        while !at.is_null() {
            let addr = (*at).ifa_addr;
            if !addr.is_null() {
                match i32::from((*addr).sa_family) {
                    libc::AF_INET => {
                        let v4 = &*(addr as *const libc::sockaddr_in);
                        out.push(IpAddr::from(u32::from_be(v4.sin_addr.s_addr).to_be_bytes()));
                    }
                    libc::AF_INET6 => {
                        let v6 = &*(addr as *const libc::sockaddr_in6);
                        out.push(IpAddr::from(v6.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            at = (*at).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Serves `app` over HTTPS on `listener` (non-blocking) with the certificate `certs` holds, until `signal`; then open
/// connections get `grace` (a live page never ends by itself). Requests carry the `Https` extension.
pub async fn serve_https(listener: std::net::TcpListener, app: Router, certs: Arc<Certs>,
                         signal: impl std::future::Future<Output = ()> + Send + 'static, grace: Duration) -> std::io::Result<()> {
    provider();
    let config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(crate::acme::server_config(certs)));
    let handle = axum_server::Handle::new();
    let stopper = handle.clone();
    tokio::spawn(async move {
        signal.await;
        stopper.graceful_shutdown(Some(grace));
    });
    axum_server::from_tcp_rustls(listener, config)?
        .handle(handle)
        .serve(app.layer(axum::Extension(Https)).into_make_service_with_connect_info::<SocketAddr>())
        .await
}
