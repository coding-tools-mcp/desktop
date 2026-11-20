//! Loopback authentication gateway for clients that cannot send a bearer header.
//!
//! The external core only understands `Authorization: Bearer`. For the
//! `url-token` and `basic` workspace modes the desktop keeps that core on a
//! private loopback port and answers on the workspace port itself: it checks
//! the client's credential, then forwards the request with the core's bearer
//! token. Requests it does not accept are never given that token, so the core
//! still decides what is public. It is not an MCP server and parses no bodies.
use crate::model::{Secrets, Workspace};
use base64::Engine;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::{
    body::{Bytes, Frame, Incoming, SizeHint},
    header::{self, HeaderMap, HeaderName, HeaderValue},
    Method, Request, Response, StatusCode, Uri, Version,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use std::{
    convert::Infallible,
    future::Future,
    net::{Ipv4Addr, TcpListener},
    pin::Pin,
    sync::Arc,
    task::Poll,
    thread::JoinHandle,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::{oneshot, Semaphore};

pub const TOKEN_PARAM: &str = "token";
pub const DEFAULT_USERNAME: &str = "ctc";
const MAX_CONNECTIONS: usize = 256;
const REALM: &str = "coding-tools-mcp";
const BODY_IDLE: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 30_000 });

/// What a client must present to this gateway.
#[derive(Clone)]
pub enum Credential {
    /// `?token=` in the URL, or the same value as a bearer header.
    UrlToken(String),
    Basic {
        username: String,
        password: String,
    },
}

impl Credential {
    /// `None` for modes the core enforces by itself.
    pub fn for_workspace(w: &Workspace, secrets: &Secrets) -> Option<Self> {
        match w.auth.as_str() {
            "url-token" => Some(Self::UrlToken(secrets.bearer_token.clone())),
            "basic" => Some(Self::Basic {
                username: w.basic_username.clone(),
                password: secrets.basic_password.clone(),
            }),
            _ => None,
        }
    }

    fn accepts(&self, uri: &Uri, headers: &HeaderMap) -> bool {
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().split_once(' '))
            .map(|(scheme, value)| (scheme.to_ascii_lowercase(), value.trim()));
        match self {
            Self::UrlToken(expected) => {
                let from_url = url_token(uri).is_some_and(|token| same(&token, expected));
                let from_header = authorization
                    .is_some_and(|(scheme, value)| scheme == "bearer" && same(value, expected));
                from_url || from_header
            }
            Self::Basic { username, password } => {
                let Some(decoded) = authorization
                    .filter(|(scheme, _)| scheme == "basic")
                    .and_then(|(_, value)| {
                        base64::engine::general_purpose::STANDARD.decode(value).ok()
                    })
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                else {
                    return false;
                };
                let Some((user, pass)) = decoded.split_once(':') else {
                    return false;
                };
                // Evaluate both halves so a wrong username costs the same as a wrong password.
                let user_ok = user.as_bytes().ct_eq(username.as_bytes());
                let pass_ok = pass.as_bytes().ct_eq(password.as_bytes());
                bool::from(user_ok & pass_ok)
            }
        }
    }

    fn challenge(&self) -> String {
        match self {
            Self::UrlToken(_) => format!("Bearer realm=\"{REALM}\""),
            Self::Basic { .. } => format!("Basic realm=\"{REALM}\", charset=\"UTF-8\""),
        }
    }
}

fn same(left: &str, right: &str) -> bool {
    !right.is_empty() && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

fn is_token_pair(pair: &str) -> bool {
    let key = pair.split('=').next().unwrap_or_default();
    url::form_urlencoded::parse(key.as_bytes())
        .next()
        .is_some_and(|(key, _)| key == TOKEN_PARAM)
}

fn url_token(uri: &Uri) -> Option<String> {
    url::form_urlencoded::parse(uri.query()?.as_bytes())
        .find(|(key, _)| key == TOKEN_PARAM)
        .map(|(_, value)| value.into_owned())
}

/// The path and query the core receives: the credential never reaches its logs.
fn upstream_target(uri: &Uri) -> String {
    let query: Vec<&str> = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty() && !is_token_pair(pair))
        .collect();
    if query.is_empty() {
        uri.path().to_owned()
    } else {
        format!("{}?{}", uri.path(), query.join("&"))
    }
}

/// Discovery and CORS preflight carry no credentials. They are forwarded
/// without the core's token, so the core alone decides what they may read.
fn anonymous(method: &Method, uri: &Uri) -> bool {
    method == Method::OPTIONS
        || ((method == Method::GET || method == Method::HEAD)
            && uri.path().starts_with("/.well-known/"))
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse().ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        HeaderName::from_static("keep-alive"),
    ] {
        headers.remove(name);
    }
}

type Body = BoxBody<Bytes, hyper::Error>;

