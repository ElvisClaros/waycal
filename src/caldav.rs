//! Nextcloud (CalDAV) backend. Talks plain WebDAV/CalDAV (PROPFIND/REPORT/
//! GET/PUT/DELETE) over HTTP Basic auth with an app password — no OAuth,
//! no vendor CLI to shell out to, unlike the Google side (`gws.rs`).
//!
//! CalDAV has no partial update: every "patch" is GET the current object,
//! merge the new fields on top, PUT the whole thing back. See `patch_event`/
//! `update_task`/`complete_task`.

use std::collections::HashMap;

use attohttpc::Method;
use base64::Engine;
use chrono::{Days, Local, NaiveDate, Utc};
use ical::parser::ical::component::{IcalAlarm, IcalEvent, IcalTodo};
use ical::property::Property;
use roxmltree::Document;
use uuid::Uuid;

use crate::backend::{AccountData, Backend, EventDraft, Result, TaskDraft};
use crate::config::{Account, expand_tilde};
use crate::model::{Calendar, Event, Task, TaskList, ical_value, strip_mailto};

const DAV: &str = "DAV:";
const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";

/// Resolved connection details for one account. Read fresh from disk on
/// every call (like `gws.rs` does for its credential paths) so a rotated
/// app password takes effect without restarting waycal.
struct Nc {
    base: String,
    username: String,
    password: String,
}

fn nc(account: &Account) -> Result<Nc> {
    let server_url = account.server_url.as_deref().ok_or_else(|| format!("[{}] missing server_url", account.name))?;
    let username = account.username.as_deref().ok_or_else(|| format!("[{}] missing username", account.name))?;
    let pw_path = account
        .app_password_file
        .as_deref()
        .ok_or_else(|| format!("[{}] missing app_password_file", account.name))?;
    let password = std::fs::read_to_string(expand_tilde(pw_path))
        .map_err(|e| format!("[{}] cannot read app_password_file: {}", account.name, e))?
        .trim()
        .to_string();
    Ok(Nc { base: server_url.trim_end_matches('/').to_string(), username: username.to_string(), password })
}

fn auth_header(nc: &Nc) -> String {
    let raw = format!("{}:{}", nc.username, nc.password);
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
}

fn calendars_url(nc: &Nc) -> String {
    format!("/remote.php/dav/calendars/{}/", nc.username)
}

fn resolve(nc: &Nc, path: &str) -> String {
    if path.starts_with("http") { path.to_string() } else { format!("{}{}", nc.base, path) }
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// One CalDAV request. `path` is either a full URL or an absolute path
/// (starting with `/`) relative to `nc.base`. Non-2xx statuses are returned,
/// not raised as errors, so callers can decide (e.g. a 404 on DELETE is
/// "already gone", not a failure).
fn request(nc: &Nc, method: &str, path: &str, content_type: Option<&str>, body: String) -> Result<(u16, String)> {
    let url = resolve(nc, path);
    let http_method = Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut builder = attohttpc::RequestBuilder::new(http_method, &url).header("Authorization", auth_header(nc));
    if let Some(ct) = content_type {
        builder = builder.header("Content-Type", ct);
    }
    if matches!(method, "PROPFIND" | "REPORT") {
        builder = builder.header("Depth", "1");
    }
    let resp = builder.text(body).send().map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().map_err(|e| e.to_string())?;
    Ok((status, text))
}

// ---------------------------------------------------------------------
// Discovery & listing
// ---------------------------------------------------------------------

struct CalendarInfo {
    href: String,
    displayname: String,
    writable: bool,
    supports_vevent: bool,
    supports_vtodo: bool,
}

const PROPFIND_CALENDARS_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <cal:supported-calendar-component-set/>
    <d:current-user-privilege-set/>
  </d:prop>
</d:propfind>"#;

fn list_calendars(nc: &Nc) -> Result<Vec<CalendarInfo>> {
    let (status, text) = request(nc, "PROPFIND", &calendars_url(nc), Some("application/xml; charset=utf-8"), PROPFIND_CALENDARS_BODY.to_string())?;
    if !ok(status) {
        return Err(format!("PROPFIND calendars: HTTP {status}"));
    }
    let doc = Document::parse(&text).map_err(|e| format!("bad PROPFIND response: {e}"))?;

    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| n.has_tag_name((DAV, "response"))) {
        let Some(href) = resp.descendants().find(|c| c.has_tag_name((DAV, "href"))).and_then(|n| n.text()) else {
            continue;
        };
        let is_calendar = resp
            .descendants()
            .find(|c| c.has_tag_name((DAV, "resourcetype")))
            .is_some_and(|rt| rt.children().any(|c| c.has_tag_name((CALDAV, "calendar"))));
        if !is_calendar {
            continue;
        }
        let displayname = resp
            .descendants()
            .find(|c| c.has_tag_name((DAV, "displayname")))
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string();

        let (mut supports_vevent, mut supports_vtodo) = (false, false);
        if let Some(comp_set) = resp.descendants().find(|c| c.has_tag_name((CALDAV, "supported-calendar-component-set"))) {
            for comp in comp_set.children().filter(|c| c.has_tag_name((CALDAV, "comp"))) {
                match comp.attribute("name") {
                    Some("VEVENT") => supports_vevent = true,
                    Some("VTODO") => supports_vtodo = true,
                    _ => {}
                }
            }
        }

        let writable = resp
            .descendants()
            .find(|c| c.has_tag_name((DAV, "current-user-privilege-set")))
            .is_some_and(|set| {
                set.descendants()
                    .filter(|p| p.has_tag_name((DAV, "privilege")))
                    .any(|p| p.children().any(|c| matches!(c.tag_name().name(), "write" | "write-content" | "all")))
            });

        out.push(CalendarInfo { href: href.to_string(), displayname, writable, supports_vevent, supports_vtodo });
    }
    Ok(out)
}

