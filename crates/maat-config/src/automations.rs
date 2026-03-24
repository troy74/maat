use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Datelike, Local, NaiveTime, TimeZone, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use crate::ConfigError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationStatus {
    Active,
    Paused,
}

impl Default for AutomationStatus {
    fn default() -> Self {
        Self::Active
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationSchedule {
    Interval { minutes: u64 },
    Daily { hour: u8, minute: u8 },
    Weekly { weekday: String, hour: u8, minute: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationDelivery {
    Telegram {
        #[serde(default)]
        chat_id: Option<i64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationSpec {
    pub id: String,
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub status: AutomationStatus,
    pub schedule: AutomationSchedule,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub delivery: Option<AutomationDelivery>,
}

pub fn load_automations(dir: &str) -> Result<Vec<AutomationSpec>, ConfigError> {
    let root = Path::new(dir);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut specs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let spec = toml::from_str::<AutomationSpec>(&raw).map_err(|source| ConfigError::Parse {
            file: path.display().to_string(),
            source,
        })?;
        specs.push(spec);
    }

    specs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(specs)
}

pub fn find_automation(dir: &str, key: &str) -> Result<Option<AutomationSpec>, ConfigError> {
    let key = key.trim();
    if key.is_empty() {
        return Ok(None);
    }
    let specs = load_automations(dir)?;
    Ok(specs
        .into_iter()
        .find(|spec| spec.id == key || spec.name.eq_ignore_ascii_case(key)))
}

pub fn set_automation_status(
    dir: &str,
    key: &str,
    status: AutomationStatus,
) -> Result<Option<AutomationSpec>, ConfigError> {
    let root = Path::new(dir);
    if !root.exists() {
        return Ok(None);
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let mut spec = toml::from_str::<AutomationSpec>(&raw).map_err(|source| ConfigError::Parse {
            file: path.display().to_string(),
            source,
        })?;
        if spec.id == key || spec.name.eq_ignore_ascii_case(key) {
            spec.status = status;
            let encoded = toml::to_string_pretty(&spec)
                .map_err(|e| ConfigError::Secret(format!("automation encode: {e}")))?;
            fs::write(&path, encoded)?;
            return Ok(Some(spec));
        }
    }

    Ok(None)
}

pub fn upsert_automation(dir: &str, spec: &AutomationSpec) -> Result<PathBuf, ConfigError> {
    let root = Path::new(dir);
    fs::create_dir_all(root)?;
    let path = root.join(format!("{}.toml", spec.id));
    let encoded = toml::to_string_pretty(spec)
        .map_err(|e| ConfigError::Secret(format!("automation encode: {e}")))?;
    fs::write(&path, encoded)?;
    Ok(path)
}

pub fn delete_automation(dir: &str, key: &str) -> Result<Option<AutomationSpec>, ConfigError> {
    let root = Path::new(dir);
    if !root.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let spec = toml::from_str::<AutomationSpec>(&raw).map_err(|source| ConfigError::Parse {
            file: path.display().to_string(),
            source,
        })?;
        if spec.id == key || spec.name.eq_ignore_ascii_case(key) {
            fs::remove_file(&path)?;
            return Ok(Some(spec));
        }
    }
    Ok(None)
}

pub fn slugify_automation_id(name: &str) -> String {
    let slug = name
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();
    slug.split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn describe_schedule(schedule: &AutomationSchedule) -> String {
    match schedule {
        AutomationSchedule::Interval { minutes } => format!("every {minutes}m"),
        AutomationSchedule::Daily { hour, minute } => format!("daily {:02}:{:02}", hour, minute),
        AutomationSchedule::Weekly { weekday, hour, minute } => {
            format!("weekly {} {:02}:{:02}", weekday, hour, minute)
        }
    }
}

pub fn parse_schedule_expr(expr: &str) -> Result<AutomationSchedule, String> {
    let parts = expr.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["every", minutes] => {
            let minutes = minutes.trim_end_matches('m').parse::<u64>()
                .map_err(|_| "expected minutes like 'every 60m'".to_string())?;
            Ok(AutomationSchedule::Interval { minutes })
        }
        ["daily", time] => {
            let (hour, minute) = parse_hhmm(time)?;
            Ok(AutomationSchedule::Daily { hour, minute })
        }
        ["weekly", weekday, time] => {
            let (hour, minute) = parse_hhmm(time)?;
            let weekday = normalize_weekday(weekday)?;
            Ok(AutomationSchedule::Weekly { weekday, hour, minute })
        }
        _ => Err("use 'every 60m', 'daily 09:30', or 'weekly mon 09:30'".into()),
    }
}

pub fn is_schedule_due(schedule: &AutomationSchedule, last_finished_at_ms: Option<u64>, now_ms: u64) -> bool {
    match schedule {
        AutomationSchedule::Interval { minutes } => match last_finished_at_ms {
            Some(last) => now_ms.saturating_sub(last) >= minutes.saturating_mul(60_000),
            None => true,
        },
        AutomationSchedule::Daily { hour, minute } => {
            let now = local_dt(now_ms);
            let scheduled = local_today_time(now, *hour, *minute);
            if now < scheduled {
                return false;
            }
            match last_finished_at_ms {
                Some(last) => local_dt(last) < scheduled,
                None => true,
            }
        }
        AutomationSchedule::Weekly { weekday, hour, minute } => {
            let now = local_dt(now_ms);
            let wanted = parse_weekday(weekday).unwrap_or(Weekday::Mon);
            let scheduled = local_weekday_time(now, wanted, *hour, *minute);
            if now < scheduled {
                return false;
            }
            match last_finished_at_ms {
                Some(last) => local_dt(last) < scheduled,
                None => true,
            }
        }
    }
}

pub fn ensure_sample_automation(dir: &str) -> Result<PathBuf, ConfigError> {
    let root = Path::new(dir);
    fs::create_dir_all(root)?;
    let path = root.join("daily-summary.toml");
    if path.exists() {
        return Ok(path);
    }

    let sample = AutomationSpec {
        id: "daily-summary".into(),
        name: "Daily Summary".into(),
        prompt: "Summarize the most important updates in this workspace and call out anything that needs attention.".into(),
        status: AutomationStatus::Paused,
        schedule: AutomationSchedule::Daily { hour: 9, minute: 0 },
        session: Some("automation".into()),
        delivery: None,
    };
    let _ = upsert_automation(dir, &sample)?;
    Ok(path)
}

fn parse_hhmm(text: &str) -> Result<(u8, u8), String> {
    let time = NaiveTime::parse_from_str(text, "%H:%M")
        .map_err(|_| "expected time in HH:MM format".to_string())?;
    Ok((time.hour() as u8, time.minute() as u8))
}

fn normalize_weekday(text: &str) -> Result<String, String> {
    Ok(match parse_weekday(text) {
        Some(Weekday::Mon) => "mon",
        Some(Weekday::Tue) => "tue",
        Some(Weekday::Wed) => "wed",
        Some(Weekday::Thu) => "thu",
        Some(Weekday::Fri) => "fri",
        Some(Weekday::Sat) => "sat",
        Some(Weekday::Sun) => "sun",
        None => return Err("expected weekday like mon/tue/wed".into()),
    }
    .into())
}

fn parse_weekday(text: &str) -> Option<Weekday> {
    match text.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Some(Weekday::Mon),
        "tue" | "tues" | "tuesday" => Some(Weekday::Tue),
        "wed" | "wednesday" => Some(Weekday::Wed),
        "thu" | "thurs" | "thursday" => Some(Weekday::Thu),
        "fri" | "friday" => Some(Weekday::Fri),
        "sat" | "saturday" => Some(Weekday::Sat),
        "sun" | "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

fn local_dt(ms: u64) -> chrono::DateTime<Local> {
    Local.timestamp_millis_opt(ms as i64).single().unwrap_or_else(Local::now)
}

fn local_today_time(now: chrono::DateTime<Local>, hour: u8, minute: u8) -> chrono::DateTime<Local> {
    now.with_hour(hour as u32)
        .and_then(|dt| dt.with_minute(minute as u32))
        .and_then(|dt| dt.with_second(0))
        .and_then(|dt| dt.with_nanosecond(0))
        .unwrap_or(now)
}

fn local_weekday_time(
    now: chrono::DateTime<Local>,
    weekday: Weekday,
    hour: u8,
    minute: u8,
) -> chrono::DateTime<Local> {
    let today = now.weekday().num_days_from_monday() as i64;
    let wanted = weekday.num_days_from_monday() as i64;
    let days_since = (7 + today - wanted) % 7;
    let base = now - chrono::Duration::days(days_since);
    local_today_time(base, hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_automations_from_directory() {
        let dir = std::env::temp_dir().join(format!("maat-automation-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.toml");
        fs::write(
            &path,
            r#"
id = "daily"
name = "Daily"
prompt = "hello"
[schedule]
kind = "interval"
minutes = 60
"#,
        )
        .unwrap();

        let specs = load_automations(dir.to_str().unwrap()).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "daily");
        assert_eq!(specs[0].status, AutomationStatus::Active);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_daily_schedule_expression() {
        let schedule = parse_schedule_expr("daily 09:30").unwrap();
        assert_eq!(describe_schedule(&schedule), "daily 09:30");
    }
}
