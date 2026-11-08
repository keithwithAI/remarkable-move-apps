//! Google Calendar via a **service account** (calendar.readonly).
//! Share each calendar with the service-account email; drop its JSON key on the device.

use crate::{Result, SourceError};
use chrono::{DateTime, Duration, Local};
use sb_core::Event;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}
fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

pub fn load_service_account(path: &Path) -> Result<ServiceAccount> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Mint a short-lived OAuth access token from the service-account key (JWT-bearer grant).
pub fn access_token(sa: &ServiceAccount, now_unix: i64) -> Result<String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let claims = Claims {
        iss: &sa.client_email,
        // Read-write events scope: fetching works on read-shared calendars regardless; DELETE
        // additionally needs the calendar shared with the service account as "Make changes to
        // events" (a 403 otherwise, parked by the action queue).
        scope: "https://www.googleapis.com/auth/calendar.events",
        aud: &sa.token_uri,
        iat: now_unix,
        exp: now_unix + 3600,
    };
    let key = EncodingKey::from_rsa_pem(sa.private_key.as_bytes())
        .map_err(|e| SourceError::Auth(format!("service-account key: {e}")))?;
    let jwt = encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| SourceError::Auth(format!("jwt sign: {e}")))?;

    let client = crate::http_client();
    let resp = client
        .post(&sa.token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", jwt.as_str()),
        ])
        .send()?;
    let status = resp.status();
    let body: serde_json::Value = resp.json()?;
    if !status.is_success() {
        return Err(SourceError::Api(status.as_u16(), body.to_string()));
    }
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| SourceError::Auth("no access_token in token response".into()))
}

/// Fetch timed events across all calendars in `[time_min, time_max)`. A failing calendar is
/// logged and skipped (matching `gcal.py`), so one revoked share doesn't kill the whole agenda —
/// but if EVERY calendar fails (flaky WiFi, API outage) this is an `Err`, not an empty success:
/// callers cache successful fetches, and an "empty" result here used to overwrite the last-good
/// cache and blank the sleep screen on a transient outage.
pub fn fetch_events(
    sa_path: &Path,
    calendar_ids: &[String],
    time_min: &str,
    time_max: &str,
    now: DateTime<Local>,
) -> Result<Vec<Event>> {
    let sa = load_service_account(sa_path)?;
    let token = access_token(&sa, now.timestamp())?;
    let client = crate::http_client();
    let mut all = Vec::new();
    let mut failures = 0usize;
    let mut last_err: Option<SourceError> = None;
    for cal in calendar_ids {
        match fetch_one(&client, &token, cal, time_min, time_max) {
            Ok(mut evs) => all.append(&mut evs),
            Err(e) => {
                log::warn!("skipping calendar {cal:?}: {e}");
                failures += 1;
                last_err = Some(e);
            }
        }
    }
    if !calendar_ids.is_empty() && failures == calendar_ids.len() {
        return Err(last_err.unwrap_or_else(|| SourceError::Auth("all calendars failed".into())));
    }
    Ok(all)
}

fn fetch_one(
    client: &reqwest::blocking::Client,
    token: &str,
    cal: &str,
    time_min: &str,
    time_max: &str,
) -> Result<Vec<Event>> {
    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/{}/events",
        percent_encode(cal)
    );
    let mut events = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut req = client.get(&url).bearer_auth(token).query(&[
            ("timeMin", time_min),
            ("timeMax", time_max),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", "250"),
        ]);
        if let Some(pt) = &page_token {
            req = req.query(&[("pageToken", pt.as_str())]);
        }
        let resp = req.send()?;
        let status = resp.status();
        let body: serde_json::Value = resp.json()?;
        if !status.is_success() {
            return Err(SourceError::Api(status.as_u16(), body.to_string()));
        }
        events.extend(parse_events(&body, cal));
        match body.get("nextPageToken").and_then(|v| v.as_str()) {
            Some(t) => page_token = Some(t.to_string()),
            None => break,
        }
    }
    Ok(events)
}