fn events_report_body(from: chrono::DateTime<Utc>, to: chrono::DateTime<Utc>) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<cal:calendar-query xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <cal:calendar-data/>
  </d:prop>
  <cal:filter>
    <cal:comp-filter name="VCALENDAR">
      <cal:comp-filter name="VEVENT">
        <cal:time-range start="{}" end="{}"/>
      </cal:comp-filter>
    </cal:comp-filter>
  </cal:filter>
</cal:calendar-query>"#,
        from.format("%Y%m%dT%H%M%SZ"),
        to.format("%Y%m%dT%H%M%SZ"),
    )
}

/// Fetches all VTODOs unfiltered (no server-side completed/incomplete
/// filter — RFC4791 `is-not-defined` prop-filters are a known cross-server
/// footgun). The UI/daemon already filter `!completed` client-side, so this
/// mirrors existing behavior at the cost of re-fetching completed todos on
/// every poll — acceptable for v1.
const TODOS_REPORT_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<cal:calendar-query xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <cal:calendar-data/>
  </d:prop>
  <cal:filter>
    <cal:comp-filter name="VCALENDAR">
      <cal:comp-filter name="VTODO"/>
    </cal:comp-filter>
  </cal:filter>
</cal:calendar-query>"#;

/// Extracts (href, calendar-data text) pairs from a REPORT multistatus body.
fn calendar_data_items(text: &str) -> Result<Vec<(String, String)>> {
    let doc = Document::parse(text).map_err(|e| format!("bad REPORT response: {e}"))?;
    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| n.has_tag_name((DAV, "response"))) {
        let Some(href) = resp.descendants().find(|c| c.has_tag_name((DAV, "href"))).and_then(|n| n.text()) else {
            continue;
        };
        let Some(data) = resp.descendants().find(|c| c.has_tag_name((CALDAV, "calendar-data"))).and_then(|n| n.text()) else {
            continue;
        };
        out.push((href.to_string(), data.to_string()));
    }
    Ok(out)
}

fn parse_first_event(ics: &str) -> Option<IcalEvent> {
    let mut parser = ical::IcalParser::new(std::io::Cursor::new(ics.as_bytes()));
    parser.next()?.ok()?.events.into_iter().next()
}

fn parse_first_todo(ics: &str) -> Option<IcalTodo> {
    let mut parser = ical::IcalParser::new(std::io::Cursor::new(ics.as_bytes()));
    parser.next()?.ok()?.todos.into_iter().next()
}

