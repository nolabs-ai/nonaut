//! Local HTTPS proxy that lets sandboxed tools reach allowlisted hosts.
//!
//! Sandboxed processes may only connect to this proxy's port (nono
//! `proxy_only`). The proxy accepts `CONNECT host:443`, resolves the host
//! once, checks the name and the resolved addresses with nono's
//! `HostFilter` (cloud metadata and link-local are always denied), and then
//! connects to an address it checked, so DNS cannot change between check and
//! connect. Anything else is refused with 403 and a reason.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, OnceLock, PoisonError, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use nono::net_filter::HostFilter;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream, lookup_host},
};

const MAX_REQUEST_HEAD: usize = 8 * 1024;
const ALLOWED_PORT: u16 = 443;
const TIMEOUT: Duration = Duration::from_secs(10);

struct Proxy {
    port: u16,
    filter: Arc<RwLock<HostFilter>>,
}

static PROXY: OnceLock<Proxy> = OnceLock::new();

/// Starts the proxy on first use and applies the current allowlist, so
/// edits to the policy take effect on the next tool call. Returns its port.
pub(crate) fn ensure(allow_hosts: &[String]) -> Result<u16> {
    let filter = HostFilter::new_strict(allow_hosts);
    if let Some(proxy) = PROXY.get() {
        *proxy.filter.write().unwrap_or_else(PoisonError::into_inner) = filter;
        return Ok(proxy.port);
    }
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("could not start the sandbox network proxy")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let listener = TcpListener::from_std(listener)?;
    let filter = Arc::new(RwLock::new(filter));
    let proxy = PROXY.get_or_init(|| Proxy {
        port,
        filter: filter.clone(),
    });
    if proxy.port == port {
        tokio::spawn(serve(listener, filter));
    }
    Ok(proxy.port)
}

async fn serve(listener: TcpListener, filter: Arc<RwLock<HostFilter>>) {
    while let Ok((client, _)) = listener.accept().await {
        let filter = filter.clone();
        tokio::spawn(async move {
            let _ = handle(client, &filter).await;
        });
    }
}

async fn handle(mut client: TcpStream, filter: &RwLock<HostFilter>) -> Result<()> {
    let (head, rest) = tokio::time::timeout(TIMEOUT, read_head(&mut client))
        .await
        .context("request timed out")??;
    let target = match parse_connect(&head) {
        Ok(target) => target,
        Err(reason) => return refuse(&mut client, &reason).await,
    };
    let (host, port) = target;
    if port != ALLOWED_PORT {
        return refuse(&mut client, &format!("only port {ALLOWED_PORT} is allowed")).await;
    }
    let addresses = match lookup_host((host.as_str(), port)).await {
        Ok(addresses) => addresses.collect::<Vec<SocketAddr>>(),
        Err(_) => return refuse(&mut client, &format!("could not resolve {host}")).await,
    };
    let ips = addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>();
    let verdict = filter
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .check_host(&host, &ips);
    if !verdict.is_allowed() {
        return refuse(
            &mut client,
            &format!("{} (edit nonaut config.json)", verdict.reason()),
        )
        .await;
    }
    let Ok(Ok(mut upstream)) =
        tokio::time::timeout(TIMEOUT, TcpStream::connect(&addresses[..])).await
    else {
        return refuse(&mut client, &format!("could not connect to {host}")).await;
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    upstream.write_all(&rest).await?;
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Reads up to the blank line that ends the request head; returns the head
/// and any bytes the client already sent after it.
async fn read_head(client: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        let read = client.read(&mut chunk).await?;
        anyhow::ensure!(read > 0, "client closed the connection");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buffer).into_owned(), rest));
        }
        anyhow::ensure!(buffer.len() <= MAX_REQUEST_HEAD, "request head too large");
    }
}

fn parse_connect(head: &str) -> std::result::Result<(String, u16), String> {
    let line = head.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err("malformed request".to_owned());
    };
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err("only HTTPS (CONNECT) is allowed through the sandbox proxy".to_owned());
    }
    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| "CONNECT target needs host:port".to_owned())?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = port.parse().map_err(|_| "invalid port".to_owned())?;
    if host.is_empty() {
        return Err("empty host".to_owned());
    }
    Ok((host.to_owned(), port))
}

async fn refuse(client: &mut TcpStream, reason: &str) -> Result<()> {
    let body = format!("nonaut sandbox: {reason}\n");
    let response = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(response.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_targets_and_rejects_plain_http() {
        assert_eq!(
            parse_connect("CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io\r\n"),
            Ok(("crates.io".to_owned(), 443))
        );
        assert_eq!(
            parse_connect("CONNECT [::1]:443 HTTP/1.1\r\n"),
            Ok(("::1".to_owned(), 443))
        );
        assert!(parse_connect("GET http://example.com/ HTTP/1.1\r\n").is_err());
        assert!(parse_connect("CONNECT example.com HTTP/1.1\r\n").is_err());
    }
}
