//! Desktop calendar refresh and publication. Only the macOS app initializes this service.
mod apple;
mod google;

use std::{
    collections::{BTreeSet, HashSet},
    rc::Rc,
    time::Duration,
};

use anyhow::{Context as _, Result, ensure};
use chrono::{Local, Utc};
use fern_core::{
    Id, Store,
    calendar_sync::{
        self, Availability, CachedCalendar, CalendarKey, DateRange, ExternalCalendar,
        ExternalEvent, Provider, Publication, Reconcile, Resolution, TimeSlot, month_ranges,
        task_slot,
    },
};
use gpui::{App, Context, Entity, Global, prelude::*};

use self::google::{Authorization, Credentials, Google, OAuthClient};

const REFRESH_MS: i64 = 5 * 60_000;
const POLL_PUBLICATIONS_MS: i64 = 45_000;
const RETRY_MS: i64 = 20_000;
const PROFILE: &str = "calendar_keychain_profile";
const ICLOUD: &str = "calendar_icloud_connected";

struct GlobalAccounts(Entity<Accounts>);
impl Global for GlobalAccounts {}

pub fn init(store: Rc<Store>, cx: &mut App) {
    if !cfg!(target_os = "macos") || std::env::var_os("FERN_DEV_STATE").is_some() {
        return;
    }
    apple::start_network_monitor();
    let result = (|| -> Result<_> {
        store.insert_setting(PROFILE, &Id::now_v7().to_string())?;
        let profile = store
            .setting(PROFILE)?
            .context("Could not initialize Calendar Keychain profile")?;
        let today = Local::now().date_naive();
        Ok(Accounts {
            profile,
            visible: DateRange::month(today),
            proposed: None,
            calendars: store.cached_calendars()?,
            destination: store.publication_destination()?,
            icloud_connected: store.setting(ICLOUD)?.is_some(),
            provider_errors: [
                store.calendar_provider_error(Provider::Google)?,
                store.calendar_provider_error(Provider::ICloud)?,
            ],
            store,
            publications: Vec::new(),
            google: GoogleStatus::from_credentials(&Credentials::default()),
            next_refresh: 0,
            next_publish: 0,
            refresh_running: false,
            publish_running: false,
            account_action_running: false,
            action_error: None,
            account_version: 0,
            connection: apple::connection(),
        })
    })();
    match result {
        Ok(mut accounts) => {
            let entity = cx.new(|cx| {
                accounts.start(cx);
                accounts
            });
            cx.set_global(GlobalAccounts(entity));
        }
        Err(error) => eprintln!("Could not initialize calendar integrations: {error:#}"),
    }
}

pub fn accounts(cx: &App) -> Option<Entity<Accounts>> {
    cx.try_global::<GlobalAccounts>()
        .map(|global| global.0.clone())
}

pub struct Accounts {
    store: Rc<Store>,
    profile: String,
    visible: DateRange,
    proposed: Option<DateRange>,
    pub calendars: Vec<CachedCalendar>,
    pub destination: Option<CalendarKey>,
    pub publications: Vec<PublicationStatus>,
    pub google: GoogleStatus,
    pub icloud_connected: bool,
    provider_errors: [Option<String>; 2],
    next_refresh: i64,
    next_publish: i64,
    pub refresh_running: bool,
    publish_running: bool,
    pub account_action_running: bool,
    pub action_error: Option<String>,
    account_version: u64,
    connection: (bool, bool),
}

pub struct GoogleStatus {
    pub configured: bool,
    pub account: Option<String>,
    pub publishing: bool,
}

impl GoogleStatus {
    fn from_credentials(credentials: &Credentials) -> Self {
        Self {
            configured: credentials.client.is_some() || OAuthClient::bundled().is_some(),
            account: credentials.read.as_ref().map(|grant| grant.email.clone()),
            publishing: credentials.write.is_some(),
        }
    }
}

