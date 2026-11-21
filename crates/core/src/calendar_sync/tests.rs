use super::*;

fn day(value: &str) -> NaiveDate {
    value.parse().unwrap()
}

fn block(start: u32, end: u32) -> TimeSlot {
    let at = |hour| wall_time(day("2026-11-23").and_hms_opt(hour, 0, 0).unwrap(), None).unwrap();
    TimeSlot::new(at(start), at(end)).unwrap()
}

fn calendar(provider: Provider, id: &str) -> ExternalCalendar {
    ExternalCalendar {
        key: CalendarKey {
            provider,
            account: "owner".into(),
            calendar: id.into(),
        },
        name: id.into(),
        account_name: "owner".into(),
        timezone: None,
        writable: provider == Provider::Google,
        present: true,
        visible: false,
        busy: false,
    }
}

fn event(id: &str, slot: TimeSlot, busy: bool) -> ExternalEvent {
    ExternalEvent {
        id: id.into(),
        title: id.into(),
        notes: String::new(),
        location: String::new(),
        span: EventSpan::Timed { slot },
        busy,
    }
}

#[test]
fn both_calendars_supply_busy_times_and_failures_keep_cached_events() -> Result<()> {
    let store = Store::open_in_memory()?;
    let google = calendar(Provider::Google, "work");
    let apple = calendar(Provider::ICloud, "home");
    let dates = DateRange::month(day("2026-11-23"));
    let prepare = |calendar: &ExternalCalendar, visible: bool, events: &[ExternalEvent]| {
        store.replace_external_calendars(calendar.key.provider, std::slice::from_ref(calendar))?;
        store.set_calendar_selection(&calendar.key, visible, true)?;
        store.save_calendar_windows(&calendar.key, dates, Ok(events), 100)
    };
    let (work, home, free) = (block(9, 10), block(10, 11), block(11, 12));
    prepare(
        &google,
        true,
        &[event("a", work, true), event("free", free, false)],
    )?;
    prepare(&apple, false, &[event("a", home, true)])?;
    let caches = store.cached_calendars()?;
    let proposed = block(9, 12);
    let result = availability(&caches, proposed, None, 101);
    assert!(!result.stale);
    assert_eq!(result.busy, [work, home]);
    assert_eq!(caches.iter().filter(|c| c.calendar.visible).count(), 1);
    assert_eq!(caches[0].events(dates).len(), 2);
    assert!(availability(&caches, free, None, 101).busy.is_empty());
    assert_eq!(
        availability(&caches, proposed, Some((&google.key, "a")), 101).busy,
        [home]
    );

    store.set_calendar_provider_error(Provider::ICloud, Some("access revoked"))?;
    store.save_calendar_windows(&google.key, dates, Err("offline"), 200)?;
    let caches = store.cached_calendars()?;
    let result = availability(&caches, proposed, None, 201);
    assert!(result.stale);
    assert_eq!(result.busy, [work, home]);
    assert_eq!(caches[0].windows[&dates.start].refreshed_at, Some(100));
    assert_eq!(store.add_task("Offline task")?.title, "Offline task");
    Ok(())
}

#[test]
fn all_day_dates_stay_civil_and_dst_affects_the_elapsed_interval() -> Result<()> {
    let span = EventSpan::AllDay {
        start: day("2026-03-08"),
        end: day("2026-03-09"),
    };
    assert_eq!(
        {
            let slot = span.busy_slot(Some("America/New_York"))?;
            (slot.end - slot.start).num_hours()
        },
        23
    );
    assert_eq!(span.display().start, day("2026-03-08"));
    assert_eq!(span.display().end, day("2026-03-08"));
    Ok(())
}

#[test]
fn suggestions_respect_work_hours_duration_occupied_time_and_deadline() -> Result<()> {
    let search = block(8, 12);
    let hours = WorkingHours {
        start: "09:00:00".parse()?,
        end: "17:00:00".parse()?,
        weekends: false,
    };
    let busy_end = block(9, 10).end + chrono::TimeDelta::seconds(30);
    let blocked = [TimeSlot::new(block(9, 10).start, busy_end)?];
    let duration = chrono::TimeDelta::hours;
    let results = suggest_slots(search, hours, duration(1), &blocked);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].start, minute_ceil(busy_end));
    assert!(results[0].end < search.end);
    assert!(suggest_slots(search, hours, duration(2), &blocked).is_empty());
    Ok(())
}

