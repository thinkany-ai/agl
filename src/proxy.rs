//! Dialing the hub through an HTTP proxy (CONNECT), for machines that only
//! reach the internet that way. Honours HTTPS_PROXY / HTTP_PROXY / ALL_PROXY
//! and NO_PROXY, from the environment or the user's login shell.

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::shellenv;

pub struct Target {
    pub host: String,
    pub port: u16,
    pub secure: bool,
}

/// Host, port and scheme of a ws(s):// or http(s):// URL.
pub fn target(url: &str) -> Result<Target> {
    let (secure, rest) = match url.split_once("://") {
        Some(("wss" | "https", r)) => (true, r),
        Some(("ws" | "http", r)) => (false, r),
        _ => bail!("not a ws(s) or http(s) url: {url}"),
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let (host, port) = match authority.strip_prefix('[') {
        // [ipv6]:port
        Some(v6) => {
            let (h, p) = v6.split_once(']').unwrap_or((v6, ""));
            (h.to_string(), p.strip_prefix(':').and_then(|p| p.parse().ok()))
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()),
            None => (authority.to_string(), None),
        },
    };
    Ok(Target { port: port.unwrap_or(if secure { 443 } else { 80 }), host, secure })
}

/// The proxy to use for this target, if any.
pub fn proxy_for(t: &Target) -> Option<String> {
    let get = |names: &[&str]| names.iter().find_map(|n| shellenv::var(n));
    let proxy = if t.secure { get(&["HTTPS_PROXY", "https_proxy"]) } else { get(&["HTTP_PROXY", "http_proxy"]) }
        .or_else(|| get(&["ALL_PROXY", "all_proxy"]))?;
    if bypass(&t.host, get(&["NO_PROXY", "no_proxy"]).as_deref().unwrap_or("")) {
        return None;
    }
    Some(proxy)
}

fn bypass(host: &str, no_proxy: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host == "::1" || host.starts_with("127.") {
        return true;
    }
    no_proxy.split(',').map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).any(|e| {
        if e == "*" {
            return true;
        }
        let e = e.split(':').next().unwrap_or_default().trim_start_matches('*').trim_start_matches('.');
        host == e || host.ends_with(&format!(".{e}"))
    })
}

/// Open a tunnel to `t` through an HTTP proxy (`http://[user:pass@]host:port`).
pub async fn tunnel(proxy: &str, t: &Target) -> Result<TcpStream> {
    let rest = match proxy.split_once("://") {
        Some(("http", r)) => r,
        Some((other, _)) => bail!("unsupported proxy scheme `{other}` (only http:// proxies)"),
        None => proxy,
    };
    let rest = rest.trim_end_matches('/');
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let hostport = if hostport.contains(':') { hostport.to_string() } else { format!("{hostport}:80") };
    let mut stream = TcpStream::connect(&hostport).await.with_context(|| format!("connect to proxy {hostport}"))?;
    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n", host = t.host, port = t.port);
    if let Some(a) = auth {
        req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64(&percent_decode(a))));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    // Read the proxy's answer up to the blank line.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 || head.len() > 16 * 1024 {
            bail!("proxy {hostport} closed the connection");
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let line = status.lines().next().unwrap_or_default();
    if line.split_whitespace().nth(1) != Some("200") {
        bail!("proxy {hostport} refused the tunnel: {line}");
    }
    Ok(stream)
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for (i, shift) in [18, 12, 6, 0].iter().enumerate() {
            if i <= c.len() {
                out.push(T[(n >> shift & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        let t = target("wss://hub.example.com/agl/ws/machine?token=x").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.secure), ("hub.example.com", 443, true));
        let t = target("ws://10.0.0.2:8787/ws/machine").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.secure), ("10.0.0.2", 8787, false));
        assert_eq!(target("ws://[::1]:9/x").unwrap().host, "::1");
        let t = target("https://app.example.com/agl/machine/poll").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.secure), ("app.example.com", 443, true));
    }

    #[test]
    fn no_proxy_rules() {
        assert!(bypass("localhost", ""));
        assert!(bypass("api.corp.example", "corp.example"));
        assert!(bypass("api.corp.example", ".corp.example, other"));
        assert!(bypass("anything", "*"));
        assert!(!bypass("hub.example.com", "corp.example"));
        assert!(!bypass("notcorp.example", "corp.example"));
    }

    #[test]
    fn basic_auth_encoding() {
        assert_eq!(base64(b"user:p@ss"), "dXNlcjpwQHNz");
        assert_eq!(base64(&percent_decode("user:p%40ss")), "dXNlcjpwQHNz");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
