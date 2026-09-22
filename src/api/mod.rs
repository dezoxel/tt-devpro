//! The two HTTP clients, and the transport policy both of them inherit.
//!
//! Ports `api/ChronoClient.kt` and `api/TtApiClient.kt`. Neither Kotlin file
//! configures the transport at all — both build a bare `HttpClient(CIO)` with only
//! `ContentNegotiation` installed — and that is the trap this module exists to
//! close: "configures nothing" on Ktor means "takes Ktor's defaults", and Ktor's
//! defaults are not `reqwest`'s. Three of them are live policy here, and each was
//! read out of the jars rather than out of the documentation.
//!
//! **C31 — every request is bounded at 15 s and every connect at 5 s.**
//! `CIOEngineConfig.<init>` sets `requestTimeout = 15000`; `EndpointConfig.<init>`
//! sets `connectTimeout = 5000`, `socketTimeout = Long.MAX_VALUE` and
//! `connectAttempts = 1`. `reqwest` has no default request timeout and no default
//! connect timeout, so the faithful port sets both explicitly. This is the one
//! default whose absence is not benign: it turns a write that fails in fifteen
//! seconds into one that hangs, and the standing operator procedure on that
//! timeout ("verify with `api get-worklogs`, do not retry") begins with it firing.
//!
//! **C31 — nothing is ever retried.** `connectAttempts = 1` and no retry plugin.
//! `reqwest` adds no retries of its own, so the port inherits this by adding
//! nothing — and `tests::the_dependency_set_carries_no_retry_middleware` is what
//! keeps it that way.
//!
//! **Redirects are followed for GET and HEAD only — measured in step 3, and not in
//! any contract before it.** `HttpClientConfig.<init>` sets `followRedirects = true`
//! (`iconst_1`), so `HttpRedirect` is installed; `HttpRedirect$Config.<init>` sets
//! `checkHttpMethod = true`; and `HttpRedirectKt.<clinit>` builds
//! `ALLOWED_FOR_REDIRECT` from exactly `HttpMethod.Get` and `HttpMethod.Head`. So a
//! POST or DELETE that draws a 302 is handed back to the caller *as a 302*.
//! `reqwest` follows redirects for every method by default, turning a 302 on
//! `POST /worklog/create` into a GET of the login page — which answers 200, which
//! makes [`portal::TtApiClient::create_worklog`] report a worklog that was never
//! written. An expired session cookie answering 302 instead of 401 is exactly the
//! shape that produces it. `reqwest`'s redirect policy cannot see the request
//! method (`redirect::Attempt` exposes `status`, `url` and `previous`, and nothing
//! else), so the split is made by building two clients instead of one: reads follow,
//! writes do not.
//!
//! **HTTP/1.1 only.** Ktor CIO speaks 1.1 and nothing else; `reqwest` negotiates
//! HTTP/2 over TLS through ALPN by default. Nothing in this tool's traffic is known
//! to depend on the version, which is the point — the incumbent is proven working
//! against this portal over 1.1, and `.http1_only()` costs one line to keep the
//! wire identical rather than merely expected to behave the same.

pub mod chrono;
pub mod portal;

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, redirect};

/// Ktor CIO 2.3.7's `CIOEngineConfig.requestTimeout`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Ktor CIO 2.3.7's `EndpointConfig.connectTimeout`.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The one construction point for both clients.
///
/// `follow_redirects` is the `ALLOWED_FOR_REDIRECT` split described in the module
/// doc: `true` for a client that only ever issues GET, `false` for one that writes.
/// The limit of 10 is `reqwest`'s own default, stated rather than inherited so that
/// the line reads as a policy; Ktor's `handleCall` loops without a stated maximum,
/// and a portal that redirects more than ten times is not a case either side is
/// engineered for.
pub(crate) fn build_client(
    request_timeout: Duration,
    connect_timeout: Duration,
    follow_redirects: bool,
) -> Result<Client> {
    let redirect_policy = if follow_redirects {
        redirect::Policy::limited(10)
    } else {
        redirect::Policy::none()
    };

    Client::builder()
        .timeout(request_timeout)
        .connect_timeout(connect_timeout)
        .redirect(redirect_policy)
        .http1_only()
        .build()
        .context("building the HTTP client")
}

