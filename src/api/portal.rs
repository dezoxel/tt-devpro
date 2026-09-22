//! The Dev.Pro Time Tracking Portal client.
//!
//! Ports `api/TtApiClient.kt`. Three reads, three writes, and the two status gates
//! that stand in front of them.
//!
//! **The status gates are pure functions here, and in Kotlin they are not.** C18
//! locks the write path's request body precisely because the write path gets no live
//! coverage — it posts worklogs other people read — and the same argument covers
//! everything the write path does with a *response*. [`classify_read_status`] and
//! [`classify_write_status`] are the two `when` blocks from `TtApiClient.kt:48-66`
//! lifted out whole, so the 404-before-4xx ordering and the two differently shaped
//! messages have an oracle that does not need a portal to be misbehaving.
//!
//! **D5 — the auth instruction goes to stderr.** `TtApiClient.kt:76-77` uses Kotlin's
//! `println`, so on the incumbent an expired cookie during `settle --json` puts two
//! decorated lines on **stdout** in place of the JSON payload. C7 — "stdout stays
//! machine-clean" — is named in the task's own Considerations as a contract that must
//! survive, so the port writes them to stderr. [`report_and_raise`] takes the sink as
//! a parameter; every call site in this file passes `std::io::stderr()`.
//!
//! **`jsonBody` has nothing to port to, and that is the JVM leaving.** The
//! call-site-serializer rule at `TtApiClient.kt:36-46` exists because Ktor resolves a
//! body's serializer reflectively and the native image's `reflect-config.json` can
//! never contain a request class — the tracing run is `settle --dry-run`, which
//! issues no POST. `serde_json::to_string` is monomorphized at compile time, so the
//! hazard the rule guards against cannot arise. The C18 assertions stay; the
//! mechanism they protect is gone.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::{CONNECT_TIMEOUT, REQUEST_TIMEOUT, build_client};
use crate::model::{
    AssignedProjectsResponse, CreateWorklogRequest, CurrentUser, NormalViewResponse,
    UpdateWorklogRequest,
};

/// `TtApiClient.kt:22`.
pub const BASE_URL: &str = "https://timetrackingportal.dev.pro/api";

/// `ApiException` (`TtApiClient.kt:17`). `status_code` is carried because the
/// incumbent carries it; the only caller today (`SettleCommand.kt:105-106`) prints
/// the message alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub status_code: u16,
    pub message: String,
}

/// The two exception types `TtApiClient` throws, before `withAuthCheck` collapses
/// the first into the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// `AuthRequiredException` (`:19`).
    AuthRequired,
    /// `ApiException` (`:17`).
    Api(ApiError),
}

/// `TtApiClient.kt:76-77`, verbatim — including the leading blank line that
/// `println("\n…")` produces, and the U+274C cross mark copied from the source
/// rather than retyped.
const AUTH_FAILURE_NOTICE: &str = "\n❌ Dev.Pro Time Tracking Portal session expired or invalid.\n   Run 'make auth' on your host machine to refresh the session.\n";

/// `TtApiClient.kt:78`. The dash is U+2014.
const AUTH_FAILURE_MESSAGE: &str =
    "Authentication failed. Session cookie expired — run 'make auth'.";

/// `HttpResponse.checkAndParse`'s status gate (`TtApiClient.kt:48-56`).
///
/// `None` means "fall through and parse the body", which is what the Kotlin `when`
/// does for every status it does not name — 2xx and 3xx alike.
///
/// The arm order is the contract: 401 and 403 are taken before the generic 4xx arm,
/// and 404 before it too, so those three never reach `Client error: …`.
///
/// `reason` is the status line's reason phrase. Ktor CIO builds its `HttpStatusCode`
/// from the wire — `ConnectionPipeline$responseHandler$1` calls
/// `new HttpStatusCode(response.getStatus(), response.getStatusText().toString())` —
/// so the incumbent echoes whatever phrase the server wrote. `hyper` discards the
/// phrase, so the port substitutes `StatusCode::canonical_reason()`. The two agree
/// for every server that sends the registered phrase, which is every mainstream one;
/// they diverge only if the portal invents its own, and this line is the record of
/// that.
pub(crate) fn classify_read_status(status: u16, reason: &str) -> Option<Failure> {
    match status {
        401 | 403 => Some(Failure::AuthRequired),
        404 => Some(Failure::Api(ApiError {
            status_code: 404,
            message: "Resource not found.".to_string(),
        })),
        400..=499 => Some(Failure::Api(ApiError {
            status_code: status,
            message: format!("Client error: {reason}"),
        })),
        500..=599 => Some(Failure::Api(ApiError {
            status_code: status,
            message: format!("Server error: {reason}"),
        })),
        _ => None,
    }
}