pub struct PublicationStatus {
    pub publication: Publication,
    pub task_title: String,
    pub local_slot: Option<TimeSlot>,
    pub pending: bool,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Accounts {
    fn start(&mut self, cx: &mut Context<Self>) {
        self.reload_publications();
        cx.observe_global::<crate::data_version::DataVersion>(|this, cx| {
            this.reload_publications();
            if this
                .publications
                .iter()
                .any(|p| p.pending && p.publication.state.conflict.is_none())
            {
                this.next_publish = 0;
            }
            cx.notify();
        })
        .detach();
        cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let connection = apple::connection();
        if self.connection != connection {
            self.connection = connection;
            self.next_refresh = 0;
            self.next_publish = 0;
            cx.notify();
        }
        let now = now_ms();
        if !self.refresh_running && now >= self.next_refresh {
            self.refresh(cx);
        }
        if !self.publish_running && !self.account_action_running && now >= self.next_publish {
            self.publish(cx);
        }
    }

    pub fn provider_error(&self, provider: Provider) -> Option<&str> {
        self.provider_errors[match provider {
            Provider::Google => 0,
            Provider::ICloud => 1,
        }]
        .as_deref()
    }

    pub fn set_visible(&mut self, dates: DateRange) {
        if !self.visible.months().eq(dates.months()) {
            self.next_refresh = 0;
        }
        self.visible = dates;
    }

    pub fn check_dates(&mut self, dates: DateRange) {
        if self
            .proposed
            .is_none_or(|previous| !previous.months().eq(dates.months()))
        {
            self.proposed = Some(dates);
            if self
                .calendars
                .iter()
                .any(|cache| cache.calendar.busy && !cache.fresh(dates, now_ms()))
            {
                self.next_refresh = 0;
            }
        }
    }

    pub fn stale(&self, dates: DateRange) -> bool {
        let now = now_ms();
        self.calendars.iter().any(|cache| {
            (cache.calendar.visible || cache.calendar.busy)
                && self.calendar_stale(cache, dates, now)
        })
    }

    pub fn calendar_stale(&self, cache: &CachedCalendar, dates: DateRange, now: i64) -> bool {
        !cache.fresh(dates, now)
            || !self.connection.0
            || (!self.connection.1 && cache.calendar.key.provider == Provider::ICloud)
    }

    pub fn calendar_availability(&self, task: Id, slot: TimeSlot) -> Availability {
        let exclude = self
            .publications
            .iter()
            .find(|p| p.publication.task_id == Some(task) && p.publication.state.conflict.is_none())
            .map(|p| {
                (
                    &p.publication.state.destination,
                    p.publication.state.event_id.as_str(),
                )
            });
        let mut result = calendar_sync::availability(&self.calendars, slot, exclude, now_ms());
        let (online, icloud_access) = self.connection;
        result.stale |= self.calendars.iter().any(|cache| {
            cache.calendar.busy
                && (!online || (!icloud_access && cache.calendar.key.provider == Provider::ICloud))
        });
        result
    }

    pub fn displayed_events(&self, dates: DateRange) -> Vec<(&ExternalCalendar, &ExternalEvent)> {
        self.calendars
            .iter()
            .filter(|c| c.calendar.visible)
            .flat_map(|c| {
                c.events(dates)
                    .into_iter()
                    .map(move |event| (&c.calendar, event))
            })
            .filter(|(calendar, event)| {
                !self.publications.iter().any(|p| {
                    p.publication.state.destination == calendar.key
                        && p.publication.state.event_id == event.id
                        && p.local_slot.is_some_and(|slot| {
                            event.span == calendar_sync::EventSpan::Timed { slot }
                        })
                })
            })
            .collect()
    }

    pub fn set_selection(
        &mut self,
        key: &CalendarKey,
        visible: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) {
        let result = self.store.set_calendar_selection(key, visible, busy);
        self.handle(result);
        self.reload_calendars();
        self.next_refresh = 0;
        self.tick(cx);
        cx.notify();
    }

    pub fn set_destination(&mut self, key: Option<&CalendarKey>, cx: &mut Context<Self>) {
        let result = self.store.set_publication_destination(key);
        if result.is_ok() {
            self.destination = key.cloned();
        }
        self.handle(result);
        cx.notify();
    }

    pub fn resolve(&mut self, id: Id, choice: Resolution, cx: &mut Context<Self>) {
        let result = self.store.resolve_publication(id, choice);
        if result.is_ok() {
            self.reload_publications();
            self.next_publish = 0;
            crate::data_version::data_changed(cx.entity_id(), cx);
        }
        self.handle(result);
        self.tick(cx);
        cx.notify();
    }

    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.next_refresh = 0;
        self.next_publish = 0;
        self.tick(cx);
        cx.notify();
    }