/// A hand-rolled HTTP/1.1 origin server, so that the parts of a request no pure
/// function can see — the query string, the `Cookie` header, the exact bytes of a
/// POST body — have an oracle that does not need the live portal.
///
/// The dependency set is closed (`## Stack` in the plan), so this is `std::net` and
/// a thread rather than a mock-HTTP crate. Every canned response carries
/// `Connection: close`, which keeps one request to one connection and makes the
/// captured order the request order.
#[cfg(test)]
pub(crate) mod stub {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    /// What the server saw, in the order it saw it.
    #[derive(Debug, Clone)]
    pub(crate) struct CapturedRequest {
        pub method: String,
        /// Path and query exactly as they arrived on the request line.
        pub target: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl CapturedRequest {
        /// Header lookup is ASCII-case-insensitive, as HTTP field names are.
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    pub(crate) struct StubServer {
        pub base_url: String,
        handle: Option<JoinHandle<()>>,
        /// Written by the server thread as each request lands, so that a test can
        /// read what arrived without first waiting for every canned response to be
        /// consumed. `seen` needs that; `requests` does not.
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    /// `HTTP/1.1 200 OK` with a JSON body.
    pub(crate) fn json_200(body: &str) -> String {
        response(200, "OK", "application/json", body)
    }

    /// A 302 pointing somewhere else, for the `ALLOWED_FOR_REDIRECT` split.
    pub(crate) fn redirect_302(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    /// A status line with an explicit reason phrase and a plain-text body.
    pub(crate) fn response(status: u16, reason: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    impl StubServer {
        /// Serves exactly `responses.len()` requests, then stops.
        pub fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub server");
            let port = listener.local_addr().expect("stub server port").port();

            let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&captured);

            let handle = std::thread::spawn(move || {
                for canned in responses {
                    let (mut stream, _) = listener.accept().expect("accept");
                    let request = read_request(&mut stream);
                    stream
                        .write_all(canned.as_bytes())
                        .expect("write the canned response");
                    stream.flush().ok();
                    sink.lock().expect("the capture lock").push(request);
                }
            });

            Self {
                base_url: format!("http://127.0.0.1:{port}"),
                handle: Some(handle),
                captured,
            }
        }

        /// Joins the server thread and returns everything it saw. Panics if the
        /// client sent fewer requests than the server was told to serve, which is
        /// the failure mode worth failing on.
        pub fn requests(mut self) -> Vec<CapturedRequest> {
            self.handle
                .take()
                .expect("the server was already joined")
                .join()
                .expect("the stub server thread panicked");
            self.captured.lock().expect("the capture lock").clone()
        }

        /// Everything that has arrived so far, without joining. This is for the test
        /// that cans *more* responses than a correct client will ask for: `requests`
        /// would block forever on the response nobody fetches, and the surplus canned
        /// response is the whole instrument — if it is ever consumed, the client did
        /// something it must not do. The server thread is left parked on `accept`
        /// and ends with the test process, the same bargain `start_black_hole` makes.
        pub fn seen(&self) -> Vec<CapturedRequest> {
            self.captured.lock().expect("the capture lock").clone()
        }
    }

    /// A listener that accepts and then says nothing, so that a request timeout is
    /// the only thing that can end the call. The thread is detached and holds each
    /// connection for a few seconds: long enough to outlast any timeout a test
    /// configures, short enough not to outlive the run.
    pub(crate) fn start_black_hole() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the black hole");
        let port = listener.local_addr().expect("black hole port").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(stream) = stream else { continue };
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    drop(stream);
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn read_request(stream: &mut std::net::TcpStream) -> CapturedRequest {
        let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));

        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header line");
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }

        let length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("request body");

        CapturedRequest {
            method,
            target,
            headers,
            body: String::from_utf8(body).expect("a UTF-8 request body"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C31's second row. The bound itself is unreachable from a parity run against a
    /// healthy portal, and `reqwest` exposes no getter for a built client's
    /// timeouts, so the values are pinned here and the *wiring* is pinned by the
    /// behavioural timeout tests in `chrono` and `portal`, which drive
    /// `with_timeouts` through the real request path.
    #[test]
    fn the_two_bounds_are_ktor_cios_and_not_reqwests_absence_of_one() {
        assert_eq!(
            REQUEST_TIMEOUT,
            Duration::from_secs(15),
            "CIOEngineConfig.<init> puts requestTimeout at 15000 ms"
        );
        assert_eq!(
            CONNECT_TIMEOUT,
            Duration::from_secs(5),
            "EndpointConfig.<init> puts connectTimeout at 5000 ms"
        );
    }

    /// C31's fourth row, which ports by adding nothing — and so has nothing in the
    /// source to defend it. The manifest is the artefact that can change, so the
    /// manifest is what this reads. `connectAttempts = 1` is the incumbent's
    /// guarantee that a POST of unknown outcome is never sent twice, and one worklog
    /// becoming two is the failure it prevents.
    #[test]
    fn the_dependency_set_carries_no_retry_middleware() {
        let manifest = include_str!("../../Cargo.toml");
        for crate_name in [
            "reqwest-retry",
            "reqwest-middleware",
            "tower-http",
            "backoff",
            "again",
            "retry",
        ] {
            assert!(
                !manifest.contains(crate_name),
                "`{crate_name}` is in Cargo.toml; C31 says a write is never retried"
            );
        }
    }

    /// Reads follow redirects and writes do not. Nothing about a built `Client`
    /// reports its policy, so this asserts the only thing that is observable: that
    /// the two arms build, and that they are reached through one function rather
    /// than two hand-configured builders that can drift apart.
    #[test]
    fn both_redirect_arms_build_a_client() {
        let following = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT, true);
        let not_following = build_client(REQUEST_TIMEOUT, CONNECT_TIMEOUT, false);
        assert!(following.is_ok(), "the read client must build");
        assert!(not_following.is_ok(), "the write client must build");
    }
}