/// `HttpResponse.checkStatus`'s status gate (`TtApiClient.kt:58-66`).
///
/// A different shape from the read gate on purpose: the status is repeated inside
/// the parentheses and the **response body** stands where the reason phrase stands
/// above. There is no 404 arm here, so a 404 on a write reads `Client error (404):
/// …`. Collapsing the two gates into one helper is the tidy-up that would break
/// both messages at once.
pub(crate) fn classify_write_status(status: u16, body: &str) -> Option<Failure> {
    match status {
        401 | 403 => Some(Failure::AuthRequired),
        400..=499 => Some(Failure::Api(ApiError {
            status_code: status,
            message: format!("Client error ({status}): {body}"),
        })),
        500..=599 => Some(Failure::Api(ApiError {
            status_code: status,
            message: format!("Server error ({status}): {body}"),
        })),
        _ => None,
    }
}

/// `withAuthCheck` (`TtApiClient.kt:72-80`), with D5's change of sink.
///
/// The session cookie can only be refreshed by a host-side browser login
/// (`make auth`), which drives a GUI browser through Playwright, so there is nothing
/// to attempt in process: the instruction is surfaced and the call fails.
pub(crate) fn report_and_raise(failure: Failure, notice_sink: &mut impl Write) -> ApiError {
    match failure {
        Failure::AuthRequired => {
            // A failure to write the notice must not replace the failure being
            // reported, so the result is deliberately dropped.
            let _ = notice_sink.write_all(AUTH_FAILURE_NOTICE.as_bytes());
            ApiError {
                status_code: 401,
                message: AUTH_FAILURE_MESSAGE.to_string(),
            }
        }
        Failure::Api(error) => error,
    }
}

/// RFC 4122 §4.4 in ten lines, for the `IdempotencyKey` header at
/// `TtApiClient.kt:107,115`.
///
/// `## Stack` carries `rand` and no `uuid` crate, and the header's job bounds how
/// much that matters: the portal uses it to collapse a retried write, so it must be
/// unique per request rather than a certified UUID. Nothing reads it back.
fn idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    // Version 4 in the high nibble of byte 6, variant `10` in the top two bits of
    // byte 8.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `TtApiClient` (`TtApiClient.kt:21`).
pub struct TtApiClient {
    cookie: String,
    base_url: String,
    /// GET only, so redirects are followed — the `ALLOWED_FOR_REDIRECT` side.
    read_client: reqwest::Client,
    /// POST and DELETE, so redirects are **not** followed. See `api`'s module doc:
    /// following one here would turn a 302 into a 200 from the login page and report
    /// a worklog that was never written.
    write_client: reqwest::Client,
}

impl TtApiClient {
    /// `TtApiClient.kt:21-34`.
    pub fn new(cookie: impl Into<String>) -> Result<Self> {
        Self::with_base_url(cookie, BASE_URL, REQUEST_TIMEOUT, CONNECT_TIMEOUT)
    }

