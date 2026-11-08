//! Data sources: Google Calendar (service account) and Todoist (API v1).
//!
//! These two are the whole network surface. Diarized, which writes the task blocks the planner
//! renders, is reached **only** through the calendar it writes to — the device never calls it.
//! (It briefly did, for `Recalculate`; that route now requires a Diarized login session, so the
//! `diarize` module and its host allowlist were removed on 2026-08-17.)
//!
//! The network calls need live credentials, so they're exercised on-device / in integration;
//! the **parsing** is split out into pure functions that are unit-tested against fixtures here.

use thiserror::Error;

pub mod gcal;
pub mod todoist;

/// The blocking HTTP client both sources use — with a hard timeout, because these run in oneshot
/// timer binaries (`agenda` renders the suspend screen) where a half-open connection on flaky WiFi
/// would otherwise hang the process indefinitely. Panics only if TLS init fails (same fail-fast as
/// `Client::new()`).
pub(crate) fn http_client() -> reqwest::blocking::Client {
    client_with_timeout(std::time::Duration::from_secs(30))
}

/// Same client, explicit deadline — for calls that want a tighter or looser bound than the 30s
/// default (see [`diarize::recalculate`]). Still bounded: never no-timeout.
pub(crate) fn client_with_timeout(timeout: std::time::Duration) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .expect("building HTTP client")
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("auth: {0}")]
    Auth(String),
    #[error("api {0}: {1}")]
    Api(u16, String),
}

pub type Result<T> = std::result::Result<T, SourceError>;