fn setup_publication(store: &Store) -> Result<(Id, CalendarKey, TimeSlot)> {
    let google = calendar(Provider::Google, "work");
    store.replace_external_calendars(Provider::Google, std::slice::from_ref(&google))?;
    store.set_publication_destination(Some(&google.key))?;
    let task = store.add_task("Project work")?;
    let slot = block(9, 10);
    store.schedule_work(task.id, slot, Some(&google.key))?;
    Ok((task.id, google.key, slot))
}

fn applied(publication: &Publication, slot: TimeSlot, etag: &str) -> RemoteEvent {
    RemoteEvent {
        revision: RemoteRevision {
            etag: etag.into(),
            slot: Some(slot),
        },
        block_id: Some(publication.id.to_string()),
        operation_id: publication
            .state
            .intent
            .as_ref()
            .map(|intent| intent.id.to_string()),
    }
}

#[test]
fn interrupted_create_and_external_edits_reconcile_by_identity_and_revision() -> Result<()> {
    let db = tempfile::NamedTempFile::new()?;
    let store = Store::open(db.path())?;
    let (task, key, slot) = setup_publication(&store)?;
    let first = store.publications()?.pop().unwrap();
    assert_eq!(
        first.reconcile(RemoteLookup::Absent, Some(slot)),
        Reconcile::Write
    );
    let inserted = applied(&first, slot, "v1");
    assert_eq!(store.queue_publication(task, &key)?, first.id);
    // The event already exists, but the local write response was lost.
    drop(store);
    let store = Store::open(db.path())?;
    let work = store.publications()?.pop().unwrap();
    assert_eq!(first.state.event_id, work.state.event_id);
    assert_eq!(first.state.intent, work.state.intent);
    let outcome = work.reconcile(RemoteLookup::Event(inserted.clone()), Some(slot));
    assert!(matches!(outcome, Reconcile::Synced { .. }));
    store.finish_publication(&work, Ok(outcome))?;

    let tomorrow = chrono::TimeDelta::days(1);
    let moved = TimeSlot::new(slot.start + tomorrow, slot.end + tomorrow)?;
    store.move_dates(task, moved.start.with_timezone(&Local).date_naive())?;
    assert_eq!(task_slot(&store.task(task)?.unwrap())?, Some(moved));
    let mut publication = store.publications()?.pop().unwrap();
    assert!(store.prepare_publication(&mut publication)?);
    assert_eq!(publication.state.event_id, first.state.event_id);
    assert_eq!(
        publication
            .state
            .intent
            .as_ref()
            .unwrap()
            .expected
            .as_deref(),
        Some("v1")
    );
    assert_eq!(
        publication.reconcile(RemoteLookup::Event(inserted), Some(moved)),
        Reconcile::Write
    );
    let external = TimeSlot::new(
        moved.start + chrono::TimeDelta::hours(1),
        moved.end + chrono::TimeDelta::hours(1),
    )?;
    let edited = applied(&publication, external, "v2");
    let conflict = publication.reconcile(RemoteLookup::Event(edited), Some(moved));
    assert!(matches!(
        conflict,
        Reconcile::Conflict(PublicationConflict::Changed(_))
    ));
    store.finish_publication(&publication, Ok(conflict))?;
    assert_eq!(task_slot(&store.task(task)?.unwrap())?, Some(moved));

    store.resolve_publication(publication.id, Resolution::RestoreFern)?;
    let mut restore = store.publications()?.pop().unwrap();
    assert!(store.prepare_publication(&mut restore)?);
    assert_eq!(
        restore.state.intent.as_ref().unwrap().expected.as_deref(),
        Some("v2")
    );
    assert_eq!(task_slot(&store.task(task)?.unwrap())?, Some(moved));
    let concurrent_edit = applied(&restore, external, "v3");
    let conflict = restore.reconcile(RemoteLookup::Event(concurrent_edit), Some(moved));
    assert!(matches!(
        conflict,
        Reconcile::Conflict(PublicationConflict::Changed(_))
    ));
    store.finish_publication(&restore, Ok(conflict))?;
    store.resolve_publication(restore.id, Resolution::AcceptTime)?;
    assert_eq!(task_slot(&store.task(task)?.unwrap())?, Some(external));

    let publication = store.publications()?.pop().unwrap();
    let deleted = publication.reconcile(RemoteLookup::Deleted, Some(external));
    assert_eq!(deleted, Reconcile::Conflict(PublicationConflict::Removed));
    store.finish_publication(&publication, Ok(deleted))?;
    let mut waiting = store.publications()?.pop().unwrap();
    assert!(!store.prepare_publication(&mut waiting)?);
    store.resolve_publication(publication.id, Resolution::RestoreFern)?;
    let retry = store.publications()?.pop().unwrap();
    assert_eq!(retry.id, first.id);
    assert_ne!(retry.state.event_id, first.state.event_id);
    assert_eq!(
        retry.reconcile(RemoteLookup::Absent, Some(external)),
        Reconcile::Write
    );
    Ok(())
}

