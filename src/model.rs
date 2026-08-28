use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use ical::parser::ical::component::{IcalAlarm, IcalEvent, IcalTodo};
use ical::property::Property;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calendar {
    pub account: String,
    pub id: String,
    pub summary: String,
    pub access_role: String,
    /// Minutes-before values of the calendar's default popup reminders.
    pub default_reminder_mins: Vec<i64>,
    pub primary: bool,
}

impl Calendar {
    pub fn writable(&self) -> bool {
        self.access_role == "owner" || self.access_role == "writer"
    }

    pub fn from_json(account: &str, v: &Value) -> Option<Self> {
        Some(Self {
            account: account.to_string(),
            id: v.get("id")?.as_str()?.to_string(),
            summary: v.get("summary").and_then(Value::as_str).unwrap_or("").to_string(),
            access_role: v.get("accessRole").and_then(Value::as_str).unwrap_or("reader").to_string(),
            default_reminder_mins: v
                .get("defaultReminders")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter(|r| r.get("method").and_then(Value::as_str) == Some("popup"))
                        .filter_map(|r| r.get("minutes").and_then(Value::as_i64))
                        .collect()
                })
                .unwrap_or_default(),
            primary: v.get("primary").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    /// accepted | declined | tentative | needsAction
    pub status: String,
    pub organizer: bool,
    pub is_self: bool,
}

impl Attendee {
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub account: String,
    pub calendar_id: String,
    pub calendar_name: String,
    pub id: String,
    pub summary: String,
    pub all_day: bool,
    /// First day the event is shown on (local).
    pub start_date: NaiveDate,
    /// Last day the event is shown on, inclusive (all-day ends are exclusive in the API).
    pub end_date: NaiveDate,
    pub start_time: Option<DateTime<Local>>,
    pub end_time: Option<DateTime<Local>>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub meet_url: Option<String>,
    pub html_link: Option<String>,
    /// Explicit popup reminder overrides; None means "use calendar defaults".
    pub reminder_overrides: Option<Vec<i64>>,
    /// Guests, organizer first (default keeps old caches deserializable).
    #[serde(default)]
    pub attendees: Vec<Attendee>,
}

fn parse_event_boundary(v: &Value) -> Option<(NaiveDate, Option<DateTime<Local>>)> {
    if let Some(dt) = v.get("dateTime").and_then(Value::as_str) {
        let parsed = DateTime::parse_from_rfc3339(dt).ok()?.with_timezone(&Local);
        return Some((parsed.date_naive(), Some(parsed)));
    }
    let d = v.get("date").and_then(Value::as_str)?;
    Some((NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?, None))
}

impl Event {
    pub fn from_json(account: &str, calendar_id: &str, calendar_name: &str, v: &Value) -> Option<Self> {
        if v.get("status").and_then(Value::as_str) == Some("cancelled") {
            return None;
        }
        let (start_date, start_time) = parse_event_boundary(v.get("start")?)?;
        let (end_date, end_time) = parse_event_boundary(v.get("end")?)?;
        let all_day = start_time.is_none();
        // All-day ends are exclusive: a one-day event has end = start + 1 day.
        let end_date = if all_day { end_date.pred_opt().unwrap_or(end_date).max(start_date) } else { end_date };

        let meet_url = v
            .get("hangoutLink")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                v.get("conferenceData")?
                    .get("entryPoints")?
                    .as_array()?
                    .iter()
                    .find(|e| e.get("entryPointType").and_then(Value::as_str) == Some("video"))?
                    .get("uri")?
                    .as_str()
                    .map(str::to_string)
            });