    pub fn configure_google(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        if self.account_action_running {
            return;
        }
        self.account_action_running = true;
        self.action_error = None;
        let profile = self.profile.clone();
        let work = cx.background_spawn(async move {
            let client = OAuthClient::read(&path)?;
            ensure!(
                apple::credentials(&profile)?.read.is_none(),
                "Disconnect Google before changing its OAuth client"
            );
            let credentials = Credentials {
                client: Some(client),
                ..Default::default()
            };
            apple::save_credentials(&profile, &credentials)?;
            Ok(GoogleStatus::from_credentials(&credentials))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| this.finish_google_action(result, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn connect_google(&mut self, publishing: bool, cx: &mut Context<Self>) {
        if self.account_action_running {
            return;
        }
        self.account_action_running = true;
        self.action_error = None;
        let profile = self.profile.clone();
        cx.spawn(async move |this, cx| {
            let setup = cx
                .background_executor()
                .spawn(async move {
                    let mut credentials = apple::credentials(&profile)?;
                    // Keep the OAuth client paired with the grants across app updates.
                    credentials.client = credentials.client.or_else(OAuthClient::bundled);
                    let client = credentials
                        .client
                        .as_ref()
                        .context("Choose a Google Desktop OAuth client file")?;
                    ensure!(
                        !publishing || credentials.read.is_some(),
                        "Connect Google before enabling publishing"
                    );
                    let flow = Authorization::start(
                        client.clone(),
                        publishing,
                        credentials.read.as_ref(),
                    )?;
                    Ok::<_, anyhow::Error>((profile, credentials, flow))
                })
                .await;
            let (profile, mut credentials, flow) = match setup {
                Ok(value) => value,
                Err(error) => {
                    this.update(cx, |this, cx| this.finish_google_action(Err(error), cx))
                        .ok();
                    return;
                }
            };
            cx.update(|cx| cx.open_url(flow.url.as_str()));
            let (send, receive) = futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let result = (|| {
                    let grant = flow.finish()?;
                    if publishing {
                        ensure!(
                            credentials
                                .read
                                .as_ref()
                                .is_some_and(|read| read.account == grant.account),
                            "Use the Google account already connected to Fern"
                        );
                        credentials.write = Some(grant);
                    } else {
                        if credentials
                            .write
                            .as_ref()
                            .is_some_and(|write| write.account != grant.account)
                        {
                            credentials.write = None;
                        }
                        credentials.read = Some(grant);
                    }
                    apple::save_credentials(&profile, &credentials)?;
                    Ok(GoogleStatus::from_credentials(&credentials))
                })();
                let _ = send.send(result);
            });
            let result = receive
                .await
                .context("Google sign-in did not finish")
                .and_then(|result| result);
            this.update(cx, |this, cx| this.finish_google_action(result, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn disconnect_google(&mut self, cx: &mut Context<Self>) {
        if self.account_action_running {
            return;
        }
        self.account_action_running = true;
        self.action_error = None;
        let profile = self.profile.clone();
        let work = cx.background_spawn(async move {
            let credentials = Credentials {
                client: apple::credentials(&profile)?.client,
                ..Default::default()
            };
            apple::save_credentials(&profile, &credentials)?;
            Ok(GoogleStatus::from_credentials(&credentials))
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                if result.is_ok() {
                    let result = this.store.set_calendar_provider_error(
                        Provider::Google,
                        Some("Google Calendar is disconnected"),
                    );
                    this.handle(result);
                    this.reload_calendars();
                }
                this.finish_google_action(result, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_google_action(&mut self, result: Result<GoogleStatus>, cx: &mut Context<Self>) {
        self.account_action_running = false;
        match result {
            Ok(google) => {
                self.account_version += 1;
                self.google = google;
                self.refresh_now(cx);
            }
            Err(error) => self.action_error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    pub fn connect_icloud(&mut self, cx: &mut Context<Self>) {
        if self.account_action_running {
            return;
        }
        let access = match apple::request_access() {
            Ok(access) => access,
            Err(error) => {
                self.action_error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        self.account_action_running = true;
        self.action_error = None;
        cx.spawn(async move |this, cx| {
            let response = access.await.unwrap_or(0);
            this.update(cx, |this, cx| {
                this.account_action_running = false;
                let result = (|| {
                    ensure!(
                        response != -1,
                        "Run Fern as a packaged macOS app to connect iCloud"
                    );
                    this.store.set_setting(ICLOUD, "1")?;
                    this.icloud_connected = true;
                    this.account_version += 1;
                    Ok(())
                })();
                this.handle(result);
                this.refresh_now(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn handle(&mut self, result: Result<()>) {
        self.action_error = result.err().map(|e| format!("{e:#}"));
    }

    fn reload_calendars(&mut self) {
        let result = (|| -> Result<()> {
            self.calendars = self.store.cached_calendars()?;
            self.provider_errors = [
                self.store.calendar_provider_error(Provider::Google)?,
                self.store.calendar_provider_error(Provider::ICloud)?,
            ];
            Ok(())
        })();
        if let Err(error) = result {
            self.action_error = Some(format!("{error:#}"));
        }
    }

    fn reload_publications(&mut self) {
        let result = (|| -> Result<Vec<_>> {
            self.store
                .publications()?
                .into_iter()
                .map(|publication| {
                    let task = publication
                        .task_id
                        .map(|id| self.store.task(id))
                        .transpose()?
                        .flatten();
                    let local_slot = task
                        .as_ref()
                        .and_then(|task| task_slot(task).ok().flatten());
                    let pending = publication.pending(task.as_ref());
                    let task_title = task.map_or_else(String::new, |task| task.title);
                    Ok(PublicationStatus {
                        publication,
                        task_title,
                        local_slot,
                        pending,
                    })
                })
                .collect()
        })();
        match result {
            Ok(publications) => self.publications = publications,
            Err(error) => self.action_error = Some(format!("{error:#}")),
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.next_refresh = now_ms() + REFRESH_MS;
        self.refresh_running = true;
        let account_version = self.account_version;
        let months: BTreeSet<_> = self
            .visible
            .months()
            .chain(DateRange::planning(Local::now().date_naive()).months())
            .chain(self.proposed.into_iter().flat_map(DateRange::months))
            .collect();
        let work = ReadWork {
            profile: self.profile.clone(),
            icloud: self.icloud_connected,
            ranges: month_ranges(months),
            selected: self
                .calendars
                .iter()
                .filter(|c| c.calendar.visible || c.calendar.busy)
                .map(|c| c.calendar.key.clone())
                .collect(),
        };
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { work.run() })
                .await;
            this.update(cx, |this, cx| {
                this.refresh_running = false;
                if this.account_version == account_version {
                    let result = this.finish_refresh(outcome);
                    if let Err(error) = result {
                        this.action_error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_refresh(&mut self, read: ReadOutcome) -> Result<()> {
        if let Some(google) = read.google_status
            && !self.account_action_running
        {
            self.google = google;
        }
        let at = now_ms();
        for (provider, result) in read.providers {
            match result {
                Err(error) => self
                    .store
                    .set_calendar_provider_error(provider, Some(&format!("{error:#}")))?,
                Ok((calendars, events)) => {
                    self.store
                        .replace_external_calendars(provider, &calendars)?;
                    for (key, dates, events) in events {
                        let error = events.as_ref().err().map(|err| format!("{err:#}"));
                        self.store.save_calendar_windows(
                            &key,
                            dates,
                            events
                                .as_deref()
                                .map_err(|_| error.as_deref().unwrap_or_default()),
                            at,
                        )?;
                    }
                }
            }
        }
        self.reload_calendars();
        Ok(())
    }

    fn publish(&mut self, cx: &mut Context<Self>) {
        self.next_publish = now_ms() + POLL_PUBLICATIONS_MS;
        let work = self.store.publications().map(|publications| {
            publications
                .into_iter()
                .filter_map(|mut publication| {
                    match self.store.prepare_publication(&mut publication) {
                        Ok(true) => {
                            let desired = (|| {
                                let task = publication
                                    .task_id
                                    .map(|id| self.store.task(id))
                                    .transpose()?
                                    .flatten();
                                Ok::<_, anyhow::Error>(
                                    task.as_ref().map(task_slot).transpose()?.flatten(),
                                )
                            })();
                            match desired {
                                Ok(slot) => Some((publication, slot)),
                                Err(error) => {
                                    let _ = self.store.finish_publication(
                                        &publication,
                                        Err(&format!("{error:#}")),
                                    );
                                    None
                                }
                            }
                        }
                        Ok(false) => None,
                        Err(error) => {
                            let _ = self
                                .store
                                .finish_publication(&publication, Err(&format!("{error:#}")));
                            None
                        }
                    }
                })
                .collect::<Vec<_>>()
        });
        let work = match work {
            Ok(work) => work,
            Err(error) => {
                self.action_error = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        if work.is_empty() {
            self.reload_publications();
            return;
        }
        self.publish_running = true;
        let profile = self.profile.clone();
        cx.spawn(async move |this, cx| {
            let outcomes = cx
                .background_executor()
                .spawn(async move {
                    let google = (|| {
                        let credentials = apple::credentials(&profile)?;
                        let client = credentials
                            .client
                            .as_ref()
                            .context("Choose a Google OAuth client file")?;
                        let grant = credentials
                            .write
                            .as_ref()
                            .context("Enable Google publishing in Integrations")?;
                        Google::connect(client, grant)
                    })();
                    work.into_iter()
                        .map(|(publication, desired)| {
                            let result = google
                                .as_ref()
                                .map_err(|err| format!("{err:#}"))
                                .and_then(|google| {
                                    google
                                        .publish(&publication, desired)
                                        .map_err(|err| format!("{err:#}"))
                                });
                            (publication, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.publish_running = false;
                for (publication, result) in outcomes {
                    if matches!(&result, Ok(Reconcile::Synced { .. } | Reconcile::Cancelled)) {
                        this.next_refresh = 0;
                    }
                    let result = match result {
                        Ok(value) => Ok(value),
                        Err(ref error) => Err(error.as_str()),
                    };
                    if let Err(error) = this.store.finish_publication(&publication, result) {
                        this.action_error = Some(format!("{error:#}"));
                    }
                }
                this.reload_publications();
                if this
                    .publications
                    .iter()
                    .any(|p| p.pending && p.publication.state.conflict.is_none())
                {
                    this.next_publish = this.next_publish.min(now_ms() + RETRY_MS);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

struct ReadWork {
    profile: String,
    icloud: bool,
    ranges: Vec<DateRange>,
    selected: HashSet<CalendarKey>,
}

type ReadEvents = (CalendarKey, DateRange, Result<Vec<ExternalEvent>>);
type ProviderRead = Result<(Vec<ExternalCalendar>, Vec<ReadEvents>)>;

struct ReadOutcome {
    google_status: Option<GoogleStatus>,
    providers: Vec<(Provider, ProviderRead)>,
}

impl ReadWork {
    fn run(self) -> ReadOutcome {
        let mut outcome = ReadOutcome {
            google_status: None,
            providers: Vec::new(),
        };
        if self.icloud {
            outcome.providers.push((
                Provider::ICloud,
                apple::calendars().map(|catalogue| {
                    self.collect(catalogue, |calendar, range| {
                        apple::events(&calendar.key, range)
                    })
                }),
            ));
        }
        let google = (|| -> ProviderRead {
            let credentials = apple::credentials(&self.profile)?;
            outcome.google_status = Some(GoogleStatus::from_credentials(&credentials));
            let (Some(client), Some(grant)) =
                (credentials.client.as_ref(), credentials.read.as_ref())
            else {
                if self
                    .selected
                    .iter()
                    .any(|key| key.provider == Provider::Google)
                {
                    anyhow::bail!("Google Calendar is disconnected");
                }
                return Ok((Vec::new(), Vec::new()));
            };
            let google = Google::connect(client, grant)?;
            Ok(self.collect(google.calendars()?, |calendar, range| {
                google.events(calendar, range)
            }))
        })();
        outcome.providers.push((Provider::Google, google));
        outcome
    }

    fn collect(
        &self,
        calendars: Vec<ExternalCalendar>,
        read: impl Fn(&ExternalCalendar, DateRange) -> Result<Vec<ExternalEvent>>,
    ) -> (Vec<ExternalCalendar>, Vec<ReadEvents>) {
        let mut events = Vec::new();
        for calendar in calendars
            .iter()
            .filter(|calendar| self.selected.contains(&calendar.key))
        {
            for &range in &self.ranges {
                events.push((calendar.key.clone(), range, read(calendar, range)));
            }
        }
        (calendars, events)
    }
}

/// The last good refresh for this calendar, even when its current coverage is stale.
pub fn last_refresh(calendar: &CachedCalendar) -> Option<chrono::DateTime<Local>> {
    calendar
        .windows
        .values()
        .filter_map(|window| window.refreshed_at)
        .max()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.with_timezone(&Local))
}
