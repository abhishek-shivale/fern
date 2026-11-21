//! Durable intent and conditional reconciliation for events published by Fern.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{CalendarKey, Provider, TimeSlot, task_slot};
use crate::{Id, Store, Task};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RemoteRevision {
    pub etag: String,
    pub slot: Option<TimeSlot>,
}

#[derive(Clone, Debug)]
pub struct RemoteEvent {
    pub revision: RemoteRevision,
    pub block_id: Option<String>,
    pub operation_id: Option<String>,
}

pub enum RemoteLookup {
    Absent,
    Deleted,
    Event(RemoteEvent),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum PublicationConflict {
    Changed(RemoteRevision),
    Removed,
    OtherEvent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum WriteKind {
    Save { slot: TimeSlot, title: String },
    Delete,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WriteIntent {
    pub id: Id,
    pub kind: WriteKind,
    /// The ETag the user last saw. A save without it is a create.
    pub expected: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Publication {
    pub id: Id,
    pub task_id: Option<Id>,
    pub generation: i64,
    pub state: PublicationState,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PublicationState {
    pub destination: CalendarKey,
    pub event_id: String,
    pub synced: Option<RemoteRevision>,
    pub last_local: Option<TimeSlot>,
    pub intent: Option<WriteIntent>,
    pub conflict: Option<PublicationConflict>,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reconcile {
    Write,
    Synced {
        revision: RemoteRevision,
        local: TimeSlot,
    },
    Cancelled,
    Superseded,
    Conflict(PublicationConflict),
    Current,
}

#[derive(Clone, Copy)]
pub enum Resolution {
    AcceptTime,
    RestoreFern,
    Unlink,
}

impl Publication {
    pub fn pending(&self, task: Option<&Task>) -> bool {
        self.state.synced.is_none()
            || self.state.intent.is_some()
            || task.and_then(|t| task_slot(t).ok().flatten()) != self.state.last_local
    }

    /// Called only after the adapter has confirmed access to the destination calendar.
    pub fn reconcile(&self, remote: RemoteLookup, desired: Option<TimeSlot>) -> Reconcile {
        let state = &self.state;
        if let Some(conflict) = &state.conflict {
            return Reconcile::Conflict(conflict.clone());
        }
        let remote = match remote {
            RemoteLookup::Event(remote) => remote,
            RemoteLookup::Deleted | RemoteLookup::Absent if desired.is_none() => {
                return Reconcile::Cancelled;
            }
            RemoteLookup::Absent => {
                if let Some(WriteIntent {
                    kind: WriteKind::Save { slot, .. },
                    expected: None,
                    ..
                }) = &state.intent
                {
                    return if desired == Some(*slot) {
                        Reconcile::Write
                    } else {
                        Reconcile::Superseded
                    };
                }
                return Reconcile::Conflict(PublicationConflict::Removed);
            }
            RemoteLookup::Deleted => return Reconcile::Conflict(PublicationConflict::Removed),
        };
        if remote.block_id.as_deref() != Some(&self.id.to_string()) {
            return Reconcile::Conflict(PublicationConflict::OtherEvent);
        }
        if let Some(intent) = &state.intent {
            if let WriteKind::Save { slot, .. } = intent.kind
                && remote.operation_id.as_deref() == Some(&intent.id.to_string())
                && remote.revision.slot == Some(slot)
            {
                return Reconcile::Synced {
                    revision: remote.revision,
                    local: slot,
                };
            }
            return if intent.expected.as_deref() == Some(&remote.revision.etag) {
                match intent.kind {
                    WriteKind::Save { slot, .. } if desired != Some(slot) => Reconcile::Superseded,
                    WriteKind::Delete if desired.is_some() => Reconcile::Superseded,
                    _ => Reconcile::Write,
                }
            } else {
                Reconcile::Conflict(PublicationConflict::Changed(remote.revision))
            };
        }
        if state.synced.as_ref() == Some(&remote.revision) {
            Reconcile::Current
        } else {
            Reconcile::Conflict(PublicationConflict::Changed(remote.revision))
        }
    }
}

fn event_id(id: Id) -> String {
    // Hexadecimal UUID digits are valid in Google's base32hex event IDs.
    format!("fern{}", id.simple())
}

impl Store {
    /// A synced repeat completion includes the rolled-forward task and its completed copy.
    /// Match them from the same applied batch without putting publication IDs on the wire.
    pub(crate) fn rebind_synced_calendar_blocks(
        &self,
        before: &[Task],
        changed: &[Id],
    ) -> Result<()> {
        let mut after = std::collections::HashMap::new();
        for &id in changed {
            if let Some(task) = self.task(id)? {
                after.insert(id, task);
            }
        }
        for block in before {
            let advanced = block.repeat.is_some()
                && after.get(&block.id).is_some_and(|task| {
                    task.repeat.is_some() && !task.is_deleted() && task.due_date > block.due_date
                });
            let undone =
                block.is_completed() && block.repeat.is_none() && !after.contains_key(&block.id);
            if !advanced && !undone {
                continue;
            }
            let mut matches = after.values().filter(|task| {
                task.id != block.id
                    && !task.is_deleted()
                    && ((advanced && task.is_completed() && task.repeat.is_none())
                        || (undone && !task.is_closed() && task.repeat.is_some()))
                    && same_occurrence(block, task)
            });
            let Some(replacement) = matches.next().filter(|_| matches.next().is_none()) else {
                continue;
            };
            self.conn.execute(
                "UPDATE calendar_publications SET task_id = ?2, generation = generation + 1
                 WHERE task_id = ?1 AND NOT EXISTS (SELECT 1 FROM calendar_publications WHERE task_id = ?2)",
                params![block.id.to_string(), replacement.id.to_string()],
            )?;
            if advanced {
                self.set_follow_up(block.id, None)?;
            }
        }
        Ok(())
    }

    pub fn publications(&self) -> Result<Vec<Publication>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, task_id, generation, state FROM calendar_publications ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(Publication {
                id: row.get::<_, String>(0)?.parse()?,
                task_id: row
                    .get::<_, Option<String>>(1)?
                    .map(|value| value.parse())
                    .transpose()?,
                generation: row.get(2)?,
                state: serde_json::from_str(&row.get::<_, String>(3)?)?,
            });
        }
        Ok(results)
    }

    pub fn queue_publication(&self, task_id: Id, destination: &CalendarKey) -> Result<Id> {
        self.in_tx(|| {
            ensure!(
                destination.provider == Provider::Google,
                "Only Google calendars support publishing"
            );
            ensure!(
                self.publication_destination()?.as_ref() == Some(destination),
                "Choose a publishing calendar in Integrations"
            );
            let task = self.task(task_id)?.context("Task is no longer available")?;
            let slot = task_slot(&task)?.context("Schedule a timed work block first")?;
            let current: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM calendar_publications WHERE task_id = ?1",
                    [task_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(current) = current {
                return Ok(current.parse()?);
            }
            let id = Id::now_v7();
            let state = PublicationState {
                destination: destination.clone(),
                event_id: event_id(id),
                synced: None,
                last_local: None,
                conflict: None,
                error: None,
                intent: Some(WriteIntent {
                    id: Id::now_v7(),
                    kind: WriteKind::Save {
                        slot,
                        title: task.title,
                    },
                    expected: None,
                }),
            };
            self.conn.execute(
                "INSERT INTO calendar_publications (id, task_id, state) VALUES (?1, ?2, ?3)",
                params![
                    id.to_string(),
                    task_id.to_string(),
                    serde_json::to_string(&state)?
                ],
            )?;
            Ok(id)
        })
    }

    /// Commit intent before attempting any network writes. Task dates supply durable desired state.
    pub fn prepare_publication(&self, publication: &mut Publication) -> Result<bool> {
        if publication.state.intent.is_some() || publication.state.conflict.is_some() {
            return Ok(publication.state.conflict.is_none());
        }
        let task = publication
            .task_id
            .map(|id| self.task(id))
            .transpose()?
            .flatten();
        let desired = task.as_ref().map(task_slot).transpose()?.flatten();
        if publication.state.synced.is_none() && desired.is_none() {
            self.remove_publication(publication)?;
            return Ok(false);
        }
        if publication.state.synced.is_some() && desired == publication.state.last_local {
            return Ok(true);
        }
        let mut state = publication.state.clone();
        state.intent = Some(WriteIntent {
            id: Id::now_v7(),
            kind: match (desired, task) {
                (Some(slot), Some(task)) => WriteKind::Save {
                    slot,
                    title: task.title,
                },
                _ => WriteKind::Delete,
            },
            expected: state.synced.as_ref().map(|revision| revision.etag.clone()),
        });
        if self.save_publication(publication, &state)? {
            publication.state = state;
            publication.generation += 1;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn finish_publication(
        &self,
        publication: &Publication,
        outcome: Result<Reconcile, &str>,
    ) -> Result<()> {
        let mut state = publication.state.clone();
        match outcome {
            Ok(Reconcile::Cancelled) => {
                let task = publication
                    .task_id
                    .map(|id| self.task(id))
                    .transpose()?
                    .flatten();
                if task
                    .as_ref()
                    .map(task_slot)
                    .transpose()?
                    .flatten()
                    .is_none()
                {
                    return self.remove_publication(publication);
                }
                // The user put the block back while the cancellation was in flight.
                state.intent = None;
                state.conflict = Some(PublicationConflict::Removed);
                state.error = None;
            }
            Ok(Reconcile::Superseded) => {
                state.intent = None;
                state.error = None;
            }
            Ok(Reconcile::Synced { revision, local }) => {
                state.synced = Some(revision);
                state.last_local = Some(local);
                state.intent = None;
                state.error = None;
            }
            Ok(Reconcile::Conflict(conflict)) => {
                state.conflict = Some(conflict);
                state.error = None;
            }
            Ok(Reconcile::Current) => state.error = None,
            Ok(Reconcile::Write) => anyhow::bail!("The calendar write did not run"),
            Err(error) => state.error = Some(error.to_owned()),
        }
        self.save_publication(publication, &state)?;
        Ok(())
    }

    pub fn resolve_publication(&self, id: Id, resolution: Resolution) -> Result<()> {
        self.in_tx(|| {
            let publication = self
                .publications()?
                .into_iter()
                .find(|p| p.id == id)
                .context("The publication is no longer available")?;
            let mut state = publication.state.clone();
            let conflict = state
                .conflict
                .take()
                .context("There is no conflict to resolve")?;
            if matches!(resolution, Resolution::Unlink) {
                return self.remove_publication(&publication);
            }
            let task = publication
                .task_id
                .map(|id| self.task(id))
                .transpose()?
                .flatten();
            let slot = task.as_ref().map(task_slot).transpose()?.flatten();
            state.error = None;
            state.intent = None;
            match (resolution, conflict) {
                (Resolution::AcceptTime, PublicationConflict::Changed(revision)) => {
                    let task = task
                        .filter(|task| !task.is_deleted())
                        .context("Task is no longer available")?;
                    self.set_work_slot(
                        task.id,
                        revision
                            .slot
                            .context("The event no longer has a timed range")?,
                    )?;
                    let updated = self.task(task.id)?.context("Task is no longer available")?;
                    state.last_local = task_slot(&updated)?;
                    state.synced = Some(revision);
                }
                (Resolution::RestoreFern, PublicationConflict::Changed(revision)) => {
                    // Keep the user's chosen revision as the precondition, even if it changes again.
                    state.last_local = revision.slot;
                    state.synced = Some(revision);
                    if slot.is_none() {
                        state.intent = Some(WriteIntent {
                            id: Id::now_v7(),
                            kind: WriteKind::Delete,
                            expected: state.synced.as_ref().map(|r| r.etag.clone()),
                        });
                    }
                }
                (
                    Resolution::RestoreFern,
                    PublicationConflict::Removed | PublicationConflict::OtherEvent,
                ) => {
                    let slot = slot.context("Schedule a timed work block first")?;
                    let task = task.context("Task is no longer available")?;
                    state.event_id = event_id(Id::now_v7());
                    state.synced = None;
                    state.last_local = None;
                    state.intent = Some(WriteIntent {
                        id: Id::now_v7(),
                        kind: WriteKind::Save {
                            slot,
                            title: task.title,
                        },
                        expected: None,
                    });
                }
                _ => anyhow::bail!("This event has no external time to accept"),
            }
            ensure!(
                self.save_publication(&publication, &state)?,
                "The publication changed. Review the refreshed conflict."
            );
            Ok(())
        })
    }

    fn save_publication(
        &self,
        publication: &Publication,
        state: &PublicationState,
    ) -> Result<bool> {
        if &publication.state == state {
            return Ok(true);
        }
        let count = self.conn.execute(
            "UPDATE calendar_publications SET state = ?2, generation = generation + 1 WHERE id = ?1 AND generation = ?3",
            params![publication.id.to_string(), serde_json::to_string(state)?, publication.generation],
        )?;
        Ok(count == 1)
    }

    fn remove_publication(&self, publication: &Publication) -> Result<()> {
        self.conn.execute(
            "DELETE FROM calendar_publications WHERE id = ?1 AND generation = ?2",
            params![publication.id.to_string(), publication.generation],
        )?;
        Ok(())
    }
}

fn same_occurrence(a: &Task, b: &Task) -> bool {
    a.created_at == b.created_at
        && a.title == b.title
        && a.kind == b.kind
        && a.list_id == b.list_id
        && a.parent_id == b.parent_id
        && a.due_date == b.due_date
        && a.due_time == b.due_time
        && a.end_date == b.end_date
        && a.end_time == b.end_time
        && a.timezone == b.timezone
        && a.is_floating == b.is_floating
}