/// Delete one event (a recurring *instance* when the id is an instance id — the fetch expands with
/// singleEvents, so a delete removes just that occurrence). Requires the calendar to be shared with
/// the service account as "Make changes to events"; a read-only share yields a 403 `Api` error the
/// action queue parks as permanent.
pub fn delete_event(sa_path: &Path, calendar_id: &str, event_id: &str, now: DateTime<Local>) -> Result<()> {
    let sa = load_service_account(sa_path)?;
    let token = access_token(&sa, now.timestamp())?;
    let url = format!(
        "https://www.googleapis.com/calendar/v3/calendars/{}/events/{}",
        percent_encode(calendar_id),
        percent_encode(event_id)
    );
    let resp = crate::http_client().delete(&url).bearer_auth(&token).send()?;
    let status = resp.status();
    // 204 = deleted; 404/410 = already gone (treat as success so a stale queue entry drains).
    if status.is_success() || status.as_u16() == 404 || status.as_u16() == 410 {
        return Ok(());
    }
    let body = resp.text().unwrap_or_default();
    Err(SourceError::Api(status.as_u16(), body))
}

/// Parse a Calendar `events.list` response into [`Event`]s, skipping all-day events
/// (those carry `start.date` instead of `start.dateTime`).
pub fn parse_events(body: &serde_json::Value, calendar_id: &str) -> Vec<Event> {
    let items = match body.get("items").and_then(|v| v.as_array()) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for it in items {
        let (Some(start), Some(end)) = (it.get("start"), it.get("end")) else { continue };
        if start.get("date").is_some() {
            continue; // all-day
        }
        let (Some(sd), Some(ed)) = (
            start.get("dateTime").and_then(|v| v.as_str()),
            end.get("dateTime").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let (Some(s), Some(e)) = (parse_rfc3339(sd), parse_rfc3339(ed)) else { continue };
        let description = it.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
        out.push(Event {
            title: it.get("summary").and_then(|v| v.as_str()).unwrap_or("(No title)").to_string(),
            start: s,
            end: e,
            calendar_id: calendar_id.to_string(),
            id: it.get("id").and_then(|v| v.as_str()).map(String::from),
            color_id: it.get("colorId").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()),
            todoist_task: is_scheduled_block(&description)
                .then(|| diarize_todoist_id(&description))
                .flatten(),
            description,
        });
    }
    out
}

/// The marker Diarized ends every auto-scheduled block's description with. Gating on it is what
/// separates a task block from an ordinary meeting.
///
/// **Deliberately the shorter, older spelling.** Diarized renamed itself mid-2026 and now writes
/// `Auto-scheduled by Diarized`, keeping `Auto-scheduled by Diarize` as a legacy marker for blocks
/// already sitting in calendars (its `core/marker.py`: `DEFAULT_MARKER` / `LEGACY_MARKERS`).
/// Because the new string *contains* the old one, this single substring matches both, and would go
/// on matching a further rename that keeps the "Auto-scheduled by Diarize…" stem. Pinned by
/// `both_marker_spellings_are_task_blocks`.
const MARKER: &str = "Auto-scheduled by Diarize";

/// Is this description one of Diarized's auto-scheduled task blocks?
///
/// The description is **HTML** (`<br>`, `<a href>`, `&nbsp;`) and ends with
/// `Auto-scheduled by Diarized (task:<uuid>)`. The marker is all we need: the device reads a
/// Diarized block, it never writes to Diarized, so it has no use for the `(task:<uuid>)` handle or
/// the action-link host.
///
/// That independence is the point. Until 2026-08-17 this function also derived and allowlisted the
/// action URLs' host so `Recalculate` could POST to them. When Diarized moved from
/// `getdiarize.com` to `getdiarized.com`, the allowlist rejected every block, `todoist_task` came
/// back `None`, and the whole day view silently degraded to plain uncheckable events — the second
/// time a rename upstream had that effect. Recalculate is gone (its route now needs a Diarized
/// login session, so the device cannot call it at all), and with it the only reason to care what
/// host the description names.
/// Public so `examples/parse_live.rs` can report the marker-vs-checkable split over a saved live
/// response — the exact diagnostic that makes this class of outage visible in one command.
pub fn is_scheduled_block(description: &str) -> bool {
    description.contains(MARKER)
}