        let reminder_overrides = match v.get("reminders") {
            Some(r) if r.get("useDefault").and_then(Value::as_bool) == Some(false) => Some(
                r.get("overrides")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter(|o| o.get("method").and_then(Value::as_str) == Some("popup"))
                            .filter_map(|o| o.get("minutes").and_then(Value::as_i64))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            _ => None,
        };

        let mut attendees: Vec<Attendee> = v
            .get("attendees")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    // Meeting rooms and other resources aren't people.
                    .filter(|g| g.get("resource").and_then(Value::as_bool) != Some(true))
                    .filter_map(|g| {
                        Some(Attendee {
                            email: g.get("email")?.as_str()?.to_string(),
                            name: g.get("displayName").and_then(Value::as_str).map(str::to_string),
                            status: g
                                .get("responseStatus")
                                .and_then(Value::as_str)
                                .unwrap_or("needsAction")
                                .to_string(),
                            organizer: g.get("organizer").and_then(Value::as_bool).unwrap_or(false),
                            is_self: g.get("self").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        attendees.sort_by_key(|a| (!a.organizer, !a.is_self));

        let non_empty = |key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        Some(Self {
            account: account.to_string(),
            calendar_id: calendar_id.to_string(),
            calendar_name: calendar_name.to_string(),
            id: v.get("id")?.as_str()?.to_string(),
            summary: v
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("(no title)")
                .to_string(),
            all_day,
            start_date,
            end_date,
            start_time,
            end_time,
            location: non_empty("location"),
            description: non_empty("description"),
            meet_url,
            html_link: non_empty("htmlLink"),
            reminder_overrides,
            attendees,
        })
    }

    pub fn covers(&self, day: NaiveDate) -> bool {
        self.start_date <= day && day <= self.end_date
    }

    pub fn time_label(&self) -> String {
        match (self.start_time, self.end_time) {
            (Some(s), Some(e)) => format!("{}–{}", s.format("%H:%M"), e.format("%H:%M")),
            (Some(s), None) => s.format("%H:%M").to_string(),
            _ => "all day".to_string(),
        }
    }

    /// Builds an event from a parsed CalDAV VEVENT. `href` (the item's CalDAV
    /// path) becomes `id`, mirroring Google's opaque event id.
    pub fn from_ical(account: &str, calendar_id: &str, calendar_name: &str, href: &str, ev: &IcalEvent) -> Option<Self> {
        let (start_date, start_time) = parse_ical_moment(ical_prop(&ev.properties, "DTSTART")?)?;
        let (end_date, end_time) = match ical_prop(&ev.properties, "DTEND") {
            Some(p) => parse_ical_moment(p)?,
            None => (start_date, start_time),
        };
        let all_day = start_time.is_none();
        // All-day ends are exclusive, same convention as Google's API.
        let end_date = if all_day { end_date.pred_opt().unwrap_or(end_date).max(start_date) } else { end_date };

        let reminder_overrides = if ev.alarms.is_empty() {
            None
        } else {
            let mins: Vec<i64> = ev.alarms.iter().filter_map(alarm_minutes).collect();
            if mins.is_empty() { None } else { Some(mins) }
        };

        let organizer_email = ical_value(&ev.properties, "ORGANIZER").map(strip_mailto);
        let attendees: Vec<Attendee> = ev
            .properties
            .iter()
            .filter(|p| p.name == "ATTENDEE")
            .filter_map(|p| {
                let email = strip_mailto(p.value.as_deref()?).to_string();
                let organizer = organizer_email.is_some_and(|o| o.eq_ignore_ascii_case(&email));
                Some(Attendee {
                    email,
                    name: ical_param(p, "CN").map(str::to_string),
                    status: partstat_to_google(ical_param(p, "PARTSTAT")),
                    organizer,
                    // No reliable "is this me" signal at this layer (would need the
                    // account's own email threaded through); left false for CalDAV.
                    is_self: false,
                })
            })
            .collect();

        let non_empty = |name: &str| {
            ical_value(&ev.properties, name)
                .map(ical_unescape)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        Some(Self {
            account: account.to_string(),
            calendar_id: calendar_id.to_string(),
            calendar_name: calendar_name.to_string(),
            id: href.to_string(),
            summary: ical_value(&ev.properties, "SUMMARY")
                .map(ical_unescape)
                .unwrap_or_else(|| "(no title)".to_string()),
            all_day,
            start_date,
            end_date,
            start_time,
            end_time,
            location: non_empty("LOCATION"),
            description: non_empty("DESCRIPTION"),
            meet_url: None, // no CalDAV equivalent
            html_link: None,
            reminder_overrides,
            attendees,
        })
    }
}

fn ical_prop<'a>(props: &'a [Property], name: &str) -> Option<&'a Property> {
    props.iter().find(|p| p.name == name)
}

pub(crate) fn ical_value<'a>(props: &'a [Property], name: &str) -> Option<&'a str> {
    ical_prop(props, name)?.value.as_deref()
}

fn ical_param<'a>(p: &'a Property, key: &str) -> Option<&'a str> {
    p.params.as_ref()?.iter().find(|(k, _)| k == key)?.1.first().map(String::as_str)
}