#[test]
fn completing_a_repeat_transfers_publication_to_the_historical_copy() -> Result<()> {
    let store = Store::open_in_memory()?;
    let (task, _, slot) = setup_publication(&store)?;
    let original = store.task(task)?.unwrap();
    store.set_repeat(
        task,
        Some("FREQ=DAILY".parse()?),
        original.due_date.unwrap(),
    )?;
    let first = store.publications()?.pop().unwrap();
    let copy = store.complete(task, true)?.unwrap();
    let publication = store.publications()?.pop().unwrap();
    assert_eq!(publication.id, first.id);
    assert_eq!(publication.task_id, Some(copy));
    assert_eq!(task_slot(&store.task(copy)?.unwrap())?, Some(slot));
    assert_ne!(task_slot(&store.task(task)?.unwrap())?, Some(slot));
    store.undo_repeat(task, copy, original.due_date, original.end_date, None)?;
    assert_eq!(store.publications()?.pop().unwrap().task_id, Some(task));
    Ok(())
}

#[test]
fn clearing_a_queued_block_does_not_create_an_event() -> Result<()> {
    let store = Store::open_in_memory()?;
    let (task, _, _) = setup_publication(&store)?;
    let publication = store.publications()?.pop().unwrap();
    store.delete_task(task)?;
    assert_eq!(
        publication.reconcile(RemoteLookup::Absent, None),
        Reconcile::Cancelled
    );
    store.finish_publication(&publication, Ok(Reconcile::Cancelled))?;
    assert!(store.publications()?.is_empty());
    Ok(())
}

#[test]
fn restoring_a_task_during_cancellation_keeps_the_publication_choice() -> Result<()> {
    let store = Store::open_in_memory()?;
    let (task, _, slot) = setup_publication(&store)?;
    let publication = store.publications()?.pop().unwrap();
    let synced = publication.reconcile(
        RemoteLookup::Event(applied(&publication, slot, "v1")),
        Some(slot),
    );
    store.finish_publication(&publication, Ok(synced))?;
    store.delete_task(task)?;
    let mut pending = store.publications()?.pop().unwrap();
    assert!(store.prepare_publication(&mut pending)?);
    store.restore_task(task)?;
    store.finish_publication(&pending, Ok(Reconcile::Cancelled))?;
    assert_eq!(
        store.publications()?.pop().unwrap().state.conflict,
        Some(PublicationConflict::Removed)
    );
    Ok(())
}

#[test]
fn synced_repeat_moves_local_binding_without_sharing_publication_state() -> Result<()> {
    let mac = Store::open_in_memory()?;
    let other = Store::open_in_memory()?;
    let (task, _, slot) = setup_publication(&mac)?;
    let original = mac.task(task)?.unwrap();
    mac.set_repeat(
        task,
        Some("FREQ=DAILY".parse()?),
        original.due_date.unwrap(),
    )?;
    mac.set_follow_up(
        task,
        Some(crate::follow_up::FollowUpOptions {
            delay_minutes: 5,
            interval_minutes: Some(10),
        }),
    )?;
    let first = mac.publications()?.pop().unwrap();
    let (changes, sent) = mac.changes_since(0)?;
    assert!(changes.iter().all(|change| change.entity == "tasks"));
    other.apply_changes(&changes)?;
    assert!(other.publications()?.is_empty());
    assert!(other.follow_up_options(task)?.is_none());

    let copy = other.complete(task, true)?.unwrap();
    let (rolled, since) = other.changes_since(0)?;
    mac.apply_changes(&rolled)?;
    let published = mac.publications()?.pop().unwrap();
    assert_eq!(published.id, first.id);
    assert_eq!(published.task_id, Some(copy));
    assert_eq!(task_slot(&mac.task(copy)?.unwrap())?, Some(slot));
    assert!(mac.follow_up_options(task)?.is_none());

    other.undo_repeat(task, copy, original.due_date, original.end_date, None)?;
    mac.apply_changes(&other.changes_since(since)?.0)?;
    assert_eq!(mac.publications()?.pop().unwrap().task_id, Some(task));
    assert!(mac.changes_since(sent)?.0.is_empty());
    Ok(())
}
