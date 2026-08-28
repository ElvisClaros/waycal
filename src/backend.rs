//! Provider-agnostic calendar/tasks backend. `GoogleBackend` (`gws.rs`,
//! talking to the `gws` CLI) and `NextcloudBackend` (`caldav.rs`, talking
//! CalDAV) are the two implementations; the UI never sees either directly.

use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::config::{Account, Provider};
use crate::model::{Calendar, Event, Task, TaskList};

pub type Result<T> = std::result::Result<T, String>;

/// Everything waycal shows for one account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountData {
    pub calendars: Vec<Calendar>,
    pub events: Vec<Event>,
    pub tasklists: Vec<TaskList>,
    pub tasks: Vec<Task>,
}

/// Event fields for create/edit, independent of any provider's wire format.
pub struct EventDraft {
    pub summary: String,
    pub location: String,
    pub description: String,
    pub all_day: bool,
    pub start_date: NaiveDate,
    /// Inclusive, matching `Event::end_date`.
    pub end_date: NaiveDate,
    pub start_time: Option<DateTime<Local>>,
    pub end_time: Option<DateTime<Local>>,
    /// Guest emails. On edit this replaces the full guest list (empty
    /// clears it); on create it's only applied when non-empty.
    pub attendees: Vec<String>,
    pub add_meet: bool,
}

/// Task fields for create/edit, independent of any provider's wire format.
pub struct TaskDraft {
    pub title: String,
    /// Empty means no notes.
    pub notes: String,
    pub due: Option<NaiveDate>,
}

/// One calendar/tasks provider account talks to.
pub trait Backend {
    fn fetch_account(
        &self,
        account: &Account,
        from: NaiveDate,
        to: NaiveDate,
        hide_types: &[String],
        errors: &mut Vec<String>,
    ) -> AccountData;

    fn insert_event(&self, account: &Account, calendar_id: &str, draft: &EventDraft, notify: bool) -> Result<()>;
    fn patch_event(
        &self,
        account: &Account,
        calendar_id: &str,
        event_id: &str,
        draft: &EventDraft,
        notify: bool,
    ) -> Result<()>;
    fn delete_event(&self, account: &Account, calendar_id: &str, event_id: &str) -> Result<()>;

    fn insert_task(&self, account: &Account, tasklist_id: &str, draft: &TaskDraft) -> Result<()>;
    fn update_task(&self, account: &Account, tasklist_id: &str, task_id: &str, draft: &TaskDraft) -> Result<()>;
    fn complete_task(&self, account: &Account, tasklist_id: &str, task_id: &str) -> Result<()>;
    fn delete_task(&self, account: &Account, tasklist_id: &str, task_id: &str) -> Result<()>;
}

/// Picks the backend implementation for an account.
pub fn for_account(account: &Account) -> &'static dyn Backend {
    match account.provider {
        Provider::Google => &crate::gws::GoogleBackend,
        Provider::Nextcloud => &crate::caldav::NextcloudBackend,
    }
}
