//! Headless notification daemon: polls both accounts, fires desktop
//! notifications for Google event reminders and a daily due-task digest.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::process::Command;

use chrono::{DateTime, Days, Duration, Local, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::model::{self, Task, parse_hhmm};
use crate::{backend, cache};

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// "event_id@reminder_ts" → reminder time, for dedup and pruning.
    #[serde(default)]
    notified: BTreeMap<String, DateTime<Local>>,
    #[serde(default)]
    digest_date: Option<NaiveDate>,
    /// Every task id ever observed. Used to fire a live notification the first
    /// time a *new* task appears already due (e.g. one just created on the
    /// phone), without re-pinging tasks that merely roll into "today" at
    /// midnight — those go through the morning digest instead. Grows by union
    /// (never pruned) so a completed task, or a transient fetch failure, can't
    /// make an id look new again.
    #[serde(default)]
    seen_tasks: BTreeSet<String>,
    /// Set once the pre-existing task backlog has been seeded into `seen_tasks`
    /// (on a fully-successful fetch), so live notifications start only for
    /// tasks created afterwards. A persistent flag rather than "is seen_tasks
    /// empty?" so a user with zero tasks still gets pinged on their first one.
    #[serde(default)]
    tasks_seeded: bool,
}

fn state_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| config::expand_tilde("~/.local/state"))
        .join("waycal/notified.json")
}

fn load_state() -> State {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &State) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(state) {
        let _ = std::fs::write(path, text);
    }
}

fn notify(summary: &str, body: &str, open_url: Option<&str>) {
    let mut cmd = Command::new("notify-send");
    // -t 0: never expires, stays until the user clicks or dismisses it.
    cmd.args(["-a", "waycal", "-i", "x-office-calendar", "-t", "0"]);
    if let Some(url) = open_url {
        // With an action registered, notify-send blocks until the
        // notification is closed, so wait it out in a detached thread.
        cmd.args(["-A", "default=Open Meet", summary, body]);
        let url = url.to_string();
        std::thread::spawn(move || match cmd.output() {
            Ok(out) if out.stdout.trim_ascii() == b"default" => {
                let _ = Command::new("xdg-open").arg(url).status();
            }
            Ok(_) => {}
            Err(e) => eprintln!("waycal daemon: notify-send failed: {e}"),
        });
    } else if let Err(e) = cmd.args([summary, body]).status() {
        eprintln!("waycal daemon: notify-send failed: {e}");
    }
}

pub fn run() {
    let Some(cfg) = config::load() else {
        eprintln!("waycal daemon: needs accounts in the config file");
        std::process::exit(1);
    };
    eprintln!(
        "waycal daemon: watching {} account(s), polling every {}s",
        cfg.accounts.len(),
        cfg.poll_interval_secs
    );
    let mut state = load_state();
    loop {
        tick(&cfg, &mut state);
        std::thread::sleep(std::time::Duration::from_secs(cfg.poll_interval_secs.max(30)));
    }
}

