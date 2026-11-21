//! Local per-task follow-up schedules.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    Id, Kind, Store,
    calendar_sync::{TimeSlot, task_slot},
};

const MINUTE_MS: i64 = 60_000;
pub const MAX_MINUTES: u32 = 30 * 24 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct FollowUpOptions {
    pub delay_minutes: u32,
    pub interval_minutes: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct FollowUp {
    options: FollowUpOptions,
    block: Option<TimeSlot>,
    next_at: Option<i64>,
    notice: Option<Id>,
}

#[derive(Clone, Debug)]
pub struct FollowUpNotice {
    pub task: Id,
    pub notice: Id,
    pub title: String,
}

#[derive(Clone, Copy, Debug)]
pub enum FollowUpAction {
    Done,
    Snooze(u32),
    Reschedule,
}

fn validate_minutes(minutes: u32) -> Result<()> {
    ensure!(
        (1..=MAX_MINUTES).contains(&minutes),
        "Follow-up minutes must be between 1 and {MAX_MINUTES}"
    );
    Ok(())
}
impl FollowUp {
    fn set_block(&mut self, block: Option<TimeSlot>) {
        self.block = block;
        self.notice = None;
        self.next_at = block.map(|slot| {
            slot.end.timestamp_millis() + i64::from(self.options.delay_minutes) * MINUTE_MS
        });
    }
}

impl Store {
    fn follow_up(&self, task: Id) -> Result<Option<FollowUp>> {
        let result: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM task_follow_ups WHERE task_id = ?1",
                [task.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result
            .map(|value| serde_json::from_str(&value))
            .transpose()?)
    }

    pub fn follow_up_options(&self, task: Id) -> Result<Option<FollowUpOptions>> {
        Ok(self.follow_up(task)?.map(|follow_up| follow_up.options))
    }

    pub fn set_follow_up(&self, task: Id, options: Option<FollowUpOptions>) -> Result<()> {
        let Some(options) = options else {
            self.conn.execute(
                "DELETE FROM task_follow_ups WHERE task_id = ?1",
                [task.to_string()],
            )?;
            return Ok(());
        };
        validate_minutes(options.delay_minutes)?;
        if let Some(interval) = options.interval_minutes {
            validate_minutes(interval)?;
        }
        self.in_tx(|| {
            let task = self
                .task(task)?
                .filter(|task| !task.is_closed() && !task.is_deleted() && task.kind == Kind::Task)
                .context("Follow-ups require an open task")?;
            let block = task_slot(&task)?;
            if self
                .follow_up(task.id)?
                .is_some_and(|f| f.block == block && f.options == options)
            {
                return Ok(());
            }
            let mut follow_up = FollowUp {
                options,
                block: None,
                notice: None,
                next_at: None,
            };
            follow_up.set_block(block);
            self.save_follow_up(task.id, &follow_up)
        })
    }

    /// Claim one due notice per task. Restart and wake recover just one missed interval.
    pub fn follow_up_tick(
        &self,
        now_ms: i64,
        delivery_enabled: bool,
    ) -> Result<Vec<FollowUpNotice>> {
        self.in_tx(|| {
            let mut statement = self
                .conn
                .prepare_cached("SELECT task_id, state FROM task_follow_ups")?;
            let mut rows = statement.query([])?;
            let mut existing = Vec::new();
            while let Some(row) = rows.next()? {
                let id: Id = row.get::<_, String>(0)?.parse()?;
                let follow_up: FollowUp = serde_json::from_str(&row.get::<_, String>(1)?)?;
                existing.push((id, follow_up));
            }
            drop(rows);
            let mut notices = Vec::new();
            for (id, mut follow_up) in existing {
                let Some(task) = self
                    .task(id)?
                    .filter(|t| !t.is_closed() && !t.is_deleted() && t.kind == Kind::Task)
                else {
                    self.set_follow_up(id, None)?;
                    continue;
                };
                let original = follow_up.clone();
                let block = task_slot(&task).ok().flatten();
                if follow_up.block != block {
                    follow_up.set_block(block);
                }
                if delivery_enabled && follow_up.next_at.is_some_and(|at| at <= now_ms) {
                    let notice = Id::now_v7();
                    follow_up.notice = Some(notice);
                    follow_up.next_at = follow_up
                        .options
                        .interval_minutes
                        .map(|interval| now_ms + i64::from(interval) * MINUTE_MS);
                    notices.push(FollowUpNotice {
                        task: id,
                        notice,
                        title: task.title,
                    });
                }
                if follow_up != original {
                    self.save_follow_up(id, &follow_up)?;
                }
            }
            Ok(notices)
        })
    }

    pub fn active_follow_up_notices(&self) -> Result<Vec<(Id, Id)>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT task_id, state FROM task_follow_ups")?;
        let mut rows = statement.query([])?;
        let mut active = Vec::new();
        while let Some(row) = rows.next()? {
            let follow_up: FollowUp = serde_json::from_str(&row.get::<_, String>(1)?)?;
            if let Some(notice) = follow_up.notice {
                active.push((row.get::<_, String>(0)?.parse()?, notice));
            }
        }
        Ok(active)
    }

    pub fn act_on_follow_up(
        &self,
        id: Id,
        notice: Id,
        action: FollowUpAction,
        now_ms: i64,
    ) -> Result<bool> {
        self.in_tx(|| {
            let Some(mut follow_up) = self.follow_up(id)?.filter(|f| f.notice == Some(notice))
            else {
                return Ok(false);
            };
            let Some(task) = self
                .task(id)?
                .filter(|t| !t.is_closed() && !t.is_deleted() && t.kind == Kind::Task)
            else {
                return Ok(false);
            };
            if task_slot(&task)? != follow_up.block {
                return Ok(false);
            }
            match action {
                FollowUpAction::Done => self.set_completed(id, true)?,
                FollowUpAction::Snooze(minutes) => {
                    validate_minutes(minutes)?;
                    follow_up.notice = None;
                    follow_up.next_at = Some(now_ms + i64::from(minutes) * MINUTE_MS);
                    self.save_follow_up(id, &follow_up)?;
                }
                FollowUpAction::Reschedule => {
                    follow_up.notice = None;
                    follow_up.next_at = None;
                    self.save_follow_up(id, &follow_up)?;
                }
            }
            Ok(true)
        })
    }

    fn save_follow_up(&self, task: Id, follow_up: &FollowUp) -> Result<()> {
        self.conn.execute(
            "INSERT INTO task_follow_ups (task_id, state) VALUES (?1, ?2)
             ON CONFLICT(task_id) DO UPDATE SET state = excluded.state",
            params![task.to_string(), serde_json::to_string(follow_up)?],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta, Utc};

    #[test]
    fn follows_up_once_after_restart_then_honors_repeat_snooze_reschedule_and_done() -> Result<()> {
        let db = tempfile::NamedTempFile::new()?;
        let store = Store::open(db.path())?;
        let task = store.add_task("Review the draft")?.id;
        let start: DateTime<Utc> = "2026-11-23T12:00:00Z".parse()?;
        let end = start + TimeDelta::hours(1);
        store.set_work_slot(task, TimeSlot::new(start, end)?)?;
        store.set_follow_up(
            task,
            Some(FollowUpOptions {
                delay_minutes: 5,
                interval_minutes: Some(10),
            }),
        )?;
        assert!(
            store
                .follow_up_tick(end.timestamp_millis(), true)?
                .is_empty()
        );
        drop(store);

        let store = Store::open(db.path())?;
        let wake = end.timestamp_millis() + 60 * MINUTE_MS;
        let first = store.follow_up_tick(wake, true)?;
        assert_eq!(first.len(), 1);
        assert!(store.follow_up_tick(wake + 9 * MINUTE_MS, true)?.is_empty());
        let next = store
            .follow_up_tick(wake + 10 * MINUTE_MS, true)?
            .pop()
            .unwrap();
        assert_ne!(next.notice, first[0].notice);
        assert!(store.act_on_follow_up(
            task,
            next.notice,
            FollowUpAction::Snooze(30),
            wake + 10 * MINUTE_MS
        )?);
        assert!(
            store
                .follow_up_tick(wake + 39 * MINUTE_MS, true)?
                .is_empty()
        );
        let next = store
            .follow_up_tick(wake + 40 * MINUTE_MS, true)?
            .pop()
            .unwrap();
        assert!(store.act_on_follow_up(
            task,
            next.notice,
            FollowUpAction::Reschedule,
            wake + 40 * MINUTE_MS
        )?);
        assert!(
            store
                .follow_up_tick(wake + 90 * MINUTE_MS, true)?
                .is_empty()
        );

        let offset = TimeDelta::hours(5);
        store.set_work_slot(task, TimeSlot::new(start + offset, end + offset)?)?;
        let new_due = (end + offset).timestamp_millis() + 5 * MINUTE_MS;
        assert!(store.follow_up_tick(new_due - 1, true)?.is_empty());
        let next = store.follow_up_tick(new_due, true)?.pop().unwrap();
        assert!(!store.act_on_follow_up(task, first[0].notice, FollowUpAction::Done, new_due)?);
        assert!(store.act_on_follow_up(task, next.notice, FollowUpAction::Done, new_due)?);
        assert!(store.task(task)?.unwrap().is_completed());
        assert!(
            store
                .follow_up_tick(new_due + 90 * MINUTE_MS, true)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn one_time_follow_up_can_be_disabled_and_task_removal_clears_it() -> Result<()> {
        let store = Store::open_in_memory()?;
        let task = store.add_task("Reply")?.id;
        let start: DateTime<Utc> = "2026-11-23T12:00:00Z".parse()?;
        let end = start + TimeDelta::hours(1);
        store.set_work_slot(task, TimeSlot::new(start, end)?)?;
        let options = FollowUpOptions {
            delay_minutes: 1,
            interval_minutes: None,
        };
        store.set_follow_up(task, Some(options))?;
        let due = end.timestamp_millis() + MINUTE_MS;
        assert!(store.follow_up_tick(due, false)?.is_empty());
        assert_eq!(store.follow_up_tick(due, true)?.len(), 1);
        assert!(store.follow_up_tick(due + 60 * MINUTE_MS, true)?.is_empty());
        store.set_follow_up(task, None)?;
        assert!(store.active_follow_up_notices()?.is_empty());
        store.set_follow_up(task, Some(options))?;
        store.delete_task(task)?;
        assert!(store.follow_up_tick(due + 60 * MINUTE_MS, true)?.is_empty());
        Ok(())
    }
}
