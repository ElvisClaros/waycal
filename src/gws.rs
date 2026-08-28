//! Google Calendar/Tasks backend, talking to the `gws` CLI.

use std::process::Command;

use chrono::{DateTime, Days, Local, NaiveDate};
use serde_json::{Map, Value, json};

use crate::backend::{AccountData, Backend, EventDraft, Result, TaskDraft};
use crate::config::{Account, expand_tilde};
use crate::model::{Calendar, Event, Task, TaskList};

/// Runs one gws invocation for the given account and parses the JSON reply.
fn run(account: &Account, args: &[&str], params: Option<&Value>, body: Option<&Value>) -> Result<Value> {
    // gws drops a stray `download.html` in its cwd when a response has no
    // body (e.g. deletes), so keep it out of wherever waycal was launched.
    let scratch = std::env::temp_dir().join("waycal-gws");
    let _ = std::fs::create_dir_all(&scratch);
    let config_dir = account.config_dir.as_deref().ok_or_else(|| format!("[{}] missing config_dir", account.name))?;
    let credentials_file = account
        .credentials_file
        .as_deref()
        .ok_or_else(|| format!("[{}] missing credentials_file", account.name))?;
    let mut cmd = Command::new("gws");
    cmd.env("GOOGLE_WORKSPACE_CLI_CONFIG_DIR", expand_tilde(config_dir))
        .env("GOOGLE_WORKSPACE_CLI_CREDENTIALS_FILE", expand_tilde(credentials_file))
        .current_dir(&scratch)
        .args(args);
    if let Some(p) = params {
        cmd.arg("--params").arg(p.to_string());
    }
    if let Some(b) = body {
        cmd.arg("--json").arg(b.to_string());
    }
    let out = cmd
        .output()
        .map_err(|e| format!("[{}] cannot run gws: {}", account.name, e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "[{}] gws {} failed: {}",
            account.name,
            args.join(" "),
            err.lines().last().unwrap_or("unknown error").trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null); // delete returns no body
    }
    serde_json::from_str(trimmed).map_err(|e| format!("[{}] bad JSON from gws: {}", account.name, e))
}

fn items(v: &Value) -> Vec<Value> {
    v.get("items").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn list_calendars(account: &Account) -> Result<Vec<Calendar>> {
    let v = run(account, &["calendar", "calendarList", "list"], Some(&json!({"maxResults": 250})), None)?;
    Ok(items(&v)
        .iter()
        // Match what the user shows in the Google Calendar UI.
        .filter(|c| c.get("selected").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|c| Calendar::from_json(&account.name, c))
        .collect())
}

fn list_events(
    account: &Account,
    cal: &Calendar,
    from: DateTime<Local>,
    to: DateTime<Local>,
    hide_types: &[String],
) -> Result<Vec<Event>> {
    let params = json!({
        "calendarId": cal.id,
        "singleEvents": true,
        "orderBy": "startTime",
        "timeMin": from.to_rfc3339(),
        "timeMax": to.to_rfc3339(),
        "maxResults": 250,
    });
    let v = run(account, &["calendar", "events", "list"], Some(&params), None)?;
    Ok(items(&v)
        .iter()
        .filter(|e| {
            let ty = e.get("eventType").and_then(Value::as_str).unwrap_or("default");
            !hide_types.iter().any(|h| h == ty)
        })
        .filter_map(|e| Event::from_json(&account.name, &cal.id, &cal.summary, e))
        .collect())
}

fn list_tasklists(account: &Account) -> Result<Vec<TaskList>> {
    let v = run(account, &["tasks", "tasklists", "list"], Some(&json!({"maxResults": 100})), None)?;
    Ok(items(&v).iter().filter_map(|t| TaskList::from_json(&account.name, t)).collect())
}

fn list_tasks(account: &Account, list: &TaskList) -> Result<Vec<Task>> {
    let params = json!({"tasklist": list.id, "maxResults": 100, "showCompleted": false});
    let v = run(account, &["tasks", "tasks", "list"], Some(&params), None)?;
    Ok(items(&v)
        .iter()
        .filter_map(|t| Task::from_json(&account.name, &list.id, &list.title, t))
        .collect())
}

fn patch_task(account: &Account, tasklist_id: &str, task_id: &str, body: &Value) -> Result<Value> {
    let params = json!({"tasklist": tasklist_id, "task": task_id});
    run(account, &["tasks", "tasks", "patch"], Some(&params), Some(body))
}

/// Builds a Calendar API event body from a draft. `include_attendees` forces
/// the (possibly empty) guest list into the body so an edit can clear it;
/// on create it's only included when non-empty.
fn event_body(draft: &EventDraft, include_attendees: bool) -> Value {
    let mut body = Map::new();
    body.insert("summary".into(), json!(draft.summary));
    let (start, end) = if draft.all_day {
        (
            json!({"date": draft.start_date.format("%Y-%m-%d").to_string()}),
            // All-day ends are exclusive in the API.
            json!({"date": (draft.end_date + Days::new(1)).format("%Y-%m-%d").to_string()}),
        )
    } else {
        (
            json!({"dateTime": draft.start_time.expect("timed event needs start_time").to_rfc3339()}),
            json!({"dateTime": draft.end_time.expect("timed event needs end_time").to_rfc3339()}),
        )
    };
    body.insert("start".into(), start);
    body.insert("end".into(), end);
    body.insert("location".into(), json!(draft.location));
    body.insert("description".into(), json!(draft.description));
    if include_attendees || !draft.attendees.is_empty() {
        let attendees: Vec<Value> = draft.attendees.iter().map(|g| json!({"email": g})).collect();
        body.insert("attendees".into(), json!(attendees));
    }
    if draft.add_meet {
        body.insert(
            "conferenceData".into(),
            json!({"createRequest": {
                "requestId": format!("waycal-{}", Local::now().timestamp_millis()),
                "conferenceSolutionKey": {"type": "hangoutsMeet"},
            }}),
        );
    }
    Value::Object(body)
}

/// Builds a Tasks API body from a draft (title/notes/due only; callers add
/// `id`/`status` for a full-replace update).
fn task_body(draft: &TaskDraft) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("title".into(), json!(draft.title));
    if !draft.notes.is_empty() {
        body.insert("notes".into(), json!(draft.notes));
    }
    if let Some(due) = draft.due {
        body.insert("due".into(), json!(format!("{}T00:00:00.000Z", due.format("%Y-%m-%d"))));
    }
    body
}

