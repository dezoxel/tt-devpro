//! The Chrono REST client.
//!
//! Ports `api/ChronoClient.kt` in full — 44 lines, one request, one error message.
//!
//! **C2 lives at the call sites, not here.** `getTimeEntries(start, end)` is a
//! literal pass-through of the two dates onto the query string; the `+1 day` padding
//! on the UTC axis and the re-dating of every entry to its local day happen in
//! `service::aggregator` and in the settle scan. This client neither pads nor
//! re-dates, and a port that "helpfully" did either would move a decision out of the
//! module that the contract is written against.
//!
//! **`isLenient = true` (`ChronoClient.kt:17`) does not port, and it is recorded
//! rather than worked around.** It relaxes the JSON *grammar* — unquoted keys,
//! unquoted string values — and `serde_json` accepts only strict JSON with no switch
//! to loosen it. Chrono is Go with `encoding/json`, which never emits either, so the
//! port being stricter than the incumbent is unreachable in practice.
//! `ignoreUnknownKeys = true` ports for free: serde ignores unknown fields unless
//! told otherwise.
//!
//! **There is no `close()`.** `ChronoClient.kt:41-43` exists because a Ktor client
//! owns a thread pool that outlives its last request; `reqwest::Client` releases its
//! pool on drop, so the Kotlin `finally { chronoClient.close() }` at
//! `SettleCommand.kt:110` has nothing to port to.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;

use crate::api::{CONNECT_TIMEOUT, REQUEST_TIMEOUT, build_client};
use crate::model::ChronoTimeEntry;

/// `ChronoClient` (`ChronoClient.kt:14`).
pub struct ChronoClient {
    base_url: String,
    client: reqwest::Client,
}