/// The Todoist task id behind a Diarized block, from the "Open in Todoist" link. This is what
/// makes a block checkable, so a block whose link is missing renders but cannot be completed.
///
/// (Diarized only exposes the Todoist id by way of this href; see the integration notes for the
/// request to put it in the marker line instead.)
///
/// The id lands in a Todoist API path, so it is restricted to alphanumerics. Note what this does
/// *not* defend against, unchanged from before and accepted: anyone who can put an event on the
/// calendar writes this description, so a crafted invite carrying the marker plus a guessed task
/// id renders a row that would close that task in the user's own Todoist if they tapped it.
fn diarize_todoist_id(description: &str) -> Option<String> {
    const LINK: &str = "app.todoist.com/app/task/";
    let at = description.find(LINK)? + LINK.len();
    let id: String = description[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.with_timezone(&Local))
}

/// `[start-of-today, start-of-tomorrow)` in local time, as RFC3339 strings for the API.
pub fn today_range(now: DateTime<Local>) -> (String, String) {
    let day = now.date_naive();
    window_range(day, day)
}

/// `[start-of-from, start-of-day-after-to)` in local time — an inclusive date window as
/// RFC3339 strings for the API. Used by the planner's multi-day fetch.
pub fn window_range(from: chrono::NaiveDate, to: chrono::NaiveDate) -> (String, String) {
    let start = local_midnight(from);
    let end = local_midnight(to.max(from)) + Duration::days(1);
    (start.to_rfc3339(), end.to_rfc3339())
}

/// Local midnight of `day`, robust to DST edges (falls forward to the earliest valid time).
fn local_midnight(day: chrono::NaiveDate) -> DateTime<Local> {
    let naive = day.and_hms_opt(0, 0, 0).unwrap();
    match naive.and_local_timezone(Local) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t,
        // Midnight skipped by a DST jump: take the first valid instant after it.
        chrono::LocalResult::None => (naive + Duration::hours(1))
            .and_local_timezone(Local)
            .earliest()
            .expect("an hour past midnight exists in every timezone"),
    }
}