/// Everything waycal shows for one account (fetched over [from, to)).
/// Partial failures degrade to what could be fetched; errors are collected.
fn fetch_account(
    account: &Account,
    from: NaiveDate,
    to: NaiveDate,
    hide_types: &[String],
    errors: &mut Vec<String>,
) -> AccountData {
    let mut data = AccountData::default();

    let day_start = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).earliest().unwrap()
    };
    match list_calendars(account) {
        Ok(cals) => {
            for cal in &cals {
                match list_events(account, cal, day_start(from), day_start(to), hide_types) {
                    Ok(evs) => data.events.extend(evs),
                    Err(e) => errors.push(e),
                }
            }
            data.calendars = cals;
        }
        Err(e) => errors.push(e),
    }

    match list_tasklists(account) {
        Ok(lists) => {
            for list in &lists {
                match list_tasks(account, list) {
                    Ok(ts) => data.tasks.extend(ts),
                    Err(e) => errors.push(e),
                }
            }
            data.tasklists = lists;
        }
        Err(e) => errors.push(e),
    }

    data.events.sort_by_key(|e| (e.start_date, e.start_time));
    data
}

pub struct GoogleBackend;

impl Backend for GoogleBackend {
    fn fetch_account(
        &self,
        account: &Account,
        from: NaiveDate,
        to: NaiveDate,
        hide_types: &[String],
        errors: &mut Vec<String>,
    ) -> AccountData {
        fetch_account(account, from, to, hide_types, errors)
    }

    fn insert_event(&self, account: &Account, calendar_id: &str, draft: &EventDraft, notify: bool) -> Result<()> {
        let body = event_body(draft, false);
        let params = json!({
            "calendarId": calendar_id,
            "conferenceDataVersion": 1,
            "sendUpdates": if notify { "all" } else { "none" },
        });
        run(account, &["calendar", "events", "insert"], Some(&params), Some(&body)).map(|_| ())
    }

    fn patch_event(
        &self,
        account: &Account,
        calendar_id: &str,
        event_id: &str,
        draft: &EventDraft,
        notify: bool,
    ) -> Result<()> {
        let body = event_body(draft, true);
        let params = json!({
            "calendarId": calendar_id,
            "eventId": event_id,
            "conferenceDataVersion": 1,
            "sendUpdates": if notify { "all" } else { "none" },
        });
        run(account, &["calendar", "events", "patch"], Some(&params), Some(&body)).map(|_| ())
    }

    fn delete_event(&self, account: &Account, calendar_id: &str, event_id: &str) -> Result<()> {
        let params = json!({"calendarId": calendar_id, "eventId": event_id});
        run(account, &["calendar", "events", "delete"], Some(&params), None).map(|_| ())
    }

    fn insert_task(&self, account: &Account, tasklist_id: &str, draft: &TaskDraft) -> Result<()> {
        let body = Value::Object(task_body(draft));
        run(account, &["tasks", "tasks", "insert"], Some(&json!({"tasklist": tasklist_id})), Some(&body)).map(|_| ())
    }

    /// Full-resource update. Unlike patch, omitted optional fields (due,
    /// notes) are cleared — and gws' schema validation rejects explicit
    /// nulls, so this is the only way to clear them.
    fn update_task(&self, account: &Account, tasklist_id: &str, task_id: &str, draft: &TaskDraft) -> Result<()> {
        let mut body = task_body(draft);
        body.insert("id".into(), json!(task_id));
        body.insert("status".into(), json!("needsAction"));
        let params = json!({"tasklist": tasklist_id, "task": task_id});
        run(account, &["tasks", "tasks", "update"], Some(&params), Some(&Value::Object(body))).map(|_| ())
    }

    fn complete_task(&self, account: &Account, tasklist_id: &str, task_id: &str) -> Result<()> {
        patch_task(account, tasklist_id, task_id, &json!({"status": "completed"})).map(|_| ())
    }

    fn delete_task(&self, account: &Account, tasklist_id: &str, task_id: &str) -> Result<()> {
        let params = json!({"tasklist": tasklist_id, "task": task_id});
        run(account, &["tasks", "tasks", "delete"], Some(&params), None).map(|_| ())
    }
}
