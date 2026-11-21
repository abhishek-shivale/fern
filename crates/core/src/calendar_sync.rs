//! Local calendar occurrences, cached coverage and work-block scheduling.
mod publication;
pub use publication::*;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, ensure};
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Id, Kind, Store, Task, ical};

pub const PLANNING_DAYS: u64 = 30;
pub const MAX_AGE_MS: i64 = 15 * 60 * 1000;
pub const DEFAULT_BLOCK_MINUTES: i64 = 30;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Google,
    ICloud,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::Google, Self::ICloud];

    pub fn code(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::ICloud => "icloud",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::ICloud => "iCloud",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.code() == value)
            .context("Unknown calendar provider")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CalendarKey {
    pub provider: Provider,
    pub account: String,
    pub calendar: String,
}

#[derive(Clone, Debug)]
pub struct ExternalCalendar {
    pub key: CalendarKey,
    pub name: String,
    pub account_name: String,
    pub timezone: Option<String>,
    pub writable: bool,
    pub present: bool,
    pub visible: bool,
    pub busy: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DateRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl DateRange {
    pub fn month(day: NaiveDate) -> Self {
        let start = day.with_day(1).unwrap_or(day);
        Self {
            start,
            end: start
                .checked_add_months(chrono::Months::new(1))
                .unwrap_or(NaiveDate::MAX),
        }
    }

    pub fn planning(today: NaiveDate) -> Self {
        Self {
            start: today,
            end: today + Days::new(PLANNING_DAYS),
        }
    }

    /// Months touching this half-open date range, as independent cache units.
    pub fn months(self) -> impl Iterator<Item = NaiveDate> {
        std::iter::successors(
            (self.start < self.end).then(|| Self::month(self.start).start),
            move |month| {
                month
                    .checked_add_months(chrono::Months::new(1))
                    .filter(|m| *m < self.end)
            },
        )
    }

    pub fn utc_bounds(self, zone: Option<&str>) -> Result<TimeSlot> {
        let resolve = |day: NaiveDate| {
            let midnight = day.and_time(NaiveTime::MIN);
            // Some zones advance their clocks at midnight.
            (0..=24)
                .find_map(|hour| {
                    wall_time(
                        midnight.checked_add_signed(chrono::TimeDelta::hours(hour))?,
                        zone,
                    )
                })
                .context("The calendar date is outside the supported time range")
        };
        TimeSlot::new(resolve(self.start)?, resolve(self.end)?)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct TimeSlot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl TimeSlot {
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Self> {
        ensure!(start < end, "The work block must end after it starts");
        Ok(Self { start, end })
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn local_days(self) -> DateRange {
        let start = self.start.with_timezone(&Local).date_naive();
        let last = (self.end - chrono::TimeDelta::nanoseconds(1))
            .with_timezone(&Local)
            .date_naive();
        DateRange {
            start,
            end: last.succ_opt().unwrap_or(last),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventSpan {
    Timed { slot: TimeSlot },
    AllDay { start: NaiveDate, end: NaiveDate },
}

impl EventSpan {
    pub fn overlaps_days(self, range: DateRange) -> bool {
        let dates = match self {
            Self::Timed { slot } => slot.local_days(),
            Self::AllDay { start, end } => DateRange { start, end },
        };
        dates.start < range.end && range.start < dates.end
    }

    pub fn busy_slot(self, zone: Option<&str>) -> Result<TimeSlot> {
        match self {
            Self::Timed { slot } => Ok(slot),
            Self::AllDay { start, end } => DateRange { start, end }.utc_bounds(zone),
        }
    }

    pub fn display(self) -> ical::Occurrence {
        match self {
            Self::Timed { slot } => {
                let start = slot.start.with_timezone(&Local);
                let end = slot.end.with_timezone(&Local);
                ical::Occurrence {
                    start: start.date_naive(),
                    start_time: Some(start.time()),
                    end: slot
                        .local_days()
                        .end
                        .pred_opt()
                        .unwrap_or(start.date_naive()),
                    end_time: Some(if end.time() == NaiveTime::MIN {
                        NaiveTime::from_hms_opt(23, 59, 0).unwrap_or_default()
                    } else {
                        end.time()
                    }),
                }
            }
            Self::AllDay { start, end } => ical::Occurrence {
                start,
                start_time: None,
                end: end.pred_opt().unwrap_or(start),
                end_time: None,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExternalEvent {
    pub id: String,
    pub title: String,
    pub location: String,
    pub notes: String,
    pub span: EventSpan,
    pub busy: bool,
}

#[derive(Clone, Debug)]
pub struct CalendarWindow {
    pub month: NaiveDate,
    pub events: Vec<ExternalEvent>,
    pub refreshed_at: Option<i64>,
    pub error: Option<String>,
    time_basis: Option<String>,
}

impl CalendarWindow {
    pub fn fresh(&self, now: i64) -> bool {
        self.error.is_none()
            && self
                .refreshed_at
                .is_some_and(|t| (0..MAX_AGE_MS).contains(&(now - t)))
            && month_time_basis(self.month)
                .is_ok_and(|basis| self.time_basis.as_ref() == Some(&basis))
    }
}

#[derive(Clone, Debug)]
pub struct CachedCalendar {
    pub calendar: ExternalCalendar,
    pub windows: BTreeMap<NaiveDate, CalendarWindow>,
    pub error: Option<String>,
}

impl CachedCalendar {
    pub fn fresh(&self, range: DateRange, now: i64) -> bool {
        self.calendar.present
            && self.error.is_none()
            && range
                .months()
                .all(|month| self.windows.get(&month).is_some_and(|w| w.fresh(now)))
    }

    pub fn events(&self, range: DateRange) -> Vec<&ExternalEvent> {
        occurrences(
            range.months().filter_map(|m| self.windows.get(&m)),
            |event| event.span.overlaps_days(range),
        )
    }

    fn busy_slots(
        &self,
        slot: TimeSlot,
        range: DateRange,
        exclude: Option<&str>,
    ) -> (bool, Vec<TimeSlot>) {
        let mut unknown = false;
        let events = occurrences(
            range.months().filter_map(|m| self.windows.get(&m)),
            |event| event.busy && Some(event.id.as_str()) != exclude,
        );
        let mut busy = Vec::new();
        for event in events {
            match event.span.busy_slot(self.calendar.timezone.as_deref()) {
                Ok(event_slot) if event_slot.overlaps(slot) => busy.push(event_slot),
                Ok(_) => {}
                Err(_) => unknown = true,
            }
        }
        (unknown, busy)
    }
}

fn occurrences<'a>(
    windows: impl Iterator<Item = &'a CalendarWindow>,
    include: impl Fn(&ExternalEvent) -> bool,
) -> Vec<&'a ExternalEvent> {
    let mut events = BTreeMap::new();
    for window in windows {
        for event in &window.events {
            let entry = events
                .entry(event.id.as_str())
                .or_insert((window.refreshed_at, event));
            if window.refreshed_at > entry.0 {
                *entry = (window.refreshed_at, event);
            }
        }
    }
    events
        .into_values()
        .map(|(_, e)| e)
        .filter(|e| include(e))
        .collect()
}

#[derive(Default)]
pub struct Availability {
    pub stale: bool,
    pub busy: Vec<TimeSlot>,
}

/// Staleness and known clashes are independent. A stale source can supply busy intervals.
pub fn availability(
    calendars: &[CachedCalendar],
    slot: TimeSlot,
    exclude: Option<(&CalendarKey, &str)>,
    now_ms: i64,
) -> Availability {
    let mut result = Availability::default();
    for cache in calendars.iter().filter(|cache| cache.calendar.busy) {
        let zone = cache.calendar.timezone.as_deref();
        let local = slot.local_days();
        let range = if let Some(tz) = zone.and_then(|name| name.parse::<chrono_tz::Tz>().ok()) {
            let start = slot.start.with_timezone(&tz).date_naive();
            let end = (slot.end - chrono::TimeDelta::nanoseconds(1))
                .with_timezone(&tz)
                .date_naive();
            DateRange {
                start: local.start.min(start),
                end: local.end.max(end.succ_opt().unwrap_or(end)),
            }
        } else {
            local
        };
        let exclude = exclude
            .filter(|(key, _)| *key == &cache.calendar.key)
            .map(|(_, id)| id);
        let (unknown, busy) = cache.busy_slots(slot, range, exclude);
        result.stale |= unknown || !cache.fresh(range, now_ms);
        result.busy.extend(busy);
    }
    result
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct WorkingHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub weekends: bool,
}

impl Default for WorkingHours {
    fn default() -> Self {
        Self {
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap_or_default(),
            weekends: false,
        }
    }
}

/// Find up to five work blocks before the deadline within the planning window.
pub fn suggest_slots(
    search: TimeSlot,
    hours: WorkingHours,
    duration: chrono::TimeDelta,
    busy: &[TimeSlot],
) -> Vec<TimeSlot> {
    let mut blocked = busy.to_vec();
    blocked.sort_unstable_by_key(|slot| slot.start);
    let dates = search.local_days();
    let mut suggestions = Vec::new();
    if duration <= chrono::TimeDelta::zero() || hours.start >= hours.end {
        return suggestions;
    }
    for day in dates.start.iter_days().take_while(|date| *date < dates.end) {
        if !hours.weekends && day.weekday().number_from_monday() > 5 {
            continue;
        }
        let (Some(open), Some(close)) = (
            wall_time(day.and_time(hours.start), None),
            wall_time(day.and_time(hours.end), None),
        ) else {
            continue;
        };
        let end = close.min(search.end);
        let mut start = minute_ceil(open.max(search.start));
        for event in blocked
            .iter()
            .copied()
            .filter(|event| event.end > open && event.start < close)
        {
            if start + duration <= end.min(event.start) {
                suggestions.push(TimeSlot {
                    start,
                    end: start + duration,
                });
                break;
            }
            start = start.max(minute_ceil(event.end));
        }
        if suggestions
            .last()
            .is_none_or(|slot| slot.start.with_timezone(&Local).date_naive() != day)
            && start + duration <= end
        {
            suggestions.push(TimeSlot {
                start,
                end: start + duration,
            });
        }
        if suggestions.len() == 5 {
            break;
        }
    }
    suggestions
}

pub fn wall_time(wall: NaiveDateTime, zone: Option<&str>) -> Option<DateTime<Utc>> {
    if let Some(name) = zone {
        let tz = name.parse::<chrono_tz::Tz>().ok()?;
        tz.from_local_datetime(&wall).earliest().map(|t| t.to_utc())
    } else {
        Local
            .from_local_datetime(&wall)
            .earliest()
            .map(|t| t.to_utc())
    }
}

fn task_zone(task: &Task) -> Option<&str> {
    (!task.is_floating)
        .then_some(task.timezone.as_deref())
        .flatten()
}

fn month_time_basis(month: NaiveDate) -> Result<String> {
    let bounds = DateRange::month(month).utc_bounds(None)?;
    Ok(format!(
        "{}/{}",
        bounds.start.timestamp(),
        bounds.end.timestamp()
    ))
}

pub fn task_slot(task: &Task) -> Result<Option<TimeSlot>> {
    let (Some(date), Some(time)) = (task.due_date, task.due_time) else {
        return Ok(None);
    };
    if task.is_deleted() || task.kind != Kind::Task {
        return Ok(None);
    }
    let zone = task_zone(task);
    let start =
        wall_time(date.and_time(time), zone).context("The task starts at an invalid local time")?;
    let end = if let Some(time) = task.end_time {
        wall_time(task.end_date.unwrap_or(date).and_time(time), zone)
            .context("The task ends at an invalid local time")?
    } else {
        start
            .checked_add_signed(chrono::TimeDelta::minutes(DEFAULT_BLOCK_MINUTES))
            .context("The task ends outside the calendar")?
    };
    TimeSlot::new(start, end).map(Some)
}

pub fn minute_floor(time: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(time.timestamp().div_euclid(60) * 60, 0).unwrap_or(time)
}

pub fn minute_ceil(time: DateTime<Utc>) -> DateTime<Utc> {
    let floor = minute_floor(time);
    if floor == time {
        floor
    } else {
        floor + chrono::TimeDelta::minutes(1)
    }
}

impl Store {
    pub fn schedule_work(
        &self,
        id: Id,
        slot: TimeSlot,
        publish: Option<&CalendarKey>,
    ) -> Result<()> {
        self.in_tx(|| {
            let task = self.task(id)?.context("Task is no longer available")?;
            ensure!(!task.is_closed(), "Choose an open task");
            self.set_work_slot(id, slot)?;
            if let Some(calendar) = publish {
                self.queue_publication(id, calendar)?;
            }
            Ok(())
        })
    }

    pub fn set_work_slot(&self, id: Id, slot: TimeSlot) -> Result<()> {
        self.in_tx(|| {
            TimeSlot::new(slot.start, slot.end)?;
            let task = self.task(id)?.context("Task is no longer available")?;
            ensure!(!task.is_deleted() && task.kind == Kind::Task, "Task is no longer available");
            let start = minute_floor(slot.start);
            let end = minute_ceil(slot.end);
            let zone = task_zone(&task).and_then(|name| name.parse::<chrono_tz::Tz>().ok());
            let mut zone_name = task.timezone.as_deref();
            let mut floating = task.is_floating;
            let wall = |t: DateTime<Utc>| zone.map_or_else(|| t.with_timezone(&Local).naive_local(), |z| t.with_timezone(&z).naive_local());
            let (mut from, mut to) = (wall(start), wall(end));
            if wall_time(from, task_zone(&task)) != Some(start) || wall_time(to, task_zone(&task)) != Some(end) {
                // Keep the intended instant when the local time occurs twice during a clock change.
                zone_name = Some("UTC");
                floating = false;
                (from, to) = (start.naive_utc(), end.naive_utc());
            }
            self.conn.execute(
                "UPDATE tasks SET due_date = ?2, due_time = ?3, end_date = ?4, end_time = ?5,
                                  timezone = ?6, is_floating = ?7, snoozed_until = NULL WHERE id = ?1",
                params![id.to_string(), from.date().to_string(), from.format("%H:%M").to_string(),
                    (from.date() != to.date()).then(|| to.date().to_string()), to.format("%H:%M").to_string(), zone_name, floating],
            )?;
            Ok(())
        })
    }

    pub fn working_hours(&self) -> Result<WorkingHours> {
        self.setting("calendar_working_hours")?.map_or_else(
            || Ok(WorkingHours::default()),
            |value| Ok(serde_json::from_str(&value)?),
        )
    }

    pub fn save_working_hours(&self, hours: WorkingHours) -> Result<()> {
        ensure!(
            hours.start < hours.end,
            "Working hours must end after they start"
        );
        self.set_setting("calendar_working_hours", &serde_json::to_string(&hours)?)
    }

    pub fn replace_external_calendars(
        &self,
        provider: Provider,
        calendars: &[ExternalCalendar],
    ) -> Result<()> {
        self.in_tx(|| {
            self.conn.execute("UPDATE external_calendars SET present = 0 WHERE provider = ?1", [provider.code()])?;
            let mut insert = self.conn.prepare_cached(
                "INSERT INTO external_calendars (provider, account, calendar, name, account_name, timezone, writable)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(provider, account, calendar) DO UPDATE SET name = excluded.name,
                    account_name = excluded.account_name, timezone = excluded.timezone,
                    writable = excluded.writable, present = 1",
            )?;
            for calendar in calendars {
                ensure!(calendar.key.provider == provider, "Unexpected calendar provider");
                insert.execute(params![provider.code(), calendar.key.account, calendar.key.calendar,
                    calendar.name, calendar.account_name, calendar.timezone, calendar.writable])?;
            }
            self.set_calendar_provider_error(provider, None)
        })
    }

    pub fn set_calendar_provider_error(
        &self,
        provider: Provider,
        error: Option<&str>,
    ) -> Result<()> {
        let key = format!("external_calendar_error_{}", provider.code());
        match error {
            Some(message) => self.set_setting(&key, message),
            None => self.delete_setting(&key),
        }
    }

    pub fn calendar_provider_error(&self, provider: Provider) -> Result<Option<String>> {
        self.setting(&format!("external_calendar_error_{}", provider.code()))
    }

    pub fn set_calendar_selection(
        &self,
        key: &CalendarKey,
        visible: bool,
        busy: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE external_calendars SET visible = ?4, busy = ?5 WHERE provider = ?1 AND account = ?2 AND calendar = ?3",
            params![key.provider.code(), key.account, key.calendar, visible, busy],
        )?;
        Ok(())
    }

    pub fn save_calendar_windows(
        &self,
        calendar: &CalendarKey,
        range: DateRange,
        events: Result<&[ExternalEvent], &str>,
        now_ms: i64,
    ) -> Result<()> {
        self.in_tx(|| {
            for month in range.months() {
                if let Err(error) = events {
                    self.conn.execute(
                        "INSERT INTO external_calendar_windows (provider, account, calendar, month, error) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(provider, account, calendar, month) DO UPDATE SET error = excluded.error",
                        params![calendar.provider.code(), calendar.account, calendar.calendar, month.to_string(), error],
                    )?;
                    continue;
                }
                let events = events.unwrap_or_default();
                let list: Vec<_> = events.iter().filter(|event| event.span.overlaps_days(DateRange::month(month))).collect();
                self.conn.execute(
                    "INSERT INTO external_calendar_windows (provider, account, calendar, month, events, refreshed_at, time_basis) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(provider, account, calendar, month) DO UPDATE
                        SET events = excluded.events, refreshed_at = excluded.refreshed_at, error = NULL, time_basis = excluded.time_basis",
                    params![calendar.provider.code(), calendar.account, calendar.calendar, month.to_string(),
                        serde_json::to_string(&list)?, now_ms, month_time_basis(month)?],
                )?;
            }
            Ok(())
        })
    }

    pub fn cached_calendars(&self) -> Result<Vec<CachedCalendar>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT provider, account, calendar, name, account_name, timezone, writable, present, visible, busy
             FROM external_calendars ORDER BY provider, account_name, name COLLATE NOCASE, calendar",
        )?;
        let mut rows = stmt.query([])?;
        let mut results = Vec::new();
        let mut windows = self.conn.prepare_cached(
            "SELECT month, events, refreshed_at, error, time_basis FROM external_calendar_windows
             WHERE provider = ?1 AND account = ?2 AND calendar = ?3",
        )?;
        while let Some(row) = rows.next()? {
            let provider = Provider::parse(&row.get::<_, String>(0)?)?;
            let key = CalendarKey {
                provider,
                account: row.get(1)?,
                calendar: row.get(2)?,
            };
            let mut coverage = BTreeMap::new();
            let mut dates = windows.query(params![provider.code(), key.account, key.calendar])?;
            while let Some(row) = dates.next()? {
                let month = row.get::<_, String>(0)?.parse()?;
                coverage.insert(
                    month,
                    CalendarWindow {
                        month,
                        events: serde_json::from_str(&row.get::<_, String>(1)?)?,
                        refreshed_at: row.get(2)?,
                        error: row.get(3)?,
                        time_basis: row.get(4)?,
                    },
                );
            }
            results.push(CachedCalendar {
                calendar: ExternalCalendar {
                    key,
                    name: row.get(3)?,
                    account_name: row.get(4)?,
                    timezone: row.get(5)?,
                    writable: row.get(6)?,
                    present: row.get(7)?,
                    visible: row.get(8)?,
                    busy: row.get(9)?,
                },
                windows: coverage,
                error: self.calendar_provider_error(provider)?,
            });
        }
        Ok(results)
    }

    pub fn publication_destination(&self) -> Result<Option<CalendarKey>> {
        self.setting("calendar_publication_destination")?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(Into::into)
    }

    pub fn set_publication_destination(&self, key: Option<&CalendarKey>) -> Result<()> {
        let Some(key) = key else {
            return self.delete_setting("calendar_publication_destination");
        };
        ensure!(
            key.provider == Provider::Google,
            "The publishing calendar must be a Google calendar"
        );
        let writable: bool = self.conn.query_row(
            "SELECT present AND writable FROM external_calendars WHERE provider = ?1 AND account = ?2 AND calendar = ?3",
            params![key.provider.code(), key.account, key.calendar], |r| r.get(0),
        ).optional()?.unwrap_or(false);
        ensure!(writable, "The calendar does not allow publishing");
        self.set_setting(
            "calendar_publication_destination",
            &serde_json::to_string(key)?,
        )
    }
}

/// Coalesce adjacent cache months into provider requests.
pub fn month_ranges(months: BTreeSet<NaiveDate>) -> Vec<DateRange> {
    let mut ranges: Vec<DateRange> = Vec::new();
    for month in months {
        let range = DateRange::month(month);
        if let Some(last) = ranges.last_mut().filter(|r| r.end == month) {
            last.end = range.end;
        } else {
            ranges.push(range);
        }
    }
    ranges
}

pub fn event_key(key: &CalendarKey, id: &str) -> String {
    // JSON serialization preserves boundaries even when a provider ID contains a delimiter.
    serde_json::to_string(&(key, id)).unwrap_or_default()
}

#[cfg(test)]
mod tests;