pub(crate) fn strip_mailto(value: &str) -> &str {
    value.strip_prefix("mailto:").or_else(|| value.strip_prefix("MAILTO:")).unwrap_or(value)
}

/// Undoes RFC 5545 §3.3.11 TEXT escaping. The `ical` crate is a low-level
/// line parser and does not do this itself.
fn ical_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other), // \\, \, and \; all resolve to the literal char
            None => {}
        }
    }
    out
}

/// Maps a CalDAV PARTSTAT to the lowercase strings the UI matches on (see
/// `ui/detail.rs`), which follow Google's own `responseStatus` casing.
fn partstat_to_google(partstat: Option<&str>) -> String {
    match partstat {
        Some("ACCEPTED") => "accepted",
        Some("DECLINED") => "declined",
        Some("TENTATIVE") => "tentative",
        _ => "needsAction",
    }
    .to_string()
}

/// Parses an RFC 5545 DATE or DATE-TIME property value (DTSTART/DTEND/DUE).
/// Resolves a `TZID` param via the IANA database (`chrono-tz`); falls back
/// to floating/local time when there's neither a `Z` suffix nor a
/// recognized `TZID` (covers the common case — servers emitting a bespoke
/// non-IANA `TZID` with only an embedded `VTIMEZONE` are not handled).
fn parse_ical_moment(p: &Property) -> Option<(NaiveDate, Option<DateTime<Local>>)> {
    let value = p.value.as_deref()?;
    if ical_param(p, "VALUE") == Some("DATE") || (value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit())) {
        return Some((NaiveDate::parse_from_str(value, "%Y%m%d").ok()?, None));
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        let local = Utc.from_utc_datetime(&naive).with_timezone(&Local);
        return Some((local.date_naive(), Some(local)));
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    if let Some(tz) = ical_param(p, "TZID").and_then(|id| id.parse::<Tz>().ok()) {
        let local = tz.from_local_datetime(&naive).earliest()?.with_timezone(&Local);
        return Some((local.date_naive(), Some(local)));
    }
    let local = Local.from_local_datetime(&naive).earliest()?;
    Some((local.date_naive(), Some(local)))
}

/// Parses an RFC 5545 duration (e.g. `-PT10M`, `-PT1H`, `-P1D`, `PT15M`)
/// into signed minutes.
fn parse_ical_duration_minutes(s: &str) -> Option<i64> {
    let neg = s.starts_with('-');
    let s = s.trim_start_matches(['+', '-']).strip_prefix('P')?;
    if let Some(weeks) = s.strip_suffix('W') {
        let w: i64 = weeks.parse().ok()?;
        return Some(if neg { -w * 7 * 24 * 60 } else { w * 7 * 24 * 60 });
    }
    let (date_part, time_part) = s.split_once('T').map(|(d, t)| (d, Some(t))).unwrap_or((s, None));
    let mut minutes = if date_part.is_empty() { 0 } else { date_part.strip_suffix('D')?.parse::<i64>().ok()? * 24 * 60 };
    if let Some(mut rest) = time_part {
        if let Some(idx) = rest.find('H') {
            minutes += rest[..idx].parse::<i64>().ok()? * 60;
            rest = &rest[idx + 1..];
        }
        if let Some(idx) = rest.find('M') {
            minutes += rest[..idx].parse::<i64>().ok()?;
        }
    }
    Some(if neg { -minutes } else { minutes })
}

