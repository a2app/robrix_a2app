//! Stateless HTTP through the host permission and information-flow boundaries.
//!
//! A client belongs to exactly one approved request. Redirects and automatic
//! retries are disabled so neither can create another, unchecked transmission.

use std::{collections::BTreeMap, net::{IpAddr, SocketAddr}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};
use a2app_core::{information_flow::{self as flow, ContextId, Influence, Recipient, SensitiveAction}, permissions::PermissionStore};
use matrix_sdk::reqwest::{self, header::{HeaderMap, HeaderName, HeaderValue}, Method};
use serde::Deserialize;
use url::Url;

const MAX_URL: usize = 8192;
const MAX_REQUEST_BODY: usize = 256 * 1024;
const MAX_RESPONSE_BODY: usize = 1024 * 1024;
const MAX_HEADERS: usize = 32 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);
static REQUESTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

pub struct Request {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<String>,
}

impl Request {
    fn sensitive_action(&self) -> Result<Option<SensitiveAction>, String> {
        if self.method == Method::GET || self.method == Method::HEAD { return Ok(None); }
        let Recipient::NetworkOrigin(origin) = Recipient::network_origin(self.url.as_str())? else { unreachable!() };
        Ok(Some(SensitiveAction { kind: format!("network.{}", self.method), target: origin }))
    }

    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        let args: Arguments = serde_json::from_value(value.clone())
            .map_err(|_| "HTTP requests accept url, method, string headers, and an optional text body.".to_string())?;
        if args.url.len() > MAX_URL || args.body.as_ref().is_some_and(|body| body.len() > MAX_REQUEST_BODY) {
            return Err("The HTTP request exceeds its size limit.".into());
        }
        let mut url = Url::parse(&args.url).map_err(|_| "Invalid HTTP URL.".to_string())?;
        Recipient::network_origin(url.as_str())?;
        url.set_fragment(None);
        let method = match args.method.as_deref().unwrap_or("GET") {
            "GET" => Method::GET, "HEAD" => Method::HEAD, "POST" => Method::POST,
            "PUT" => Method::PUT, "PATCH" => Method::PATCH, "DELETE" => Method::DELETE,
            "OPTIONS" => Method::OPTIONS,
            _ => return Err("Unsupported HTTP method.".into()),
        };
        let mut headers = HeaderMap::new();
        let mut size = 0;
        if args.headers.len() > 64 { return Err("Too many HTTP headers.".into()); }
        for (name, value) in args.headers {
            size += name.len() + value.len();
            if size > MAX_HEADERS { return Err("HTTP headers exceed their size limit.".into()); }
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| "Invalid HTTP header name.".to_string())?;
            if matches!(name.as_str(), "authorization" | "cookie" | "proxy-authorization" | "host"
                | "connection" | "proxy-connection" | "transfer-encoding" | "content-length"
                | "upgrade" | "te" | "trailer" | "accept-encoding")
            {
                return Err("Authorization, cookie, and transport-controlled HTTP headers are not supported.".into());
            }
            let value = HeaderValue::from_str(&value).map_err(|_| "Invalid HTTP header value.".to_string())?;
            headers.insert(name, value);
        }
        headers.insert(reqwest::header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        Ok(Self { url, method, headers, body: args.body })
    }
}