fn fetch_account(account: &Account, from: NaiveDate, to: NaiveDate, errors: &mut Vec<String>) -> AccountData {
    let mut data = AccountData::default();
    let nc = match nc(account) {
        Ok(nc) => nc,
        Err(e) => {
            errors.push(e);
            return data;
        }
    };
    let cals = match list_calendars(&nc) {
        Ok(c) => c,
        Err(e) => {
            errors.push(e);
            return data;
        }
    };

    let day_start = |d: NaiveDate| d.and_hms_opt(0, 0, 0).unwrap().and_local_timezone(Local).earliest().unwrap().with_timezone(&Utc);
    let (from_utc, to_utc) = (day_start(from), day_start(to));

    for cal in &cals {
        if cal.supports_vevent {
            let calendar = Calendar {
                account: account.name.clone(),
                id: cal.href.clone(),
                summary: cal.displayname.clone(),
                access_role: if cal.writable { "owner".to_string() } else { "reader".to_string() },
                default_reminder_mins: Vec::new(), // no CalDAV equivalent
                primary: false,                    // no CalDAV equivalent
            };
            match request(&nc, "REPORT", &cal.href, Some("application/xml; charset=utf-8"), events_report_body(from_utc, to_utc)) {
                Ok((status, text)) if ok(status) => match calendar_data_items(&text) {
                    Ok(items) => {
                        for (href, ics) in items {
                            if let Some(ev) =
                                parse_first_event(&ics).and_then(|e| Event::from_ical(&account.name, &calendar.id, &calendar.summary, &href, &e))
                            {
                                data.events.push(ev);
                            }
                        }
                    }
                    Err(e) => errors.push(e),
                },
                Ok((status, _)) => errors.push(format!("[{}] REPORT {} (events): HTTP {status}", account.name, cal.href)),
                Err(e) => errors.push(e),
            }
            data.calendars.push(calendar);
        }

        if cal.supports_vtodo {
            let list = TaskList { account: account.name.clone(), id: cal.href.clone(), title: cal.displayname.clone() };
            match request(&nc, "REPORT", &cal.href, Some("application/xml; charset=utf-8"), TODOS_REPORT_BODY.to_string()) {
                Ok((status, text)) if ok(status) => match calendar_data_items(&text) {
                    Ok(items) => {
                        for (href, ics) in items {
                            if let Some(t) = parse_first_todo(&ics).and_then(|t| Task::from_ical(&account.name, &list.id, &list.title, &href, &t)) {
                                data.tasks.push(t);
                            }
                        }
                    }
                    Err(e) => errors.push(e),
                },
                Ok((status, _)) => errors.push(format!("[{}] REPORT {} (tasks): HTTP {status}", account.name, cal.href)),
                Err(e) => errors.push(e),
            }
            data.tasklists.push(list);
        }
    }

    data.events.sort_by_key(|e| (e.start_date, e.start_time));
    data
}

// ---------------------------------------------------------------------
// ICS generation
// ---------------------------------------------------------------------

fn ics_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(',', "\\,").replace(';', "\\;").replace('\n', "\\n")
}