impl ChronoClient {
    /// `ChronoClient.kt:14-24`. `base_url` is the config's `chrono_api` verbatim,
    /// trailing slash and all, because `ChronoClient.kt:27` concatenates it verbatim.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Self::with_timeouts(base_url, REQUEST_TIMEOUT, CONNECT_TIMEOUT)
    }

    /// The same client with the C31 bounds supplied rather than taken from the
    /// constants, so a test can drive the real request path against a real timeout
    /// without waiting fifteen seconds for it.
    pub(crate) fn with_timeouts(
        base_url: impl Into<String>,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into(),
            // Only ever issues GET, so it sits on the redirect-following side of the
            // `ALLOWED_FOR_REDIRECT` split.
            client: build_client(request_timeout, connect_timeout, true)?,
        })
    }

    /// `ChronoClient.getTimeEntries` (`ChronoClient.kt:26-39`).
    ///
    /// The success gate is `in 200..299`, not "is 200" and not `is_success` — those
    /// happen to agree, and the literal range is what the source says.
    pub async fn get_time_entries(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<Vec<ChronoTimeEntry>> {
        let url = format!("{}/api/time-entries", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[
                ("start_date", start_date.to_string()),
                ("end_date", end_date.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;

        let status = response.status().as_u16();
        let body = response.text().await.context("reading the Chrono body")?;

        if (200..=299).contains(&status) {
            return serde_json::from_str(&body)
                .with_context(|| format!("parsing the Chrono response from {url}"));
        }

        // `ChronoClient.kt:36` — `error(…)` throws `IllegalStateException`; the message is the
        // contract, down to the blank line before the question.
        bail!(
            "Chrono API error ({status}): {body}\n\nIs Chrono running at {}?",
            self.base_url
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::stub::{StubServer, json_200, response, start_black_hole};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("a real date")
    }

    const ONE_ENTRY: &str = r#"[{"id":7,"description":"Sync w Ivan","start_time":"2026-09-18T05:55:30-04:00","end_time":"2026-09-18T06:25:30-04:00","duration":1800,"project":null,"aspect":null}]"#;

    /// `ChronoClient.kt:27-30`: the path is `/api/time-entries` appended to the
    /// configured base, and both dates go on the query string under their snake_case
    /// names. A port that sent `startDate`, or that padded the end date here instead
    /// of at the call site (C2), fails this.
    #[tokio::test]
    async fn the_fetch_puts_both_dates_on_the_query_string_of_api_time_entries() {
        let server = StubServer::start(vec![json_200("[]")]);
        let client = ChronoClient::new(&server.base_url).expect("client");

        let entries = client
            .get_time_entries(d(2026, 9, 12), d(2026, 9, 19))
            .await
            .expect("fetch");
        assert!(entries.is_empty());

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].target,
            "/api/time-entries?start_date=2026-09-12&end_date=2026-09-19"
        );
    }

    /// The base URL is concatenated verbatim (`ChronoClient.kt:27`), so a configured value carrying
    /// a path prefix keeps it. `~/.config/tt-devpro/config.yaml` holds a bare origin today, which
    /// is exactly why nothing else would notice a port that parsed and rebuilt it.
    #[tokio::test]
    async fn a_base_url_with_a_path_prefix_keeps_the_prefix() {
        let server = StubServer::start(vec![json_200("[]")]);
        let client = ChronoClient::new(format!("{}/chrono", server.base_url)).expect("client");

        client
            .get_time_entries(d(2026, 9, 12), d(2026, 9, 13))
            .await
            .expect("fetch");

        let requests = server.requests();
        assert_eq!(
            requests[0].target,
            "/chrono/api/time-entries?start_date=2026-09-12&end_date=2026-09-13"
        );
    }

    /// A 2xx body parses into the model, including the offset-carrying timestamp
    /// shape — 36 % of the live window is `-04:00` rather than `Z`.
    #[tokio::test]
    async fn a_2xx_body_parses_into_entries() {
        let server = StubServer::start(vec![json_200(ONE_ENTRY)]);
        let client = ChronoClient::new(&server.base_url).expect("client");

        let entries = client
            .get_time_entries(d(2026, 9, 18), d(2026, 9, 19))
            .await
            .expect("fetch");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 7);
        assert_eq!(entries[0].start_time, "2026-09-18T05:55:30-04:00");
        assert_eq!(entries[0].duration, Some(1800));
        server.requests();
    }

    /// `ChronoClient.kt:33` is `in 200..299`, so the top of the range parses and the first status
    /// outside it does not. `is_success()` would agree on both; `status == 200`
    /// would fail the first, and that is the port this test exists to catch.
    ///
    /// 299 rather than 204: a 204 carries no body by definition, so `hyper` discards
    /// one even when a stub sends it, and the request would fail on the empty parse
    /// under either reading of the gate — a case that cannot separate them.
    #[tokio::test]
    async fn the_top_of_the_success_range_parses_and_a_300_does_not() {
        let server = StubServer::start(vec![
            response(299, "Almost There", "application/json", "[]"),
            response(300, "Multiple Choices", "text/plain", "pick one"),
        ]);
        let client = ChronoClient::new(&server.base_url).expect("client");

        let ok = client
            .get_time_entries(d(2026, 9, 18), d(2026, 9, 19))
            .await;
        assert!(ok.is_ok(), "299 is inside 200..299: {ok:?}");
        assert!(ok.expect("parsed").is_empty());

        let err = client
            .get_time_entries(d(2026, 9, 18), d(2026, 9, 19))
            .await
            .expect_err("300 is outside 200..299");
        assert!(
            err.to_string()
                .starts_with("Chrono API error (300): pick one"),
            "got {err}"
        );
        server.requests();
    }

    /// `ChronoClient.kt:36`'s message verbatim: the status in parentheses, the raw body, a blank
    /// line, and the base URL in the question. The blank line is `\n\n` in the Kotlin
    /// string and is the part a retyped port drops.
    #[tokio::test]
    async fn a_failure_names_the_status_the_body_and_the_configured_base_url() {
        let server = StubServer::start(vec![response(
            500,
            "Internal Server Error",
            "text/plain",
            "boom",
        )]);
        let base = server.base_url.clone();
        let client = ChronoClient::new(&base).expect("client");

        let err = client
            .get_time_entries(d(2026, 9, 18), d(2026, 9, 19))
            .await
            .expect_err("500 must fail");

        assert_eq!(
            err.to_string(),
            format!("Chrono API error (500): boom\n\nIs Chrono running at {base}?")
        );
        server.requests();
    }

    /// C31's first row, exercised rather than asserted. The server accepts and never
    /// answers; the request must end by itself. The outer guard is what makes a
    /// missing `.timeout(…)` fail this test instead of hanging the suite forever.
    #[tokio::test]
    async fn a_server_that_never_answers_ends_the_request_rather_than_hanging() {
        let base = start_black_hole();
        let client =
            ChronoClient::with_timeouts(&base, Duration::from_millis(150), CONNECT_TIMEOUT)
                .expect("client");

        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            client.get_time_entries(d(2026, 9, 18), d(2026, 9, 19)),
        )
        .await;

        let inner = outcome.expect("the request must bound itself, not wait for the guard");
        let err = inner.expect_err("a silent server cannot produce entries");
        let source = err
            .downcast_ref::<reqwest::Error>()
            .expect("the failure comes from the transport");
        assert!(source.is_timeout(), "expected a timeout, got {source}");
    }
}