pub async fn run(
    request: Request,
    context: ContextId,
    subject: String,
    origin_room: Option<String>,
    consent: PermissionStore,
    lifetime: Option<Arc<AtomicBool>>,
) -> Result<String, String> {
    if lifetime.as_ref().is_some_and(|alive| !alive.load(Ordering::Acquire)) {
        return Err("This mini-app instance is no longer running.".into());
    }
    let epoch = flow::context_epoch(&context)?;
    let authorize = || {
        if lifetime.as_ref().is_some_and(|alive| !alive.load(Ordering::Acquire)) {
            return Err("This mini-app instance is no longer running.".into());
        }
        super::information_flow::current_context(&context)?;
        let recipient = Recipient::network_origin(request.url.as_str())?;
        flow::ensure_allowed_for_activation(&context, epoch, &recipient)?;
        if let Some(action) = request.sensitive_action()? {
            flow::ensure_action_allowed_for_activation(&context, epoch, &action)?;
        }
        if !super::matrix::policy::network_allowed(&subject, origin_room.as_deref(), request.url.as_str(), &consent) {
            return Err("Internet permission is no longer granted for this request.".into());
        }
        Ok(())
    };
    // Even the hostname can encode private content. Check before DNS or queueing.
    authorize()?;
    let Recipient::NetworkOrigin(origin) = Recipient::network_origin(request.url.as_str())? else { unreachable!() };
    // DNS success, failure and timing are outside inputs too. Record the
    // influence before any lookup; a write may now need explicit action review.
    flow::add_influences_for_activation(&context, epoch, [Influence::InternetOrigin(origin)])?;
    authorize()?;
    let _permit = REQUESTS.try_acquire().map_err(|_| "Too many active mini-app HTTP requests.".to_string())?;
    let addresses = tokio::time::timeout(Duration::from_secs(5), public_addresses(&request.url)).await
        .map_err(|_| "DNS resolution timed out.".to_string())??;
    let response = send(&request, &addresses, &authorize).await;
    authorize()?;
    response
}

async fn public_addresses(url: &Url) -> Result<Vec<SocketAddr>, String> {
    let port = url.port_or_known_default().filter(|port| *port != 0).ok_or("Invalid HTTP port.")?;
    let host = url.host_str().ok_or("The HTTP URL has no hostname.")?;
    let addresses = match url.host().ok_or("The HTTP URL has no hostname.")? {
        url::Host::Ipv4(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Ipv6(ip) => vec![SocketAddr::new(ip.into(), port)],
        url::Host::Domain(_) => {
            let normalized = host.trim_end_matches('.').to_ascii_lowercase();
            if !normalized.contains('.') || ["localhost", "local", "internal", "home.arpa"].iter()
                .any(|suffix| normalized == *suffix || normalized.ends_with(&format!(".{suffix}")))
            {
                return Err("Local and private network destinations are not supported.".into());
            }
            tokio::net::lookup_host((host, port)).await.map_err(|_| "DNS resolution failed.".to_string())?
                .take(17).collect()
        }
    };
    if addresses.is_empty() || addresses.len() > 16 || addresses.iter().any(|address| !is_public(address.ip())) {
        return Err("The destination must resolve exclusively to public internet addresses.".into());
    }
    Ok(addresses)
}

// Conservative subset of globally routable addresses. Special-purpose ranges
// follow the IANA IPv4/IPv6 registries; transition and mapped forms are excluded.
fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_documentation()
                || ip.is_multicast() || a == 0 || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19)))
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            (a & 0xe000) == 0x2000
                && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
                && a != 0x2002
                && !(a == 0x3fff && (b & 0xf000) == 0)
        }
    }
}

// A fresh client can resolve exactly one prechecked name. An unexpected lookup
// fails closed instead of falling back to the system resolver.
struct PinnedResolver {
    host: String,
    addresses: Vec<SocketAddr>,
}

impl reqwest::dns::Resolve for PinnedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let result: Result<reqwest::dns::Addrs, Box<dyn std::error::Error + Send + Sync>> =
            if name.as_str().eq_ignore_ascii_case(&self.host) {
                Ok(Box::new(self.addresses.clone().into_iter()))
            } else {
                Err(Box::new(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Unapproved DNS lookup")))
            };
        Box::pin(std::future::ready(result))
    }
}