/// Folds one already-escaped, single logical content line to <=75 octets
/// per physical line (continuation lines are prefixed with one space, which
/// counts toward that line's budget), CRLF-terminated. UTF-8-safe: never
/// splits inside a multi-byte sequence.
fn ics_fold(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() <= 75 {
        return format!("{line}\r\n");
    }
    let mut out = String::new();
    let mut start = 0;
    let mut budget = 75;
    while start < bytes.len() {
        let mut end = (start + budget).min(bytes.len());
        while end > start && !line.is_char_boundary(end) {
            end -= 1; // don't split a UTF-8 character
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n");
        start = end;
        if start < bytes.len() {
            out.push(' ');
            budget = 74; // the leading space counts against the next line's 75 octets
        }
    }
    out
}

fn ics_line(name: &str, value: &str) -> String {
    ics_fold(&format!("{name}:{}", ics_escape(value)))
}

/// Reconstructs a property's content line from its parsed form (name,
/// params, value). Used to preserve fields verbatim (ORGANIZER, VALARM)
/// across a GET-merge-PUT edit — not re-escaped, since the source value is
/// carried through unmodified.
fn render_property(p: &Property) -> String {
    let mut line = p.name.clone();
    if let Some(params) = &p.params {
        for (k, vals) in params {
            line.push(';');
            line.push_str(k);
            line.push('=');
            line.push_str(&vals.join(","));
        }
    }
    line.push(':');
    line.push_str(p.value.as_deref().unwrap_or(""));
    ics_fold(&line)
}

fn render_alarm(alarm: &IcalAlarm) -> String {
    let mut s = String::from("BEGIN:VALARM\r\n");
    for p in &alarm.properties {
        s.push_str(&render_property(p));
    }
    s.push_str("END:VALARM\r\n");
    s
}

/// Fields carried across a GET-merge-PUT edit that `EventDraft` doesn't
/// model. `None`/empty on create.
struct EventCtx {
    uid: String,
    organizer_raw: Option<String>,
    alarms_raw: String,
    sequence: Option<u32>,
    /// Existing attendees' PARTSTAT, keyed by lowercased email — preserved
    /// so an unrelated edit (e.g. a title change) doesn't reset every
    /// guest's RSVP to NEEDS-ACTION.
    existing_partstat: HashMap<String, String>,
}

impl EventCtx {
    fn for_create(uid: String) -> Self {
        Self { uid, organizer_raw: None, alarms_raw: String::new(), sequence: None, existing_partstat: HashMap::new() }
    }

    fn for_edit(existing: &IcalEvent) -> Self {
        let uid = ical_value(&existing.properties, "UID").unwrap_or_default().to_string();
        let organizer_raw = existing.properties.iter().find(|p| p.name == "ORGANIZER").map(render_property);
        let alarms_raw = existing.alarms.iter().map(render_alarm).collect();
        let sequence = existing
            .properties
            .iter()
            .find(|p| p.name == "SEQUENCE")
            .and_then(|p| p.value.as_deref())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
            + 1;

        let mut existing_partstat = HashMap::new();
        for p in existing.properties.iter().filter(|p| p.name == "ATTENDEE") {
            let Some(email) = p.value.as_deref() else { continue };
            let email = strip_mailto(email).to_lowercase();
            let partstat = p
                .params
                .as_ref()
                .and_then(|params| params.iter().find(|(k, _)| k == "PARTSTAT"))
                .and_then(|(_, v)| v.first().cloned())
                .unwrap_or_else(|| "NEEDS-ACTION".to_string());
            existing_partstat.insert(email, partstat);
        }

        Self { uid, organizer_raw, alarms_raw, sequence: Some(sequence), existing_partstat }
    }
}

fn build_vevent_ics(draft: &EventDraft, ctx: &EventCtx) -> String {
    let mut ics = String::new();
    ics.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//waycal//caldav//EN\r\nBEGIN:VEVENT\r\n");
    ics.push_str(&ics_fold(&format!("UID:{}", ctx.uid)));
    ics.push_str(&ics_fold(&format!("DTSTAMP:{}", Utc::now().format("%Y%m%dT%H%M%SZ"))));

    if draft.all_day {
        ics.push_str(&ics_fold(&format!("DTSTART;VALUE=DATE:{}", draft.start_date.format("%Y%m%d"))));
        // All-day ends are exclusive, same convention as Google's API.
        ics.push_str(&ics_fold(&format!("DTEND;VALUE=DATE:{}", (draft.end_date + Days::new(1)).format("%Y%m%d"))));
    } else {
        let start = draft.start_time.expect("timed event needs start_time").with_timezone(&Utc);
        let end = draft.end_time.expect("timed event needs end_time").with_timezone(&Utc);
        ics.push_str(&ics_fold(&format!("DTSTART:{}", start.format("%Y%m%dT%H%M%SZ"))));
        ics.push_str(&ics_fold(&format!("DTEND:{}", end.format("%Y%m%dT%H%M%SZ"))));
    }

    ics.push_str(&ics_line("SUMMARY", &draft.summary));
    if !draft.location.is_empty() {
        ics.push_str(&ics_line("LOCATION", &draft.location));
    }
    if !draft.description.is_empty() {
        ics.push_str(&ics_line("DESCRIPTION", &draft.description));
    }
    for email in &draft.attendees {
        let partstat = ctx.existing_partstat.get(&email.to_lowercase()).map(String::as_str).unwrap_or("NEEDS-ACTION");
        let rsvp = if partstat == "NEEDS-ACTION" { ";RSVP=TRUE" } else { "" };
        ics.push_str(&ics_fold(&format!("ATTENDEE;PARTSTAT={partstat}{rsvp}:mailto:{email}")));
    }
    if let Some(organizer) = &ctx.organizer_raw {
        ics.push_str(organizer);
    }
    if let Some(seq) = ctx.sequence {
        ics.push_str(&ics_fold(&format!("SEQUENCE:{seq}")));
    }
    ics.push_str(&ctx.alarms_raw);
    ics.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    ics
}

fn build_vtodo_ics(draft: &TaskDraft, uid: &str, status: &str, extra: &str) -> String {
    let mut ics = String::new();
    ics.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//waycal//caldav//EN\r\nBEGIN:VTODO\r\n");
    ics.push_str(&ics_fold(&format!("UID:{uid}")));
    ics.push_str(&ics_fold(&format!("DTSTAMP:{}", Utc::now().format("%Y%m%dT%H%M%SZ"))));
    ics.push_str(&ics_line("SUMMARY", &draft.title));
    if !draft.notes.is_empty() {
        ics.push_str(&ics_line("DESCRIPTION", &draft.notes));
    }
    if let Some(due) = draft.due {
        ics.push_str(&ics_fold(&format!("DUE;VALUE=DATE:{}", due.format("%Y%m%d"))));
    }
    ics.push_str(&ics_fold(&format!("STATUS:{status}")));
    ics.push_str(extra);
    ics.push_str("END:VTODO\r\nEND:VCALENDAR\r\n");
    ics
}

// ---------------------------------------------------------------------
// GET-before-mutate helpers
// ---------------------------------------------------------------------

fn get_event(nc: &Nc, href: &str) -> Result<IcalEvent> {
    let (status, body) = request(nc, "GET", href, None, String::new())?;
    if !ok(status) {
        return Err(format!("GET {href}: HTTP {status}"));
    }
    parse_first_event(&body).ok_or_else(|| format!("GET {href}: no VEVENT in response"))
}

fn get_todo(nc: &Nc, href: &str) -> Result<IcalTodo> {
    let (status, body) = request(nc, "GET", href, None, String::new())?;
    if !ok(status) {
        return Err(format!("GET {href}: HTTP {status}"));
    }
    parse_first_todo(&body).ok_or_else(|| format!("GET {href}: no VTODO in response"))
}

fn put_ics(nc: &Nc, href: &str, ics: String) -> Result<()> {
    let (status, _) = request(nc, "PUT", href, Some("text/calendar; charset=utf-8"), ics)?;
    if ok(status) { Ok(()) } else { Err(format!("PUT {href}: HTTP {status}")) }
}

fn delete(nc: &Nc, href: &str) -> Result<()> {
    let (status, _) = request(nc, "DELETE", href, None, String::new())?;
    // A 404 on delete means it's already gone — treat as success.
    if ok(status) || status == 404 { Ok(()) } else { Err(format!("DELETE {href}: HTTP {status}")) }
}

fn new_item_href(collection_href: &str, uid: &str) -> String {
    if collection_href.ends_with('/') { format!("{collection_href}{uid}.ics") } else { format!("{collection_href}/{uid}.ics") }
}

pub struct NextcloudBackend;

impl Backend for NextcloudBackend {
    fn fetch_account(&self, account: &Account, from: NaiveDate, to: NaiveDate, _hide_types: &[String], errors: &mut Vec<String>) -> AccountData {
        // hide_types (Google's birthday/working-location pseudo-event
        // filter) has no CalDAV equivalent — nothing to filter here.
        fetch_account(account, from, to, errors)
    }

    fn insert_event(&self, account: &Account, calendar_id: &str, draft: &EventDraft, _notify: bool) -> Result<()> {
        // CalDAV has no per-request equivalent to `sendUpdates`; Nextcloud's
        // own RFC 6638 scheduling (if enabled) governs attendee emails
        // server-side. `draft.add_meet` is also a no-op: no Nextcloud/Talk
        // integration is implemented.
        let nc = nc(account)?;
        let uid = Uuid::new_v4().to_string();
        let ics = build_vevent_ics(draft, &EventCtx::for_create(uid.clone()));
        put_ics(&nc, &new_item_href(calendar_id, &uid), ics)
    }

    fn patch_event(&self, account: &Account, _calendar_id: &str, event_id: &str, draft: &EventDraft, _notify: bool) -> Result<()> {
        let nc = nc(account)?;
        let existing = get_event(&nc, event_id)?;
        let ctx = EventCtx::for_edit(&existing);
        let ics = build_vevent_ics(draft, &ctx);
        put_ics(&nc, event_id, ics)
    }

    fn delete_event(&self, account: &Account, _calendar_id: &str, event_id: &str) -> Result<()> {
        delete(&nc(account)?, event_id)
    }

    fn insert_task(&self, account: &Account, tasklist_id: &str, draft: &TaskDraft) -> Result<()> {
        let nc = nc(account)?;
        let uid = Uuid::new_v4().to_string();
        let ics = build_vtodo_ics(draft, &uid, "NEEDS-ACTION", "");
        put_ics(&nc, &new_item_href(tasklist_id, &uid), ics)
    }

    /// Full-resource replace (CalDAV has no partial update): forces
    /// NEEDS-ACTION on every save, mirroring `gws.rs`'s `update_task`.
    fn update_task(&self, account: &Account, _tasklist_id: &str, task_id: &str, draft: &TaskDraft) -> Result<()> {
        let nc = nc(account)?;
        let existing = get_todo(&nc, task_id)?;
        let uid = ical_value(&existing.properties, "UID").unwrap_or_default().to_string();
        let ics = build_vtodo_ics(draft, &uid, "NEEDS-ACTION", "");
        put_ics(&nc, task_id, ics)
    }

    fn complete_task(&self, account: &Account, tasklist_id: &str, task_id: &str) -> Result<()> {
        let nc = nc(account)?;
        let existing = get_todo(&nc, task_id)?;
        let uid = ical_value(&existing.properties, "UID").unwrap_or_default().to_string();
        let Some(task) = Task::from_ical(&account.name, tasklist_id, "", task_id, &existing) else {
            return Err(format!("[{}] cannot parse existing task at {task_id}", account.name));
        };
        let draft = TaskDraft { title: task.title, notes: task.notes.unwrap_or_default(), due: task.due };
        let now = Utc::now().format("%Y%m%dT%H%M%SZ");
        let extra = format!("{}{}", ics_fold(&format!("COMPLETED:{now}")), ics_fold("PERCENT-COMPLETE:100"));
        let ics = build_vtodo_ics(&draft, &uid, "COMPLETED", &extra);
        put_ics(&nc, task_id, ics)
    }

    fn delete_task(&self, account: &Account, _tasklist_id: &str, task_id: &str) -> Result<()> {
        delete(&nc(account)?, task_id)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn fold_short_line_unchanged() {
        assert_eq!(ics_fold("SUMMARY:short"), "SUMMARY:short\r\n");
    }

    #[test]
    fn fold_respects_75_octet_limit_and_rejoins() {
        let value = "x".repeat(200);
        let line = format!("DESCRIPTION:{value}");
        let folded = ics_fold(&line);
        let physical: Vec<&str> = folded.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert!(physical.len() > 1, "expected the line to actually fold");
        for l in &physical {
            assert!(l.len() <= 75, "line too long: {} bytes", l.len());
        }
        let rejoined: String =
            physical.iter().enumerate().map(|(i, l)| if i == 0 { *l } else { l.strip_prefix(' ').unwrap_or(l) }).collect();
        assert_eq!(rejoined, line);
    }

    #[test]
    fn fold_never_splits_multibyte_utf8() {
        let value = "€".repeat(40); // 3 bytes each, 120 bytes total
        let line = format!("DESCRIPTION:{value}");
        let folded = ics_fold(&line);
        for physical in folded.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(std::str::from_utf8(physical.as_bytes()).is_ok(), "fold split a multi-byte character");
        }
    }

    #[test]
    fn escape_produces_expected_wire_format() {
        assert_eq!(ics_escape("a,b;c\\d\ne"), "a\\,b\\;c\\\\d\\ne");
    }

    fn timed_draft() -> EventDraft {
        let start = Local.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).unwrap();
        let end = Local.with_ymd_and_hms(2026, 9, 1, 15, 0, 0).unwrap();
        EventDraft {
            summary: "Team sync, weekly".to_string(),
            location: "Room A; B".to_string(),
            description: "Line1\nLine2".to_string(),
            all_day: false,
            start_date: start.date_naive(),
            end_date: end.date_naive(),
            start_time: Some(start),
            end_time: Some(end),
            attendees: vec!["a@example.com".to_string()],
            add_meet: false,
        }
    }

    #[test]
    fn vevent_round_trips_through_parser() {
        let draft = timed_draft();
        let ics = build_vevent_ics(&draft, &EventCtx::for_create("test-uid".to_string()));
        let parsed = parse_first_event(&ics).unwrap();
        let ev = Event::from_ical("acct", "cal1", "Cal", "/href.ics", &parsed).unwrap();
        assert_eq!(ev.summary, draft.summary);
        assert_eq!(ev.location.as_deref(), Some(draft.location.as_str()));
        assert_eq!(ev.description.as_deref(), Some(draft.description.as_str()));
        assert_eq!(ev.attendees.len(), 1);
        assert_eq!(ev.attendees[0].email, "a@example.com");
        assert_eq!(ev.start_time.unwrap().with_timezone(&Utc), draft.start_time.unwrap().with_timezone(&Utc));
    }

    #[test]
    fn vevent_all_day_round_trip() {
        let draft = EventDraft {
            summary: "Holiday".into(),
            location: String::new(),
            description: String::new(),
            all_day: true,
            start_date: NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
            end_date: NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
            start_time: None,
            end_time: None,
            attendees: vec![],
            add_meet: false,
        };
        let ics = build_vevent_ics(&draft, &EventCtx::for_create("uid2".into()));
        let parsed = parse_first_event(&ics).unwrap();
        let ev = Event::from_ical("acct", "cal1", "Cal", "/h.ics", &parsed).unwrap();
        assert!(ev.all_day);
        assert_eq!(ev.start_date, draft.start_date);
        assert_eq!(ev.end_date, draft.end_date);
        assert!(ev.location.is_none());
        assert!(ev.description.is_none());
    }

    #[test]
    fn vevent_edit_preserves_existing_partstat() {
        let mut ctx = EventCtx::for_create("uid-edit".to_string());
        ctx.existing_partstat.insert("a@example.com".to_string(), "ACCEPTED".to_string());
        let draft = timed_draft();
        let ics = build_vevent_ics(&draft, &ctx);
        let parsed = parse_first_event(&ics).unwrap();
        let ev = Event::from_ical("acct", "cal1", "Cal", "/h.ics", &parsed).unwrap();
        assert_eq!(ev.attendees[0].status, "accepted");
    }

    #[test]
    fn vtodo_round_trip() {
        let draft = TaskDraft { title: "Buy milk, eggs".into(), notes: "urgent\nplease".into(), due: Some(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()) };
        let ics = build_vtodo_ics(&draft, "uid3", "NEEDS-ACTION", "");
        let parsed = parse_first_todo(&ics).unwrap();
        let t = Task::from_ical("acct", "list1", "List", "/t.ics", &parsed).unwrap();
        assert_eq!(t.title, draft.title);
        assert_eq!(t.notes.as_deref(), Some(draft.notes.as_str()));
        assert_eq!(t.due, draft.due);
        assert!(!t.completed);
    }

    #[test]
    fn vtodo_completed_status_round_trip() {
        let draft = TaskDraft { title: "Done thing".into(), notes: String::new(), due: None };
        let extra = format!("{}{}", ics_fold("COMPLETED:20260901T000000Z"), ics_fold("PERCENT-COMPLETE:100"));
        let ics = build_vtodo_ics(&draft, "uid4", "COMPLETED", &extra);
        let parsed = parse_first_todo(&ics).unwrap();
        let t = Task::from_ical("acct", "list1", "List", "/t.ics", &parsed).unwrap();
        assert!(t.completed);
    }
}