    /// The same client against an arbitrary origin and with the C31 bounds supplied,
    /// so the request shapes and the write bodies can be driven against a stub.
    pub(crate) fn with_base_url(
        cookie: impl Into<String>,
        base_url: impl Into<String>,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            cookie: cookie.into(),
            base_url: base_url.into(),
            read_client: build_client(request_timeout, connect_timeout, true)?,
            write_client: build_client(request_timeout, connect_timeout, false)?,
        })
    }

    /// `getCurrentUser` (`TtApiClient.kt:82-86`). C17 calls this before the month
    /// scan for no reason but to surface a dead session early.
    pub async fn get_current_user(&self) -> Result<CurrentUser> {
        let url = format!("{}/contact/currentUser", self.base_url);
        self.read(self.read_client.get(&url), &url).await
    }

    /// `getAssignedProjects` (`TtApiClient.kt:88-93`). The endpoint is
    /// `assignedProjectsOnDate` — assignments exist per date, not globally — so
    /// `date_from` is part of the answer and not a default to hide.
    pub async fn get_assigned_projects(
        &self,
        contact_id: &str,
        date_from: &str,
    ) -> Result<AssignedProjectsResponse> {
        let url = format!(
            "{}/contact/{contact_id}/assignedProjectsOnDate",
            self.base_url
        );
        self.read(
            self.read_client.get(&url).query(&[("dateFrom", date_from)]),
            &url,
        )
        .await
    }

    /// `getNormalView` (`TtApiClient.kt:95-102`). The page size is the literal 500
    /// from `:100`, not a paging loop: one page is the whole answer for one person's
    /// month, and adding paging would be a behaviour the incumbent does not have.
    pub async fn get_normal_view(&self, period: &str) -> Result<NormalViewResponse> {
        let url = format!("{}/timeTracking/normalView", self.base_url);
        self.read(
            self.read_client.get(&url).query(&[
                ("period", period),
                ("pageInfo.pageIndex", "1"),
                ("pageInfo.pageSize", "500"),
            ]),
            &url,
        )
        .await
    }

    /// `createWorklog` (`TtApiClient.kt:104-110`).
    pub async fn create_worklog(&self, request: &CreateWorklogRequest) -> Result<bool> {
        let url = format!("{}/worklog/create", self.base_url);
        self.write(&url, request).await
    }

    /// `updateWorklog` (`TtApiClient.kt:112-118`).
    pub async fn update_worklog(&self, request: &UpdateWorklogRequest) -> Result<bool> {
        let url = format!("{}/worklog/update", self.base_url);
        self.write(&url, request).await
    }

    /// `deleteWorklog` (`TtApiClient.kt:120-124`). No `IdempotencyKey` here — the
    /// incumbent sets one on the two POSTs and not on the DELETE, and a port that
    /// "completed the set" would be sending a header the portal has never seen from
    /// this tool.
    pub async fn delete_worklog(&self, unique_id: &str) -> Result<bool> {
        let url = format!("{}/worklog/{unique_id}", self.base_url);
        let response = self
            .write_client
            .delete(&url)
            .header("Cookie", &self.cookie)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        self.finish_write(response, &url).await
    }

    /// The read half of `checkAndParse`: the gate, then the body.
    async fn read<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        url: &str,
    ) -> Result<T> {
        let response = request
            .header("Cookie", &self.cookie)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;

        let status = response.status();
        if let Some(failure) =
            classify_read_status(status.as_u16(), status.canonical_reason().unwrap_or(""))
        {
            return Err(report_and_raise(failure, &mut std::io::stderr()).into());
        }

        let body = response
            .text()
            .await
            .with_context(|| format!("reading the response from {url}"))?;
        serde_json::from_str(&body).with_context(|| format!("parsing the response from {url}"))
    }

    /// The write half. The body is serialized at the call site and the content type
    /// is set by hand, which is what `jsonBody` did on the JVM for a reason that no
    /// longer exists — see the module doc.
    async fn write<T: Serialize>(&self, url: &str, request: &T) -> Result<bool> {
        let body = serde_json::to_string(request).context("serializing the worklog request")?;
        let response = self
            .write_client
            .post(url)
            .header("Cookie", &self.cookie)
            .header("IdempotencyKey", idempotency_key())
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        self.finish_write(response, url).await
    }

    /// `checkStatus` (`TtApiClient.kt:58-66`). The body is read **first** and
    /// unconditionally, exactly as `:59` does, and the success test is `== 200`
    /// rather than `is_success()` — so a 201 or a 302 comes back as `false` and the
    /// caller counts it as a failed write.
    async fn finish_write(&self, response: reqwest::Response, url: &str) -> Result<bool> {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .with_context(|| format!("reading the response from {url}"))?;

        if let Some(failure) = classify_write_status(status, &body) {
            return Err(report_and_raise(failure, &mut std::io::stderr()).into());
        }
        Ok(status == 200)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::stub::{StubServer, json_200, redirect_302, response, start_black_hole};

    const CONTACT: &str = "0f7c2a1e-1111-2222-3333-444455556666";

    fn client(base_url: &str) -> TtApiClient {
        TtApiClient::with_base_url("session=test", base_url, REQUEST_TIMEOUT, CONNECT_TIMEOUT)
            .expect("client")
    }

    fn create_request() -> CreateWorklogRequest {
        CreateWorklogRequest {
            worklog_date: "2026-07-13".to_string(),
            project_unique_id: "9096d358-040f-4a59-a623-8e5b1513b930".to_string(),
            task_title: "Velocitor NLP Daily".to_string(),
            billability: "Billable".to_string(),
            duration: 0.5,
            description: None,
            overtime: None,
            expense_type: Some("None".to_string()),
            pif: None,
            google_calendar_event_id: None,
        }
    }

    fn update_request() -> UpdateWorklogRequest {
        UpdateWorklogRequest {
            unique_id: CONTACT.to_string(),
            worklog_date: "2026-07-13".to_string(),
            project_unique_id: "9096d358-040f-4a59-a623-8e5b1513b930".to_string(),
            task_title: "Velocitor NLP Daily".to_string(),
            billability: "Billable".to_string(),
            duration: 1.0,
            description: None,
            overtime: None,
            expense_type: Some("None".to_string()),
            pif: None,
        }
    }

    // -- the two status gates ------------------------------------------------

    /// `TtApiClient.kt:50-51`. 404 is matched before the generic 4xx arm, so it gets
    /// its own message; a port that ordered the arms the other way would answer
    /// `Client error: Not Found` and pass any test that only checked "it failed".
    #[test]
    fn a_read_404_says_resource_not_found_rather_than_client_error() {
        assert_eq!(
            classify_read_status(404, "Not Found"),
            Some(Failure::Api(ApiError {
                status_code: 404,
                message: "Resource not found.".to_string(),
            }))
        );
        assert_eq!(
            classify_read_status(400, "Bad Request"),
            Some(Failure::Api(ApiError {
                status_code: 400,
                message: "Client error: Bad Request".to_string(),
            }))
        );
    }

    /// `:50`. Both auth statuses are taken before the 4xx arm, which is what makes
    /// the `make auth` instruction reachable at all.
    #[test]
    fn a_read_401_or_403_is_an_auth_failure_and_not_a_client_error() {
        assert_eq!(
            classify_read_status(401, "Unauthorized"),
            Some(Failure::AuthRequired)
        );
        assert_eq!(
            classify_read_status(403, "Forbidden"),
            Some(Failure::AuthRequired)
        );
        assert_eq!(
            classify_read_status(402, "Payment Required"),
            Some(Failure::Api(ApiError {
                status_code: 402,
                message: "Client error: Payment Required".to_string(),
            }))
        );
    }

    /// `:52-53`'s two ranges and the gap around them. Anything the `when` does not
    /// name falls through to the parse, which is why 200 and 302 are `None` — and
    /// the 302 is not academic: it is what an expired portal session can answer.
    #[test]
    fn the_read_gate_names_only_4xx_and_5xx_and_falls_through_everywhere_else() {
        assert_eq!(classify_read_status(200, "OK"), None);
        assert_eq!(classify_read_status(204, "No Content"), None);
        assert_eq!(classify_read_status(302, "Found"), None);
        assert_eq!(classify_read_status(399, ""), None);
        assert_eq!(
            classify_read_status(500, "Internal Server Error"),
            Some(Failure::Api(ApiError {
                status_code: 500,
                message: "Server error: Internal Server Error".to_string(),
            }))
        );
        assert_eq!(
            classify_read_status(599, ""),
            Some(Failure::Api(ApiError {
                status_code: 599,
                message: "Server error: ".to_string(),
            }))
        );
        assert_eq!(classify_read_status(600, "Nonsense"), None);
    }

    /// `:60-63` against `:49-53`. Same statuses, deliberately different text: the
    /// write gate repeats the code in parentheses and quotes the response **body**
    /// where the read gate quotes the reason phrase, and it has no 404 arm at all.
    #[test]
    fn the_write_gate_quotes_the_body_and_has_no_404_arm() {
        assert_eq!(
            classify_write_status(404, "no such worklog"),
            Some(Failure::Api(ApiError {
                status_code: 404,
                message: "Client error (404): no such worklog".to_string(),
            }))
        );
        assert_eq!(
            classify_write_status(500, "stack trace"),
            Some(Failure::Api(ApiError {
                status_code: 500,
                message: "Server error (500): stack trace".to_string(),
            }))
        );
        assert_eq!(
            classify_write_status(401, "whatever"),
            Some(Failure::AuthRequired)
        );
        assert_eq!(classify_write_status(200, "{}"), None);
        assert_eq!(classify_write_status(302, ""), None);
    }

    /// `:76-78` byte for byte: the leading blank line `println("\n…")` produces, the
    /// U+274C cross mark, the three-space indent on the second line, and the U+2014
    /// dash in the raised message. D5 is the sink being a parameter at all — every
    /// call site in this file passes `stderr`, where the incumbent passes `stdout`.
    #[test]
    fn an_auth_failure_writes_both_notice_lines_and_raises_a_401() {
        let mut sink: Vec<u8> = Vec::new();
        let error = report_and_raise(Failure::AuthRequired, &mut sink);

        assert_eq!(
            String::from_utf8(sink).expect("utf-8"),
            "\n\u{274c} Dev.Pro Time Tracking Portal session expired or invalid.\n   Run 'make auth' on your host machine to refresh the session.\n"
        );
        assert_eq!(error.status_code, 401);
        assert_eq!(
            error.message,
            "Authentication failed. Session cookie expired \u{2014} run 'make auth'."
        );
    }

    /// The other arm writes nothing: an ordinary API failure must not print the
    /// `make auth` instruction, which would send the operator to refresh a cookie
    /// that is fine.
    #[test]
    fn an_ordinary_api_failure_prints_no_notice_and_keeps_its_own_message() {
        let mut sink: Vec<u8> = Vec::new();
        let error = report_and_raise(
            Failure::Api(ApiError {
                status_code: 404,
                message: "Resource not found.".to_string(),
            }),
            &mut sink,
        );

        assert!(
            sink.is_empty(),
            "wrote {:?}",
            String::from_utf8_lossy(&sink)
        );
        assert_eq!(error.status_code, 404);
        assert_eq!(error.message, "Resource not found.");
    }

    // -- the idempotency key -------------------------------------------------

    /// RFC 4122 §4.4's shape, version and variant. Nothing reads the key back, so
    /// what matters is that it is well-formed and distinct per request.
    #[test]
    fn the_idempotency_key_is_a_v4_uuid_and_a_fresh_one_every_time() {
        let key = idempotency_key();
        let groups: Vec<&str> = key.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{key}"
        );
        assert!(
            key.chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {key}"
        );
        assert_eq!(groups[2].as_bytes()[0], b'4', "version nibble: {key}");
        assert!(
            matches!(groups[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "variant bits: {key}"
        );

        let many: std::collections::HashSet<String> = (0..64).map(|_| idempotency_key()).collect();
        assert_eq!(many.len(), 64, "every write needs its own key");
    }

    // -- request shapes ------------------------------------------------------

    /// `:83-85`. The cookie rides as a header on every call, which is the whole of
    /// the portal's authentication.
    #[tokio::test]
    async fn the_current_user_call_carries_the_cookie_to_contact_current_user() {
        let server = StubServer::start(vec![json_200(
            r#"{"uniqueId":"u-1","fullName":"Yurii","email":"y@dev.pro"}"#,
        )]);
        let user = client(&server.base_url)
            .get_current_user()
            .await
            .expect("current user");

        assert_eq!(user.unique_id, "u-1");
        let requests = server.requests();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/contact/currentUser");
        assert_eq!(requests[0].header("Cookie"), Some("session=test"));
    }

    /// `:89-91`. The contact id is a path segment and the date is a query parameter,
    /// and the endpoint name says why: assignments exist per date.
    #[tokio::test]
    async fn assigned_projects_puts_the_contact_in_the_path_and_the_date_in_the_query() {
        let server = StubServer::start(vec![json_200(
            r#"{"uniqueId":"u-1","projects":[{"uniqueId":"p-1","shortName":"Presales"}]}"#,
        )]);
        let response = client(&server.base_url)
            .get_assigned_projects(CONTACT, "2026-09-22")
            .await
            .expect("assigned projects");

        assert_eq!(response.projects.len(), 1);
        assert_eq!(response.projects[0].short_name, "Presales");
        let requests = server.requests();
        assert_eq!(
            requests[0].target,
            format!("/contact/{CONTACT}/assignedProjectsOnDate?dateFrom=2026-09-22")
        );
    }

    /// `:96-100`. The two `pageInfo.` parameters are literals, dots and all, and the
    /// page size is 500. A port that paged, or that renamed them to something a
    /// query-string builder found tidier, fails here.
    #[tokio::test]
    async fn the_normal_view_call_asks_for_one_page_of_five_hundred() {
        let server = StubServer::start(vec![json_200(
            r#"{"totalLoggedHours":8.0,"totalExpectedHours":8.0,"pageList":[]}"#,
        )]);
        let response = client(&server.base_url)
            .get_normal_view("2026-09-01 - 2026-09-30")
            .await
            .expect("normal view");

        assert_eq!(response.total_logged_hours, 8.0);
        let requests = server.requests();
        assert_eq!(
            requests[0].target,
            "/timeTracking/normalView?period=2026-09-01+-+2026-09-30&pageInfo.pageIndex=1&pageInfo.pageSize=500"
        );
    }

    // -- C18, the write bodies ----------------------------------------------

    /// C18, and the reason this file has a stub server at all. The body on the wire
    /// is asserted as a whole string — field order, the five explicit `null`s, and
    /// `"duration":0.5` as a number — because the write path is the one thing this
    /// run never exercises live. The expectation is `WorklogRequestBodyTest`'s,
    /// character for character.
    #[tokio::test]
    async fn the_create_body_on_the_wire_is_the_raw_json_object_the_portal_expects() {
        let server = StubServer::start(vec![json_200("")]);
        let created = client(&server.base_url)
            .create_worklog(&create_request())
            .await
            .expect("create");

        assert!(created, "a 200 is the incumbent's only success");
        let requests = server.requests();
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/worklog/create");
        assert_eq!(
            requests[0].body,
            r#"{"worklogDate":"2026-07-13","projectUniqueId":"9096d358-040f-4a59-a623-8e5b1513b930","taskTitle":"Velocitor NLP Daily","billability":"Billable","duration":0.5,"description":null,"overtime":null,"expenseType":"None","pif":null,"googleCalendarEventId":null}"#
        );
        assert_eq!(requests[0].header("Content-Type"), Some("application/json"));
        assert!(
            requests[0]
                .header("IdempotencyKey")
                .is_some_and(|k| k.len() == 36),
            "every write carries a fresh key"
        );
    }

    /// C18's other half. `UpdateWorklogRequest` leads with `uniqueId` and has **no**
    /// `googleCalendarEventId`, so it carries four nulls where create carries five —
    /// and `1.0` must not collapse to `1`.
    #[tokio::test]
    async fn the_update_body_leads_with_the_worklog_id_and_omits_the_calendar_field() {
        let server = StubServer::start(vec![json_200("")]);
        let updated = client(&server.base_url)
            .update_worklog(&update_request())
            .await
            .expect("update");

        assert!(updated);
        let requests = server.requests();
        assert_eq!(requests[0].target, "/worklog/update");
        assert_eq!(
            requests[0].body,
            format!(
                r#"{{"uniqueId":"{CONTACT}","worklogDate":"2026-07-13","projectUniqueId":"9096d358-040f-4a59-a623-8e5b1513b930","taskTitle":"Velocitor NLP Daily","billability":"Billable","duration":1.0,"description":null,"overtime":null,"expenseType":"None","pif":null}}"#
            )
        );
        assert!(
            !requests[0].body.contains("googleCalendarEventId"),
            "update has no such field"
        );
    }

    /// `:121-123`. DELETE puts the worklog id in the path and sends no idempotency
    /// key, because the incumbent sends none.
    #[tokio::test]
    async fn delete_names_the_worklog_in_the_path_and_sends_no_idempotency_key() {
        let server = StubServer::start(vec![json_200("")]);
        let deleted = client(&server.base_url)
            .delete_worklog(CONTACT)
            .await
            .expect("delete");

        assert!(deleted);
        let requests = server.requests();
        assert_eq!(requests[0].method, "DELETE");
        assert_eq!(requests[0].target, format!("/worklog/{CONTACT}"));
        assert_eq!(requests[0].header("IdempotencyKey"), None);
        assert!(requests[0].body.is_empty());
    }

    /// `:65` is `status == HttpStatusCode.OK`, so a 2xx that is not 200 is a *failed*
    /// write rather than a successful one. `is_success()` would report a worklog the
    /// portal may not have stored.
    #[tokio::test]
    async fn a_201_is_reported_as_a_failed_write() {
        let server = StubServer::start(vec![response(201, "Created", "application/json", "{}")]);
        let created = client(&server.base_url)
            .create_worklog(&create_request())
            .await
            .expect("no exception, just false");

        assert!(!created, "only 200 counts");
        server.requests();
    }

    // -- the redirect split --------------------------------------------------

    /// The step-3 finding, on the side where it bites. Ktor's `ALLOWED_FOR_REDIRECT`
    /// is `{GET, HEAD}`, so a 302 on a POST is handed back as a 302 and
    /// `checkStatus` answers `false`. `reqwest`'s default policy would follow it,
    /// re-issue the write as a GET of the login page, read that page's 200 and
    /// report a worklog that was never created — the exact shape an expired session
    /// cookie produces.
    ///
    /// The second canned response is the instrument, and it has to be there for this
    /// test to mean what it says. With only the 302 canned, a client that follows
    /// gets a connection refused and the test fails on the `expect` — the right
    /// verdict for the wrong reason, and a portal whose login page answers (which is
    /// the real case) would sail past. With a 200 waiting behind the redirect, a
    /// following client reaches `Ok(true)` and dies on the assertion that names the
    /// hazard. `seen` rather than `requests` because a correct client never fetches
    /// that second response and joining would wait for it forever.
    #[tokio::test]
    async fn a_302_on_a_write_is_not_followed_and_is_reported_as_a_failed_write() {
        let server = StubServer::start(vec![
            redirect_302("/login"),
            json_200(r#"{"id":"a worklog that was never created"}"#),
        ]);
        let created = client(&server.base_url)
            .create_worklog(&create_request())
            .await
            .expect("a redirect is not an exception");

        assert!(
            !created,
            "a 302 on a write is not a written worklog, whatever answers behind it"
        );
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            1,
            "the write must not be re-issued as a GET of the redirect target"
        );
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].target, "/worklog/create");
    }

    /// The other side of the same split: GET *is* in `ALLOWED_FOR_REDIRECT`, so a
    /// read follows and the incumbent's behaviour is reproduced by leaving
    /// `reqwest`'s default in place.
    #[tokio::test]
    async fn a_302_on_a_read_is_followed() {
        let server = StubServer::start(vec![
            redirect_302("/contact/currentUser2"),
            json_200(r#"{"uniqueId":"u-2","fullName":"Yurii","email":"y@dev.pro"}"#),
        ]);
        let user = client(&server.base_url)
            .get_current_user()
            .await
            .expect("current user");

        assert_eq!(user.unique_id, "u-2");
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "the read must follow the redirect");
        assert_eq!(requests[1].target, "/contact/currentUser2");
    }

    // -- C31 on the path that matters ---------------------------------------

    /// C31's first row on the write client. The rationale for the bound is entirely
    /// about this path: a POST that hangs removes the timeout the operator procedure
    /// ("verify with `api get-worklogs`, do not retry") starts from, and leaves a
    /// terminal that merely looks busy.
    #[tokio::test]
    async fn a_write_to_a_server_that_never_answers_ends_rather_than_hanging() {
        let base = start_black_hole();
        let client = TtApiClient::with_base_url(
            "session=test",
            &base,
            Duration::from_millis(150),
            CONNECT_TIMEOUT,
        )
        .expect("client");

        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            client.create_worklog(&create_request()),
        )
        .await;

        let inner = outcome.expect("the write must bound itself, not wait for the guard");
        let err = inner.expect_err("a silent server cannot confirm a write");
        let source = err
            .downcast_ref::<reqwest::Error>()
            .expect("the failure comes from the transport");
        assert!(source.is_timeout(), "expected a timeout, got {source}");
    }

    /// An auth failure reaches the caller as `ApiError` with the 401 the incumbent
    /// raises, through the real request path rather than through the pure gate.
    #[tokio::test]
    async fn a_401_from_the_portal_surfaces_as_the_make_auth_api_error() {
        let server = StubServer::start(vec![response(
            401,
            "Unauthorized",
            "text/plain",
            "session expired",
        )]);
        let err = client(&server.base_url)
            .get_current_user()
            .await
            .expect_err("401 must fail");

        let api = err.downcast_ref::<ApiError>().expect("an ApiError");
        assert_eq!(api.status_code, 401);
        assert_eq!(
            api.message,
            "Authentication failed. Session cookie expired \u{2014} run 'make auth'."
        );
        server.requests();
    }
}