async fn send(
    request: &Request,
    addresses: &[SocketAddr],
    authorize: impl Fn() -> Result<(), String>,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .http1_only()
        .no_gzip().no_brotli().no_deflate().no_zstd()
        .connect_timeout(Duration::from_secs(10))
        .timeout(DEADLINE)
        .dns_resolver(Arc::new(PinnedResolver {
            host: request.url.host_str().ok_or("The HTTP URL has no hostname.")?.to_owned(),
            addresses: addresses.to_vec(),
        }))
        .build().map_err(|_| "Could not create the isolated HTTP client.".to_string())?;
    authorize()?;
    let mut outgoing = client.request(request.method.clone(), request.url.clone()).headers(request.headers.clone());
    if let Some(body) = &request.body { outgoing = outgoing.body(body.clone()); }
    let mut response = outgoing.send().await.map_err(|_| "The HTTP request failed.".to_string())?;
    authorize()?;
    if response.status().is_redirection() {
        return Err("Redirects are not followed. Request the destination URL separately.".into());
    }
    if response.content_length().is_some_and(|length| length > MAX_RESPONSE_BODY as u64) {
        return Err("The HTTP response exceeds its size limit.".into());
    }
    let status = response.status().as_u16();
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    let mut header_size = 0;
    for (name, value) in response.headers() {
        header_size += name.as_str().len() + value.as_bytes().len();
        if header_size > MAX_HEADERS { return Err("HTTP response headers exceed their size limit.".into()); }
        if name == reqwest::header::SET_COOKIE { continue; }
        if let Ok(value) = value.to_str() { headers.entry(name.to_string()).or_default().push(value.to_owned()); }
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "Reading the HTTP response failed.".to_string())? {
        authorize()?;
        if chunk.len() > MAX_RESPONSE_BODY - bytes.len() { return Err("The HTTP response exceeds its size limit.".into()); }
        bytes.extend_from_slice(&chunk);
    }
    authorize()?;
    let body = String::from_utf8(bytes).map_err(|_| "Only UTF-8 text HTTP responses are currently supported.".to_string())?;
    serde_json::to_string(&serde_json::json!({"status":status,"headers":headers,"body":body}))
        .map_err(|_| "Could not encode the HTTP response.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_reject_ambient_credentials_and_transport_overrides() {
        for url in ["file:///private", "https://user:secret@example.com/", "ftp://example.com/"] {
            assert!(Request::parse(&serde_json::json!({"url":url})).is_err());
        }
        for header in ["Authorization", "Cookie", "Host", "Content-Length", "Proxy-Authorization", "Connection"] {
            assert!(Request::parse(&serde_json::json!({"url":"https://example.com/","headers":{header:"private"}})).is_err());
        }
        assert!(Request::parse(&serde_json::json!({"url":"https://example.com/","method":"CONNECT"})).is_err());
        assert!(Request::parse(&serde_json::json!({"url":"https://example.com/","body":"x".repeat(MAX_REQUEST_BODY+1)})).is_err());
        assert!(Request::parse(&serde_json::json!({"url":"https://example.com/","headers":{"x-test":"a\r\nb: c"}})).is_err());
    }

    #[test]
    fn sensitive_http_authority_binds_method_and_canonical_origin() {
        for method in ["GET", "HEAD"] {
            let request = Request::parse(&serde_json::json!({"url":"https://EXAMPLE.com:443/path","method":method})).unwrap();
            assert_eq!(request.sensitive_action().unwrap(), None);
        }
        for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            let request = Request::parse(&serde_json::json!({"url":"https://EXAMPLE.com:443/path?private=value","method":method})).unwrap();
            assert_eq!(request.sensitive_action().unwrap(), Some(SensitiveAction { kind: format!("network.{method}"), target: "https://example.com".into() }));
        }
    }

    #[test]
    fn address_filter_rejects_private_mapped_and_transition_ranges() {
        for ip in ["127.0.0.1","10.0.0.1","172.16.0.1","192.168.0.1","169.254.169.254","100.64.1.1",
            "198.18.0.1","192.0.0.8","192.0.2.1","224.0.0.1","255.255.255.255","0.0.0.1",
            "::1","::ffff:127.0.0.1","::ffff:8.8.8.8","64:ff9b::a00:1","fe80::1","fc00::1",
            "2001:db8::1","2002:7f00:1::","2001::1","3fff::1"]
        { assert!(!is_public(ip.parse().unwrap()), "accepted {ip}"); }
        for ip in ["8.8.8.8","1.1.1.1","2606:4700:4700::1111","2001:4860:4860::8888"] {
            assert!(is_public(ip.parse().unwrap()), "rejected {ip}");
        }
    }

    #[tokio::test]
    async fn literal_and_local_destinations_fail_before_connecting() {
        for url in ["http://127.0.0.1/","http://[::1]/","https://device.local/","http://localhost/","http://printer/"] {
            assert!(public_addresses(&Url::parse(url).unwrap()).await.is_err());
        }
    }

    fn server(response: impl Into<String>) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let response = response.into();
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bytes = [0; 4096];
            let read = stream.read(&mut bytes).unwrap();
            let request = std::str::from_utf8(&bytes[..read]).unwrap();
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            assert!(!request.to_ascii_lowercase().contains("cookie:"));
            let _ = stream.write_all(response.as_bytes());
        });
        (address, thread)
    }

    #[tokio::test]
    async fn pinned_transport_returns_text_without_following_redirects() {
        let (address, thread) = server("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/leak\r\nContent-Length: 0\r\n\r\n");
        let request = Request::parse(&serde_json::json!({"url":format!("http://example.invalid:{}/",address.port())})).unwrap();
        assert!(send(&request, &[address], || Ok(())).await.unwrap_err().contains("Redirects"));
        thread.join().unwrap();
        let (address, thread) = server("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: hidden=secret\r\n\r\nhello");
        let request = Request::parse(&serde_json::json!({"url":format!("http://example.invalid:{}/",address.port())})).unwrap();
        let response: serde_json::Value = serde_json::from_str(&send(&request, &[address], || Ok(())).await.unwrap()).unwrap();
        assert_eq!(response["body"], "hello");
        assert!(response["headers"].get("set-cookie").is_none());
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn transport_rechecks_revocation_before_delivering_a_response() {
        let (address, thread) = server("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello");
        let request = Request::parse(&serde_json::json!({"url":format!("http://example.invalid:{}/",address.port())})).unwrap();
        let calls = std::cell::Cell::new(0);
        let result = send(&request, &[address], || {
            calls.set(calls.get()+1);
            if calls.get() > 1 { Err("revoked".into()) } else { Ok(()) }
        }).await;
        assert_eq!(result.unwrap_err(), "revoked");
        thread.join().unwrap();
    }
    #[tokio::test]
    async fn denied_request_does_not_connect_and_dead_instance_stops_before_dns() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let request = Request::parse(&serde_json::json!({"url":format!("http://example.invalid:{}/",address.port())})).unwrap();
        assert_eq!(send(&request, &[address], || Err("blocked".into())).await.unwrap_err(), "blocked");
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        let context = ContextId::App { account: "test".into(), app: "test".into(), room: None };
        let dead = Arc::new(AtomicBool::new(false));
        assert!(run(request, context, "test".into(), None, PermissionStore::default(), Some(dead)).await
            .unwrap_err().contains("no longer running"));
    }

    #[tokio::test]
    async fn response_body_limit_applies_without_a_content_length() {
        let body = "x".repeat(MAX_RESPONSE_BODY+1);
        let (address, thread) = server(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body));
        let request = Request::parse(&serde_json::json!({"url":format!("http://example.invalid:{}/",address.port())})).unwrap();
        assert!(send(&request, &[address], || Ok(())).await.unwrap_err().contains("size limit"));
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn pinned_resolver_has_no_system_dns_fallback() {
        use reqwest::dns::Resolve;
        let address: SocketAddr = "1.1.1.1:443".parse().unwrap();
        let resolver = PinnedResolver { host: "example.com".into(), addresses: vec![address] };
        let addresses = resolver.resolve("example.com".parse().unwrap()).await.unwrap().collect::<Vec<_>>();
        assert_eq!(addresses, vec![address]);
        assert!(resolver.resolve("other.example.com".parse().unwrap()).await.is_err());
    }

}
