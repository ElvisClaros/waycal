use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Google,
    Nextcloud,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub provider: Provider,
    #[serde(default = "default_account_color")]
    pub color: String,

    // Google (gws) — required iff provider = Google.
    pub config_dir: Option<String>,
    pub credentials_file: Option<String>,

    // Nextcloud (CalDAV) — required iff provider = Nextcloud. The app
    // password is read from a file (like `credentials_file`) rather than
    // stored inline in config.toml.
    pub server_url: Option<String>,
    pub username: Option<String>,
    pub app_password_file: Option<String>,
}

fn default_account_color() -> String {
    "#8FBC8F".to_string()
}

/// Checks that each account carries the fields its provider needs. Needed
/// because those fields are `Option` (shared across providers) rather than
/// plain required fields serde would reject for free.
fn validate(accounts: &[Account]) -> Result<(), String> {
    for a in accounts {
        let missing = match a.provider {
            Provider::Google => [("config_dir", &a.config_dir), ("credentials_file", &a.credentials_file)]
                .into_iter()
                .find(|(_, v)| v.as_deref().unwrap_or("").is_empty()),
            Provider::Nextcloud => [
                ("server_url", &a.server_url),
                ("username", &a.username),
                ("app_password_file", &a.app_password_file),
            ]
            .into_iter()
            .find(|(_, v)| v.as_deref().unwrap_or("").is_empty()),
        };
        if let Some((field, _)) = missing {
            return Err(format!("account '{}' (provider={:?}) is missing required field '{field}'", a.name, a.provider));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_reminder_mins")]
    pub default_reminder_mins: i64,
    /// "HH:MM" local time for the daily task digest; None disables it.
    #[serde(default)]
    pub task_digest_time: Option<String>,
    #[serde(default = "default_hidden_event_types")]
    pub hide_event_types: Vec<String>,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

/// App-wide colors. Any CSS color syntax works (hex, `rgb()`, named colors)
/// since these are injected as GTK CSS `@define-color` values.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub background: String,
    pub input_background: String,
    pub text: String,
    pub dim_text: String,
    pub accent: String,
    pub error: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: "#1a2125".to_string(),
            input_background: "#232c31".to_string(),
            text: "#c9d1d9".to_string(),
            dim_text: "#6a7a71".to_string(),
            accent: "#8FBC8F".to_string(),
            error: "#e06c75".to_string(),
        }
    }
}

fn default_poll_interval() -> u64 {
    300
}

fn default_reminder_mins() -> i64 {
    10
}

fn default_hidden_event_types() -> Vec<String> {
    vec!["workingLocation".into(), "birthday".into()]
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    PathBuf::from(path)
}

pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_tilde("~/.config"))
        .join("waycal/config.toml")
}

/// Loads the config file. Returns None when it doesn't exist (plain
/// calendar mode); parse errors are reported so a typo doesn't silently
/// disable the panels.
pub fn load() -> Option<Config> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "waycal: no config at {} — running plain calendar. \
                 Add [[accounts]] entries there to enable Google Calendar/Tasks.",
                path.display()
            );
            return None;
        }
        Err(e) => {
            eprintln!("waycal: cannot read {}: {}", path.display(), e);
            return None;
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(cfg) if cfg.accounts.is_empty() => {
            eprintln!("waycal: {} has no [[accounts]] — running plain calendar", path.display());
            None
        }
        Ok(cfg) => match validate(&cfg.accounts) {
            Ok(()) => Some(cfg),
            Err(e) => {
                eprintln!("waycal: invalid config {}: {}", path.display(), e);
                None
            }
        },
        Err(e) => {
            eprintln!("waycal: invalid config {}: {}", path.display(), e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_only_config_without_provider_key_still_parses() {
        let toml = r#"
            [[accounts]]
            name = "celvisc"
            config_dir = "~/.config/gws-celvisc"
            credentials_file = "~/.config/gwc-conf/celvisc-credentials.json"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].provider, Provider::Google);
        assert!(validate(&cfg.accounts).is_ok());
    }

    #[test]
    fn nextcloud_account_missing_field_fails_validation() {
        let toml = r#"
            [[accounts]]
            name = "personal-nc"
            provider = "nextcloud"
            server_url = "https://cloud.example.com"
            username = "alice"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let err = validate(&cfg.accounts).unwrap_err();
        assert!(err.contains("app_password_file"), "unexpected error: {err}");
    }

    #[test]
    fn complete_nextcloud_account_validates() {
        let toml = r#"
            [[accounts]]
            name = "personal-nc"
            provider = "nextcloud"
            server_url = "https://cloud.example.com"
            username = "alice"
            app_password_file = "~/.config/waycal/nextcloud-app-password"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.accounts[0].provider, Provider::Nextcloud);
        assert!(validate(&cfg.accounts).is_ok());
    }
}
