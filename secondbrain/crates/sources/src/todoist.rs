//! Todoist via the unified **API v1** (`api.todoist.com/api/v1`), ported from Focustask
//! `todoist_sync.py`. Personal Bearer token; cursor pagination. REST v2 / Sync v9 are deprecated.

use crate::{Result, SourceError};
use chrono::NaiveDate;
use sb_core::Task;

const API: &str = "https://api.todoist.com/api/v1";

/// Fetch open tasks matching `filter` (e.g. "today | overdue").
pub fn fetch_tasks(token: &str, filter: &str) -> Result<Vec<Task>> {
    let client = crate::http_client();
    let mut tasks = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut req = client
            .get(format!("{API}/tasks"))
            .bearer_auth(token)
            .query(&[("filter", filter), ("limit", "200")]);
        if let Some(c) = &cursor {
            req = req.query(&[("cursor", c.as_str())]);
        }
        let resp = req.send()?;
        let status = resp.status();
        let body: serde_json::Value = resp.json()?;
        if !status.is_success() {
            return Err(SourceError::Api(status.as_u16(), body.to_string()));
        }
        tasks.extend(parse_tasks(&body));
        match next_cursor(&body) {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(tasks)
}

/// Fetch EVERY open task (no `filter` param — v1 `GET /tasks` unfiltered returns all active
/// tasks). Used by the One Thing picker, which browses the whole backlog rather than a day view.
pub fn fetch_all_tasks(token: &str) -> Result<Vec<Task>> {
    let client = crate::http_client();
    let mut tasks = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut req = client.get(format!("{API}/tasks")).bearer_auth(token).query(&[("limit", "200")]);
        if let Some(c) = &cursor {
            req = req.query(&[("cursor", c.as_str())]);
        }
        let resp = req.send()?;
        let status = resp.status();
        let body: serde_json::Value = resp.json()?;
        if !status.is_success() {
            return Err(SourceError::Api(status.as_u16(), body.to_string()));
        }
        tasks.extend(parse_tasks(&body));
        match next_cursor(&body) {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(tasks)
}

/// Close (complete) a task. Used by the planner's on-device check-off.
pub fn complete_task(token: &str, id: &str) -> Result<()> {
    let client = crate::http_client();
    let resp = client
        .post(format!("{API}/tasks/{id}/close"))
        .bearer_auth(token)
        .send()?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(SourceError::Api(status.as_u16(), resp.text().unwrap_or_default()))
    }
}

/// Delete a task entirely (v1 `DELETE /tasks/{id}`). Unlike close, this removes it — for
/// recurring tasks it ends the whole series.
pub fn delete_task(token: &str, id: &str) -> Result<()> {
    let client = crate::http_client();
    let resp = client
        .delete(format!("{API}/tasks/{id}"))
        .bearer_auth(token)
        .send()?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(SourceError::Api(status.as_u16(), resp.text().unwrap_or_default()))
    }
}

/// How to rewrite a task's due date when deferring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DueUpdate {
    /// Exact date (`{"due_date": "YYYY-MM-DD"}`). Destroys recurrence — the planner UI
    /// disables defer for recurring tasks, so this is the only variant the device needs.
    Date(NaiveDate),
}

/// JSON body for a due-date update (pure, for tests).
pub fn due_update_body(due: &DueUpdate) -> serde_json::Value {
    match due {
        DueUpdate::Date(d) => serde_json::json!({ "due_date": d.format("%Y-%m-%d").to_string() }),
    }
}

/// Defer: rewrite a task's due date (v1 `POST /tasks/{id}`).
pub fn update_due(token: &str, id: &str, due: &DueUpdate) -> Result<()> {
    let client = crate::http_client();
    let resp = client
        .post(format!("{API}/tasks/{id}"))
        .bearer_auth(token)
        .json(&due_update_body(due))
        .send()?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(SourceError::Api(status.as_u16(), resp.text().unwrap_or_default()))
    }
}

/// JSON body for a priority rewrite (pure, for tests). `priority` is the RAW Todoist value —
/// 4 = P1 (urgent) … 1 = P4 — the same value the whole codebase stores and colours by.
pub fn priority_body(priority: u8) -> serde_json::Value {
    serde_json::json!({ "priority": priority.clamp(1, 4) })
}

/// Reprioritise: rewrite a task's priority (v1 `POST /tasks/{id}`, like `update_due`).
pub fn update_priority(token: &str, id: &str, priority: u8) -> Result<()> {
    let client = crate::http_client();
    let resp = client
        .post(format!("{API}/tasks/{id}"))
        .bearer_auth(token)
        .json(&priority_body(priority))
        .send()?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(SourceError::Api(status.as_u16(), resp.text().unwrap_or_default()))
    }
}

/// JSON body for creating a task (pure, for tests). `due` is a natural-language due string;
/// `priority` is the raw Todoist value (4 = P1 … 1 = P4), omitted when `None` (Todoist then
/// defaults to 1 = P4).
pub fn create_body(content: &str, due: Option<&str>, priority: Option<u8>) -> serde_json::Value {
    let mut body = serde_json::json!({ "content": content });
    if let Some(d) = due {
        body["due_string"] = serde_json::json!(d);
    }
    if let Some(p) = priority {
        body["priority"] = serde_json::json!(p.clamp(1, 4));
    }
    body
}