fn tick(cfg: &Config, state: &mut State) {
    let now = Local::now();
    let today = now.date_naive();

    // Same window the popup uses, so both share a fresh cache.
    let from = today - Days::new(7);
    let to = today + Days::new(45);
    let mut errors = Vec::new();
    let mut cache = cache::Cache {
        fetched_at: Some(now),
        from: Some(from),
        to: Some(to),
        ..Default::default()
    };
    for account in &cfg.accounts {
        let data = backend::for_account(account).fetch_account(account, from, to, &cfg.hide_event_types, &mut errors);
        cache.accounts.insert(account.name.clone(), data);
    }
    for e in &errors {
        eprintln!("waycal daemon: {e}");
    }
    if errors.len() >= cfg.accounts.len() * 2 {
        // Everything failed (offline?) — don't overwrite a good cache.
        return;
    }
    cache::save(&cache);

    // Event reminders (timed events only; duplicates across accounts collapse by id).
    let mut seen = HashSet::new();
    for data in cache.accounts.values() {
        for ev in &data.events {
            let Some(start) = ev.start_time else { continue };
            if start <= now || !seen.insert(ev.id.clone()) {
                continue;
            }
            // Google reminder semantics: event overrides win; otherwise the
            // calendar's default popup reminders; otherwise our configured fallback.
            let minutes = match &ev.reminder_overrides {
                Some(overrides) => overrides.clone(),
                None => {
                    let defaults = data
                        .calendars
                        .iter()
                        .find(|c| c.id == ev.calendar_id)
                        .map(|c| c.default_reminder_mins.clone())
                        .unwrap_or_default();
                    if defaults.is_empty() { vec![cfg.default_reminder_mins] } else { defaults }
                }
            };
            for m in minutes {
                let remind_at = start - Duration::minutes(m);
                if remind_at > now {
                    continue;
                }
                let key = format!("{}@{}", ev.id, remind_at.timestamp());
                if state.notified.contains_key(&key) {
                    continue;
                }
                let mut body = format!("{} \u{00B7} {}", ev.account, ev.calendar_name);
                if let Some(loc) = &ev.location {
                    body.push_str(&format!("\n{loc}"));
                }
                if let Some(url) = &ev.meet_url {
                    body.push_str(&format!("\n{url}"));
                }
                notify(
                    &format!("{} {}", start.format("%H:%M"), ev.summary),
                    &body,
                    ev.meet_url.as_deref(),
                );
                state.notified.insert(key, remind_at);
            }
        }
    }

    // Timed task reminders: CalDAV tasks carrying a due *time* (and possibly
    // an RRULE) fire at that instant, like events — the recurrence is expanded
    // onto today. Deduped per occurrence date so a daily task rings once a day;
    // if the machine was off at the time, it fires once on the next poll.
    for t in cache.accounts.values().flat_map(|d| d.tasks.iter()) {
        let Some(due_time) = t.due_time else { continue };
        if t.completed || !model::task_occurs_on(due_time.date_naive(), t.rrule.as_deref(), today) {
            continue;
        }
        let Some(occ) = today.and_time(due_time.time()).and_local_timezone(Local).single() else { continue };
        if occ > now {
            continue; // not time yet
        }
        let key = format!("task:{}@{}", t.id, today);
        if state.notified.contains_key(&key) {
            continue;
        }
        notify(
            &format!("{} {}", occ.format("%H:%M"), t.title),
            &format!("{} \u{00B7} {}", t.account, t.tasklist_title),
            None,
        );
        state.notified.insert(key, occ);
    }

    // Whether a *date-only* task (no due time) is actionable today — occurs
    // today per its RRULE, or (non-recurring) is due on/before today. Timed
    // tasks are excluded here; the block above owns them.
    let due_today = |t: &Task| -> bool {
        t.due_time.is_none()
            && t.due.is_some_and(|dd| match t.rrule.as_deref() {
                Some(rr) => model::task_occurs_on(dd, Some(rr), today),
                None => dd <= today,
            })
    };
    let overdue = |t: &Task| -> bool { t.rrule.is_none() && t.due.is_some_and(|dd| dd < today) };

    // Live heads-up: ping once when a *new* date-only task shows up already due
    // (typically one just created on the phone). Dedup by task identity, not by
    // day, so pre-existing tasks rolling into "today" stay silent here and go
    // through the morning digest. Opt-out via `task_live_notify`.
    let seeding = !state.tasks_seeded;
    for t in cache.accounts.values().flat_map(|d| d.tasks.iter()) {
        if !state.seen_tasks.insert(t.id.clone()) {
            continue; // already known — not a newly appeared task
        }
        if seeding || !cfg.task_live_notify || t.completed || t.due_time.is_some() || !due_today(t) {
            continue; // seeding, disabled, done, timed (handled above), or not due
        }
        notify(
            &format!("Task {}: {}", if overdue(t) { "vencida" } else { "para hoy" }, t.title),
            &format!("{} \u{00B7} {}", t.account, t.tasklist_title),
            None,
        );
    }
    // Only close seeding on a clean fetch, so a partial failure doesn't leave
    // an account's backlog to surface as "new" (a flood) once it recovers.
    if seeding && errors.is_empty() {
        state.tasks_seeded = true;
    }

    // Morning digest of date-only tasks due today (timed ones get their own
    // reminder above, so they're left out to avoid a redundant listing).
    if let Some(digest_at) = cfg.task_digest_time.as_deref().and_then(parse_hhmm)
        && now.time() >= digest_at && state.digest_date != Some(today) {
            let mut due: Vec<_> = cache
                .accounts
                .values()
                .flat_map(|d| d.tasks.iter())
                .filter(|t| !t.completed && due_today(t))
                .collect();
            due.sort_by_key(|t| (t.due, t.title.clone()));
            if !due.is_empty() {
                let body: Vec<String> = due
                    .iter()
                    .map(|t| format!("\u{2022} {}{} ({})", t.title, if overdue(t) { " (overdue)" } else { "" }, t.account))
                    .collect();
                notify(&format!("{} task(s) due today", due.len()), &body.join("\n"), None);
            }
            state.digest_date = Some(today);
        }

    let cutoff = now - Duration::days(2);
    state.notified.retain(|_, ts| *ts > cutoff);
    save_state(state);
}