fn message(status: StatusCode, text: &str, challenge: Option<String>) -> Response<Body> {
    let body =
        serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":text}})
            .to_string();
    let mut response = Response::new(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(challenge) = challenge.and_then(|value| HeaderValue::from_str(&value).ok()) {
        headers.insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

/// A request body that fails once the client sends nothing for `BODY_IDLE`,
/// so a stalled upload cannot keep its connection slot.
struct Upload {
    body: Incoming,
    idle: Pin<Box<tokio::time::Sleep>>,
}

impl hyper::body::Body for Upload {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(frame) => {
                let next = tokio::time::Instant::now() + BODY_IDLE;
                self.idle.as_mut().reset(next);
                Poll::Ready(frame.map(|frame| frame.map_err(Into::into)))
            }
            Poll::Pending => self
                .idle
                .as_mut()
                .poll(cx)
                .map(|()| Some(Err("request body stalled".into()))),
        }
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

struct Context {
    upstream: u16,
    credential: Credential,
    bearer: HeaderValue,
    client: Client<HttpConnector, Upload>,
}

async fn forward(
    context: Arc<Context>,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let accepted = context.credential.accepts(request.uri(), request.headers());
    if !accepted && !anonymous(request.method(), request.uri()) {
        return Ok(message(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            Some(context.credential.challenge()),
        ));
    }
    let Ok(target) = format!(
        "http://127.0.0.1:{}{}",
        context.upstream,
        upstream_target(request.uri())
    )
    .parse::<Uri>() else {
        return Ok(message(StatusCode::BAD_REQUEST, "Invalid request", None));
    };
    *request.uri_mut() = target;
    *request.version_mut() = Version::HTTP_11;
    let headers = request.headers_mut();
    strip_hop_by_hop(headers);
    headers.remove(header::AUTHORIZATION);
    if accepted {
        headers.insert(header::AUTHORIZATION, context.bearer.clone());
    }
    let request = request.map(|body| Upload {
        body,
        idle: Box::pin(tokio::time::sleep(BODY_IDLE)),
    });
    Ok(match context.client.request(request).await {
        Ok(upstream) => {
            let (mut parts, body) = upstream.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            let mut response = Response::new(body.boxed());
            *response.status_mut() = parts.status;
            *response.headers_mut() = parts.headers;
            response
        }
        Err(_) => message(
            StatusCode::BAD_GATEWAY,
            "Local MCP core is not reachable",
            None,
        ),
    })
}

async fn serve(listener: tokio::net::TcpListener, context: Arc<Context>) {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(permit) = connections.clone().acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(_) => {
                // Descriptor exhaustion must not spin this thread.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let context = context.clone();
        tokio::spawn(async move {
            let service =
                hyper::service::service_fn(move |request| forward(context.clone(), request));
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30))
                .serve_connection(TokioIo::new(stream), service)
                .await;
            drop(permit);
        });
    }
}

/// Owns the workspace port for as long as it lives; dropping it closes the
/// listener and every open connection before returning.
pub struct Gateway {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Gateway {
    pub fn start(
        port: u16,
        upstream: u16,
        credential: Credential,
        bearer_token: &str,
    ) -> std::io::Result<Self> {
        let invalid = |text: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, text);
        let mut bearer = HeaderValue::from_str(&format!("Bearer {bearer_token}"))
            .map_err(|_| invalid("Bearer credential contains characters a header cannot carry"))?;
        bearer.set_sensitive(true);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        connector.set_nodelay(true);
        let context = Arc::new(Context {
            upstream,
            credential,
            bearer,
            // The core may close after each response; never reuse a stale socket.
            client: Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(0)
                .build(connector),
        });
        let (shutdown, stopped) = oneshot::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("auth-gateway".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                        return;
                    };
                    let accepting = tokio::spawn(serve(listener, context));
                    let _ = stopped.await;
                    accepting.abort();
                    let _ = accepting.await;
                });
                // Dropping the runtime cancels every connection task.
            })?;
        Ok(Self {
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        drop(self.shutdown.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(authorization: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = authorization {
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        }
        headers
    }
    fn uri(text: &str) -> Uri {
        text.parse().unwrap()
    }
    fn basic(pair: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    }

    #[test]
    fn url_token_accepts_the_query_or_the_same_bearer_value_only() {
        let credential = Credential::UrlToken("s3cret+/=".into());
        assert!(credential.accepts(&uri("/mcp?token=s3cret%2B%2F%3D"), &headers(None)));
        assert!(credential.accepts(&uri("/mcp?a=1&token=s3cret%2B%2F%3D"), &headers(None)));
        assert!(credential.accepts(&uri("/mcp"), &headers(Some("Bearer s3cret+/="))));
        assert!(!credential.accepts(&uri("/mcp"), &headers(None)));
        assert!(!credential.accepts(&uri("/mcp?token="), &headers(None)));
        assert!(!credential.accepts(&uri("/mcp?token=s3cret"), &headers(None)));
        assert!(!credential.accepts(&uri("/mcp?Token=s3cret%2B%2F%3D"), &headers(None)));
        assert!(!credential.accepts(&uri("/mcp"), &headers(Some(&basic("mcp:s3cret+/=")))));
        assert!(!Credential::UrlToken(String::new()).accepts(&uri("/mcp?token="), &headers(None)));
    }

    #[test]
    fn basic_requires_both_username_and_password() {
        let credential = Credential::Basic {
            username: "mcp".into(),
            password: "pa:ss wörd".into(),
        };
        assert!(credential.accepts(&uri("/mcp"), &headers(Some(&basic("mcp:pa:ss wörd")))));
        assert!(credential.accepts(
            &uri("/mcp"),
            &headers(Some(&basic("mcp:pa:ss wörd").replace("Basic", "basic")))
        ));
        for wrong in ["mcp:nope", "other:pa:ss wörd", "mcp", ":pa:ss wörd", ""] {
            assert!(
                !credential.accepts(&uri("/mcp"), &headers(Some(&basic(wrong)))),
                "{wrong}"
            );
        }
        assert!(!credential.accepts(&uri("/mcp"), &headers(Some("Basic not-base64!"))));
        assert!(!credential.accepts(
            &uri("/mcp"),
            &headers(Some("Bearer pa:ss wörd".replace('ö', "o").as_str()))
        ));
        assert!(!credential.accepts(&uri("/mcp?token=pa:ss"), &headers(None)));
    }

    #[test]
    fn the_token_is_removed_before_the_core_sees_the_request() {
        assert_eq!(upstream_target(&uri("/mcp?token=abc")), "/mcp");
        assert_eq!(
            upstream_target(&uri("/mcp?a=1&token=abc&b=%20")),
            "/mcp?a=1&b=%20"
        );
        assert_eq!(upstream_target(&uri("/mcp?%74oken=abc&token=x")), "/mcp");
        assert_eq!(upstream_target(&uri("/mcp")), "/mcp");
    }

    #[test]
    fn responses_stream_through_and_stopping_releases_the_port() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::sync::mpsc;

        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (seen, request) = mpsc::channel::<String>();
        let (resume, resumed) = mpsc::channel::<()>();
        let core = std::thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            seen.send(String::from_utf8(head).unwrap()).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n")
                .unwrap();
            resumed.recv().unwrap();
            stream.write_all(b"6\r\nsecond\r\n0\r\n\r\n").unwrap();
        });
        let gateway = Gateway::start(
            port,
            upstream_port,
            Credential::UrlToken("t0ken".into()),
            "t0ken",
        )
        .unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client
            .write_all(b"GET /mcp?token=t0ken HTTP/1.1\r\nHost: example.test\r\nAuthorization: Basic abc\r\n\r\n")
            .unwrap();
        let mut received = String::new();
        let mut read_until = |needle: &str, received: &mut String| {
            let mut buffer = [0u8; 256];
            while !received.contains(needle) {
                let count = client.read(&mut buffer).unwrap();
                assert!(count > 0, "connection closed before {needle}: {received}");
                received.push_str(std::str::from_utf8(&buffer[..count]).unwrap());
            }
        };
        // The first event must arrive while the core is still holding the response open.
        read_until("first", &mut received);
        assert!(!received.contains("second"));
        resume.send(()).unwrap();
        read_until("second", &mut received);
        core.join().unwrap();

        let head = request.recv().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /mcp http/1.1\r\n"), "{head}");
        assert!(head.contains("authorization: bearer t0ken\r\n"));
        assert!(!head.contains("basic"));
        assert!(head.contains("host: example.test\r\n"));

        drop(gateway);
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
    }

    #[test]
    fn a_stalled_upload_gives_its_connection_back() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // A core that waits for the whole body before it answers.
        let core = std::thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        });
        let _gateway = Gateway::start(
            port,
            upstream_port,
            Credential::UrlToken("t0ken".into()),
            "internal",
        )
        .unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        client
            .write_all(b"POST /mcp?token=t0ken HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab")
            .unwrap();
        let mut response = String::new();
        // Reaching end of stream means the gateway closed the connection.
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        core.join().unwrap();
    }

    #[test]
    fn only_discovery_and_preflight_pass_without_a_credential() {
        assert!(anonymous(&Method::GET, &uri("/.well-known/mcp.json")));
        assert!(anonymous(
            &Method::HEAD,
            &uri("/.well-known/oauth-protected-resource")
        ));
        assert!(anonymous(&Method::OPTIONS, &uri("/mcp")));
        assert!(!anonymous(&Method::GET, &uri("/mcp")));
        assert!(!anonymous(&Method::POST, &uri("/.well-known/mcp.json")));
        assert!(!anonymous(&Method::POST, &uri("/mcp")));
    }
}