/// Create a task (v1 `POST /tasks`). Unlike the other writes, this reads its response back to
/// recover the server-assigned id and normalized fields. `due` is optional natural language
/// ("today", "tomorrow"); with `None` the task lands in the Todoist inbox.
pub fn create_task(token: &str, content: &str, due: Option<&str>, priority: Option<u8>) -> Result<Task> {
    let client = crate::http_client();
    let resp = client
        .post(format!("{API}/tasks"))
        .bearer_auth(token)
        .json(&create_body(content, due, priority))
        .send()?;
    let status = resp.status();
    let json: serde_json::Value = resp.json()?;
    if !status.is_success() {
        return Err(SourceError::Api(status.as_u16(), json.to_string()));
    }
    parse_task(&json).ok_or_else(|| SourceError::Api(status.as_u16(), format!("unparseable create response: {json}")))
}

fn next_cursor(body: &serde_json::Value) -> Option<String> {
    body.get("next_cursor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Parse a v1 `/tasks` response. v1 wraps results in `{ "results": [...] }`; a bare array
/// (older shape) is tolerated too.
pub fn parse_tasks(body: &serde_json::Value) -> Vec<Task> {
    let items = if let Some(arr) = body.as_array() {
        arr.as_slice()
    } else if let Some(arr) = body.get("results").and_then(|v| v.as_array()) {
        arr.as_slice()
    } else {
        return Vec::new();
    };
    items.iter().filter_map(parse_task).collect()
}

fn parse_task(it: &serde_json::Value) -> Option<Task> {
    let id = json_id(it.get("id")?)?;
    let title = it.get("content").and_then(|v| v.as_str())?.to_string();
    let priority = it.get("priority").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 4) as u8;
    let due = it
        .get("due")
        .and_then(|d| d.get("date"))
        .and_then(|v| v.as_str())
        .and_then(|s| NaiveDate::parse_from_str(&s[..s.len().min(10)], "%Y-%m-%d").ok());
    let is_recurring = it
        .get("due")
        .and_then(|d| d.get("is_recurring"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let duration_min = it.get("duration").and_then(parse_duration).unwrap_or(0);
    let done = it.get("checked").and_then(|v| v.as_bool()).unwrap_or(false)
        || it.get("is_completed").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(Task { id, title, priority, due, duration_min, done, is_recurring })
}

/// Todoist ids are strings in v1 but were numbers historically — accept both.
fn json_id(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        Some(s.to_string())
    } else {
        v.as_u64().map(|n| n.to_string())
    }
}

fn parse_duration(d: &serde_json::Value) -> Option<u32> {
    let amount = d.get("amount").and_then(|v| v.as_u64())? as u32;
    match d.get("unit").and_then(|v| v.as_str()).unwrap_or("minute") {
        "day" => Some(amount * 24 * 60),
        _ => Some(amount),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v1_results_wrapper() {
        let body = serde_json::json!({
            "results": [
                {"id": "6X4", "content": "Ship renderer", "priority": 4,
                 "due": {"date": "2026-06-29", "is_recurring": false},
                 "duration": {"amount": 60, "unit": "minute"}},
                {"id": "6X5", "content": "Email Sam", "priority": 1},
                {"id": "6X6", "content": "Water plants", "priority": 2,
                 "due": {"date": "2026-06-30", "is_recurring": true, "string": "every sat"}}
            ],
            "next_cursor": ""
        });
        let tasks = parse_tasks(&body);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].title, "Ship renderer");
        assert_eq!(tasks[0].priority, 4);
        assert_eq!(tasks[0].due, Some(NaiveDate::from_ymd_opt(2026, 6, 29).unwrap()));
        assert_eq!(tasks[0].duration_min, 60);
        assert!(!tasks[0].is_recurring);
        assert_eq!(tasks[1].priority, 1);
        assert!(!tasks[1].is_recurring, "no due block defaults to non-recurring");
        assert!(tasks[2].is_recurring);
    }

    #[test]
    fn due_update_bodies() {
        let d = DueUpdate::Date(NaiveDate::from_ymd_opt(2026, 7, 6).unwrap());
        assert_eq!(due_update_body(&d), serde_json::json!({"due_date": "2026-07-06"}));
    }

    #[test]
    fn create_body_and_response_parse() {
        assert_eq!(create_body("Buy milk", None, None), serde_json::json!({"content": "Buy milk"}));
        assert_eq!(
            create_body("Buy milk", Some("tomorrow"), None),
            serde_json::json!({"content": "Buy milk", "due_string": "tomorrow"})
        );
        // Priority rides along as the RAW Todoist value (4 = P1) and clamps garbage.
        assert_eq!(
            create_body("Ship it", None, Some(4)),
            serde_json::json!({"content": "Ship it", "priority": 4})
        );
        assert_eq!(create_body("x", None, Some(9))["priority"], serde_json::json!(4));
        assert_eq!(priority_body(3), serde_json::json!({"priority": 3}));
        assert_eq!(priority_body(0), serde_json::json!({"priority": 1}), "clamped");
        // the create response is a single task object → parse_task recovers the server id
        let resp = serde_json::json!({"id": "789", "content": "Buy milk", "priority": 1,
            "due": {"date": "2026-07-06", "is_recurring": false}});
        let t = parse_task(&resp).expect("parses the create response");
        assert_eq!(t.id, "789");
        assert_eq!(t.title, "Buy milk");
        assert_eq!(t.due, Some(NaiveDate::from_ymd_opt(2026, 7, 6).unwrap()));
    }

    #[test]
    fn tolerates_bare_array_and_numeric_id() {
        let body = serde_json::json!([{ "id": 12345, "content": "x", "priority": 2 }]);
        let tasks = parse_tasks(&body);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "12345");
    }

    #[test]
    fn next_cursor_empty_is_none() {
        assert!(next_cursor(&serde_json::json!({"next_cursor": ""})).is_none());
        assert_eq!(next_cursor(&serde_json::json!({"next_cursor": "abc"})), Some("abc".to_string()));
    }
}