/// Percent-encode a calendar id for use in a URL path segment.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timed_events_and_skips_all_day() {
        let body = serde_json::json!({
            "items": [
                { "summary": "Standup", "id": "ev_standup_20260629T093000Z",
                  "start": {"dateTime": "2026-06-29T09:30:00+01:00"},
                  "end":   {"dateTime": "2026-06-29T09:45:00+01:00"} },
                { "summary": "Holiday", "start": {"date": "2026-06-29"}, "end": {"date": "2026-06-30"} },
                { "start": {"dateTime": "2026-06-29T14:00:00+01:00"},
                  "end":   {"dateTime": "2026-06-29T15:00:00+01:00"} }
            ]
        });
        let evs = parse_events(&body, "primary");
        assert_eq!(evs.len(), 2, "all-day event should be skipped");
        assert_eq!(evs[0].title, "Standup");
        assert_eq!(evs[0].id.as_deref(), Some("ev_standup_20260629T093000Z"), "google event id captured");
        assert_eq!(evs[1].id, None, "id-less item parses fine");
        assert_eq!(evs[0].duration_min(), 15);
        assert_eq!(evs[0].color_id, None);
        assert_eq!(evs[0].todoist_task, None);
        assert_eq!(evs[1].title, "(No title)");
        assert_eq!(evs[1].calendar_id, "primary");
    }

    /// A **real** Diarized description in the shape the service emits today: the renamed marker
    /// (`Diarized`) and action links on the current apex, `getdiarized.com`. Kept byte-for-byte
    /// (HTML entities, emoji, `&nbsp;` separators and all) because the whole point of this parser
    /// is to survive the exact bytes Diarized emits.
    const REAL_DIARIZED_DESC: &str = "Low priority<br>Duration: 30m<br>Due: 2026-08-17<br><br>\
        <a href=\"https://app.todoist.com/app/task/6hExampleTask001\">Open in Todoist</a><br><br>\
        <a href=\"https://getdiarized.com/task/0123456789abcdef0123456789abcdef/done\">✅ Mark done</a> \
        &nbsp;·&nbsp; <a href=\"https://getdiarized.com/task/0123456789abcdef0123456789abcdef/recalculate\">🔄 Recalculate</a> \
        &nbsp;·&nbsp; <a href=\"https://getdiarized.com/why/0123456789abcdef0123456789abcdef\">💡 Why this time</a> \
        &nbsp;·&nbsp; <a href=\"https://getdiarized.com/task/0123456789abcdef0123456789abcdef/not-today\">⏭ Not today</a> \
        &nbsp;·&nbsp; <a href=\"https://getdiarized.com/task/0123456789abcdef0123456789abcdef/needs-longer\">⏱ Needs longer</a><br><br>\
        Auto-scheduled by Diarized (task:0123456789abcdef0123456789abcdef)";

    /// The same block in its older shape, before the rename:
    /// `Diarize` marker, `getdiarize.com` links. Blocks in this shape sit in calendars for as long
    /// as Diarized takes to rewrite them, so both must parse identically.
    const LEGACY_DIARIZE_DESC: &str = "Low priority<br>Duration: 30m<br>Due: 2026-08-11<br><br>\
        <a href=\"https://app.todoist.com/app/task/6hExampleTask001\">Open in Todoist</a><br><br>\
        <a href=\"https://getdiarize.com/task/0123456789abcdef0123456789abcdef/done\">✅ Mark done</a> \
        &nbsp;·&nbsp; <a href=\"https://getdiarize.com/task/0123456789abcdef0123456789abcdef/recalculate\">🔄 Recalculate</a><br><br>\
        Auto-scheduled by Diarize (task:0123456789abcdef0123456789abcdef)";

    #[test]
    fn parses_a_real_diarized_task_block() {
        let body = serde_json::json!({
            "items": [
                { "summary": "Ship the planner", "colorId": "2", "description": REAL_DIARIZED_DESC,
                  "start": {"dateTime": "2026-08-17T08:00:00+01:00"},
                  "end":   {"dateTime": "2026-08-17T08:30:00+01:00"} },
                { "summary": "Plain meeting", "colorId": "7", "description": "notes",
                  "start": {"dateTime": "2026-08-17T10:00:00+01:00"},
                  "end":   {"dateTime": "2026-08-17T11:00:00+01:00"} }
            ]
        });
        let evs = parse_events(&body, "primary");
        assert_eq!(evs[0].todoist_task.as_deref(), Some("6hExampleTask001"));
        assert_eq!(evs[0].priority(None), Some(1), "Sage block is P4 (\"Low priority\")");

        assert_eq!(evs[1].todoist_task, None, "no marker → plain event");
    }

    /// The regression test for the 2026-08-17 outage. Diarized renamed `getdiarize.com` →
    /// `getdiarized.com`; the parser allowlisted the old host to build Recalculate URLs from, so
    /// it rejected every new block, `todoist_task` came back `None`, and the entire day view
    /// degraded to plain uncheckable events. A block is now recognised by its marker alone, so the
    /// host in the description — any host, including one that does not exist yet — is irrelevant.
    #[test]
    fn a_block_is_checkable_whatever_host_its_action_links_name() {
        let block = |marker_suffix: &str, host: &str| {
            format!(
                "<a href=\"https://app.todoist.com/app/task/6hExampleTask001\">Open in Todoist</a><br>\
                 <a href=\"{host}/task/abc123/done\">✅ Mark done</a><br>\
                 Auto-scheduled by Diarize{marker_suffix} (task:abc123)"
            )
        };
        let checkable = |desc: &str| {
            let body = serde_json::json!({
                "items": [{ "summary": "t", "description": desc,
                    "start": {"dateTime": "2026-08-17T08:00:00+01:00"},
                    "end":   {"dateTime": "2026-08-17T09:00:00+01:00"} }]
            });
            parse_events(&body, "primary")[0].todoist_task.clone()
        };
        for (suffix, host) in [
            ("d", "https://getdiarized.com"),      // today
            ("", "https://getdiarize.com"),        // before the rename
            ("d", "https://eu.getdiarized.com"),   // a regional subdomain
            ("d", "https://diarized.example"),     // a rename that hasn't happened yet
            ("d", "http://localhost:8000"),        // a self-hosted instance
        ] {
            assert_eq!(
                checkable(&block(suffix, host)).as_deref(),
                Some("6hExampleTask001"),
                "marker + Todoist link must be enough for {host}"
            );
        }
    }

    #[test]
    fn both_marker_spellings_are_task_blocks() {
        // `MARKER` is the short stem on purpose: the renamed marker contains it, so one substring
        // covers both. Losing this is how the day view silently goes plain.
        let evs = |desc: &str| {
            let body = serde_json::json!({
                "items": [{ "summary": "t", "description": desc,
                    "start": {"dateTime": "2026-08-17T08:00:00+01:00"},
                    "end":   {"dateTime": "2026-08-17T09:00:00+01:00"} }]
            });
            parse_events(&body, "primary")
        };
        assert_eq!(evs(REAL_DIARIZED_DESC)[0].todoist_task.as_deref(), Some("6hExampleTask001"));
        assert_eq!(evs(LEGACY_DIARIZE_DESC)[0].todoist_task.as_deref(), Some("6hExampleTask001"));
        assert!(is_scheduled_block("Auto-scheduled by Diarized (task:a)"));
        assert!(is_scheduled_block("Auto-scheduled by Diarize (task:a)"));
        assert!(!is_scheduled_block(""));
        assert!(
            !is_scheduled_block("Auto-scheduled by Focustask (task:9)"),
            "the retired Focustask marker is not a Diarized block"
        );
    }

    #[test]
    fn colour_palette_matches_the_live_calendar() {
        // Observed across 26 live blocks on 2026-08-11, and matching Diarized's own
        // `RANK_COLOR = {0:"11", 1:"6", 2:"5", 3:"2"}` in `adapters/gcal.py`.
        let ev = |color: u8| Event {
            title: "t".into(),
            start: Local::now(),
            end: Local::now(),
            calendar_id: "c".into(),
            id: None,
            description: String::new(),
            color_id: Some(color),
            todoist_task: None,
        };
        assert_eq!(ev(11).priority(None), Some(4), "Highest priority");
        assert_eq!(ev(6).priority(None), Some(3), "High priority");
        assert_eq!(ev(5).priority(None), Some(2), "Medium priority");
        assert_eq!(ev(2).priority(None), Some(1), "Low priority");
    }

    #[test]
    fn marker_without_a_todoist_link_is_not_checkable() {
        // The Todoist link is what makes a block completable; a block missing it must render as a
        // normal row rather than a checkbox that completes nothing.
        let desc = "<a href=\"https://getdiarized.com/task/deadbeef/done\">done</a><br>\
                    Auto-scheduled by Diarized (task:deadbeef)";
        let body = serde_json::json!({
            "items": [
                { "summary": "Orphan", "description": desc,
                  "start": {"dateTime": "2026-08-17T08:00:00+01:00"},
                  "end":   {"dateTime": "2026-08-17T09:00:00+01:00"} }
            ]
        });
        let evs = parse_events(&body, "primary");
        assert_eq!(evs[0].todoist_task, None, "no link → not checkable");
    }

    #[test]
    fn a_todoist_link_alone_is_not_a_task_block() {
        // An ordinary meeting whose notes happen to link a Todoist task must stay a plain event —
        // otherwise it would render a checkbox that completes someone's unrelated task.
        let body = serde_json::json!({
            "items": [
                { "summary": "Standup", "description": "see https://app.todoist.com/app/task/123",
                  "start": {"dateTime": "2026-08-11T09:00:00+01:00"},
                  "end":   {"dateTime": "2026-08-11T09:15:00+01:00"} }
            ]
        });
        let evs = parse_events(&body, "primary");
        assert_eq!(evs[0].todoist_task, None);
    }

    #[test]
    fn empty_or_missing_items_is_empty() {
        assert!(parse_events(&serde_json::json!({}), "primary").is_empty());
        assert!(parse_events(&serde_json::json!({"items": []}), "primary").is_empty());
    }

    #[test]
    fn percent_encode_handles_group_calendar_ids() {
        assert_eq!(percent_encode("primary"), "primary");
        assert_eq!(percent_encode("a@group.calendar.google.com"), "a%40group.calendar.google.com");
    }

    #[test]
    fn window_range_spans_inclusive_dates() {
        use chrono::NaiveDate;
        let from = NaiveDate::from_ymd_opt(2026, 6, 28).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 7, 5).unwrap();
        let (min, max) = window_range(from, to);
        assert!(min.starts_with("2026-06-28T00:00:00"), "{min}");
        assert!(max.starts_with("2026-07-06T00:00:00"), "end is exclusive next-day: {max}");
        // degenerate: to < from clamps to a single day
        let (min1, max1) = window_range(to, from);
        assert!(min1.starts_with("2026-07-05T") && max1.starts_with("2026-07-06T"));
    }
}