/// Minutes before the event start for one VALARM, or None for alarm types
/// waycal doesn't surface as a popup reminder (email/procedure alarms,
/// absolute-datetime triggers).
fn alarm_minutes(alarm: &IcalAlarm) -> Option<i64> {
    match ical_value(&alarm.properties, "ACTION") {
        Some("DISPLAY") | Some("AUDIO") => {}
        _ => return None,
    }
    let trigger = ical_prop(&alarm.properties, "TRIGGER")?;
    if ical_param(trigger, "VALUE") == Some("DATE-TIME") {
        return None;
    }
    Some(-parse_ical_duration_minutes(trigger.value.as_deref()?)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskList {
    pub account: String,
    pub id: String,
    pub title: String,
}

impl TaskList {
    pub fn from_json(account: &str, v: &Value) -> Option<Self> {
        Some(Self {
            account: account.to_string(),
            id: v.get("id")?.as_str()?.to_string(),
            title: v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub account: String,
    pub tasklist_id: String,
    pub tasklist_title: String,
    pub id: String,
    pub title: String,
    pub notes: Option<String>,
    /// Google Tasks due dates carry no meaningful time component.
    pub due: Option<NaiveDate>,
    pub completed: bool,
}

impl Task {
    pub fn from_json(account: &str, tasklist_id: &str, tasklist_title: &str, v: &Value) -> Option<Self> {
        let due = v
            .get("due")
            .and_then(Value::as_str)
            .and_then(|d| NaiveDate::parse_from_str(&d[..10.min(d.len())], "%Y-%m-%d").ok());
        Some(Self {
            account: account.to_string(),
            tasklist_id: tasklist_id.to_string(),
            tasklist_title: tasklist_title.to_string(),
            id: v.get("id")?.as_str()?.to_string(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("(untitled)")
                .to_string(),
            notes: v
                .get("notes")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            due,
            completed: v.get("status").and_then(Value::as_str) == Some("completed"),
        })
    }

    /// Builds a task from a parsed CalDAV VTODO. `href` becomes `id`.
    pub fn from_ical(account: &str, tasklist_id: &str, tasklist_title: &str, href: &str, todo: &IcalTodo) -> Option<Self> {
        let due = ical_prop(&todo.properties, "DUE").and_then(parse_ical_moment).map(|(d, _)| d);
        Some(Self {
            account: account.to_string(),
            tasklist_id: tasklist_id.to_string(),
            tasklist_title: tasklist_title.to_string(),
            id: href.to_string(),
            title: ical_value(&todo.properties, "SUMMARY")
                .map(ical_unescape)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(untitled)".to_string()),
            notes: ical_value(&todo.properties, "DESCRIPTION")
                .map(ical_unescape)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            due,
            completed: ical_value(&todo.properties, "STATUS") == Some("COMPLETED")
                || ical_prop(&todo.properties, "COMPLETED").is_some(),
        })
    }
}

/// Parses "HH:MM" into a NaiveTime (used for task_digest_time).
pub fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_event(vevent: &str) -> IcalEvent {
        let full = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{vevent}END:VCALENDAR\r\n");
        let mut parser = ical::IcalParser::new(std::io::Cursor::new(full.as_bytes()));
        parser.next().unwrap().unwrap().events.into_iter().next().unwrap()
    }

    fn parse_todo(vtodo: &str) -> IcalTodo {
        let full = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{vtodo}END:VCALENDAR\r\n");
        let mut parser = ical::IcalParser::new(std::io::Cursor::new(full.as_bytes()));
        parser.next().unwrap().unwrap().todos.into_iter().next().unwrap()
    }

    #[test]
    fn utc_timed_event() {
        let ev = parse_event(
            "BEGIN:VEVENT\r\nUID:abc\r\nDTSTART:20260901T140000Z\r\nDTEND:20260901T150000Z\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n",
        );
        let e = Event::from_ical("acct", "cal1", "Cal", "/href.ics", &ev).unwrap();
        assert!(!e.all_day);
        assert_eq!(e.summary, "Standup");
        assert_eq!(e.start_time.unwrap().with_timezone(&Utc), Utc.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).unwrap());
    }

    #[test]
    fn tzid_qualified_event() {
        let ev = parse_event(
            "BEGIN:VEVENT\r\nUID:abc\r\nDTSTART;TZID=America/New_York:20260901T100000\r\n\
             DTEND;TZID=America/New_York:20260901T110000\r\nSUMMARY:Call\r\nEND:VEVENT\r\n",
        );
        let e = Event::from_ical("acct", "cal1", "Cal", "/href.ics", &ev).unwrap();
        assert!(!e.all_day);
        // 10:00 America/New_York on 2026-09-01 is EDT (UTC-4) -> 14:00 UTC.
        assert_eq!(e.start_time.unwrap().with_timezone(&Utc), Utc.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).unwrap());
    }

    #[test]
    fn all_day_event() {
        let ev = parse_event(
            "BEGIN:VEVENT\r\nUID:abc\r\nDTSTART;VALUE=DATE:20260901\r\nDTEND;VALUE=DATE:20260902\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\n",
        );
        let e = Event::from_ical("acct", "cal1", "Cal", "/href.ics", &ev).unwrap();
        assert!(e.all_day);
        assert_eq!(e.start_date, NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
        // DTEND is exclusive; a one-day all-day event's inclusive end == start.
        assert_eq!(e.end_date, NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
    }

    #[test]
    fn event_with_alarm() {
        let ev = parse_event(
            "BEGIN:VEVENT\r\nUID:abc\r\nDTSTART:20260901T140000Z\r\nDTEND:20260901T150000Z\r\nSUMMARY:Standup\r\n\
             BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
        );
        let e = Event::from_ical("acct", "cal1", "Cal", "/href.ics", &ev).unwrap();
        assert_eq!(e.reminder_overrides, Some(vec![10]));
    }

    #[test]
    fn escaped_text_is_unescaped() {
        let ev = parse_event(
            "BEGIN:VEVENT\r\nUID:abc\r\nDTSTART:20260901T140000Z\r\nDTEND:20260901T150000Z\r\n\
             SUMMARY:Line1\\nLine2\\, comma\r\nEND:VEVENT\r\n",
        );
        let e = Event::from_ical("acct", "cal1", "Cal", "/href.ics", &ev).unwrap();
        assert_eq!(e.summary, "Line1\nLine2, comma");
    }

    #[test]
    fn completed_and_incomplete_todo() {
        let done = parse_todo("BEGIN:VTODO\r\nUID:t1\r\nSUMMARY:Buy milk\r\nSTATUS:COMPLETED\r\nEND:VTODO\r\n");
        let t = Task::from_ical("acct", "list1", "List", "/t.ics", &done).unwrap();
        assert!(t.completed);

        let open = parse_todo("BEGIN:VTODO\r\nUID:t2\r\nSUMMARY:Buy eggs\r\nSTATUS:NEEDS-ACTION\r\nDUE;VALUE=DATE:20260901\r\nEND:VTODO\r\n");
        let t = Task::from_ical("acct", "list1", "List", "/t2.ics", &open).unwrap();
        assert!(!t.completed);
        assert_eq!(t.due, Some(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()));
    }

    #[test]
    fn alarm_duration_parsing() {
        assert_eq!(parse_ical_duration_minutes("-PT10M"), Some(-10));
        assert_eq!(parse_ical_duration_minutes("-PT1H"), Some(-60));
        assert_eq!(parse_ical_duration_minutes("-P1D"), Some(-1440));
        assert_eq!(parse_ical_duration_minutes("-P1DT2H30M"), Some(-(1440 + 150)));
        assert_eq!(parse_ical_duration_minutes("PT15M"), Some(15));
    }
}
