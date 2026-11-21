//! Google Calendar REST requests and the installed-app OAuth loopback flow.
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use fern_core::calendar_sync::{
    CalendarKey, DateRange, EventSpan, ExternalCalendar, ExternalEvent, Provider, Publication,
    PublicationConflict, Reconcile, RemoteEvent, RemoteLookup, RemoteRevision, TimeSlot, WriteKind,
    wall_time,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use url::Url;

const AUTH: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const USER: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const BASE: &str = "https://www.googleapis.com/calendar/v3/";
const READ: &str = "https://www.googleapis.com/auth/calendar.readonly";
const WRITE: &str = "https://www.googleapis.com/auth/calendar.events";
const MAX_REPLY: u64 = 24 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct OAuthClient {
    client_id: String,
    client_secret: String,
}

impl OAuthClient {
    pub(super) fn bundled() -> Option<Self> {
        let json = option_env!("FERN_GOOGLE_OAUTH_CLIENT_JSON")?;
        Self::from_reader(json.as_bytes()).ok()
    }

    pub(super) fn read(path: &Path) -> Result<Self> {
        Self::from_reader(std::fs::File::open(path)?.take(128 * 1024))
    }

    fn from_reader(contents: impl std::io::Read) -> Result<Self> {
        #[derive(Deserialize)]
        struct Download {
            installed: OAuthClient,
        }
        let config: Download = serde_json::from_reader(contents)
            .context("Choose a Google Desktop app OAuth client JSON file")?;
        ensure!(
            !config.installed.client_secret.is_empty()
                && config
                    .installed
                    .client_id
                    .ends_with(".apps.googleusercontent.com"),
            "Invalid Google Desktop app client"
        );
        Ok(config.installed)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Grant {
    pub account: String,
    pub email: String,
    refresh: String,
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct Credentials {
    pub client: Option<OAuthClient>,
    pub read: Option<Grant>,
    pub write: Option<Grant>,
}

pub(super) struct Authorization {
    pub url: Url,
    /// Set from the UI to stop waiting for the browser.
    pub cancel: Arc<AtomicBool>,
    client: OAuthClient,
    listener: TcpListener,
    nonce: String,
    verifier: String,
    redirect: String,
    write: bool,
}

impl Authorization {
    pub(super) fn start(client: OAuthClient, write: bool, current: Option<&Grant>) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("Cannot listen for Google's sign-in response")?;
        listener.set_nonblocking(true)?;
        let redirect = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
        let nonce = random_string()?;
        let verifier = random_string()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let mut url = Url::parse(AUTH)?;
        let scope = if write {
            format!("openid email {READ} {WRITE}")
        } else {
            format!("openid email {READ}")
        };
        url.query_pairs_mut().extend_pairs([
            ("client_id", client.client_id.as_str()),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("scope", &scope),
            ("state", &nonce),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ]);
        if let Some(grant) = current {
            url.query_pairs_mut()
                .append_pair("login_hint", &grant.email);
        }
        Ok(Self {
            url,
            cancel: Arc::default(),
            client,
            listener,
            nonce,
            verifier,
            redirect,
            write,
        })
    }

    pub(super) fn finish(self) -> Result<Grant> {
        let code = self.wait_for_code()?;
        let token = exchange(&[
            ("code", &code),
            ("client_id", &self.client.client_id),
            ("client_secret", &self.client.client_secret),
            ("redirect_uri", &self.redirect),
            ("code_verifier", &self.verifier),
            ("grant_type", "authorization_code"),
        ])?;
        let granted: Vec<_> = token.scope.split_ascii_whitespace().collect();
        ensure!(
            granted.contains(&READ) && (!self.write || granted.contains(&WRITE)),
            "The required Google Calendar permission was declined"
        );
        let (_, identity) = request("GET", USER, Some(&token.access_token), None, None)?
            .success("Read Google account")?;
        let account = required_str(&identity, "sub")?.to_owned();
        let email = required_str(&identity, "email")?.to_owned();
        let refresh = token
            .refresh_token
            .context("Google did not supply offline access")?;
        Ok(Grant {
            account,
            email,
            refresh,
        })
    }

    fn wait_for_code(&self) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            if self.cancel.load(Ordering::Relaxed) {
                bail!("Google sign-in was cancelled");
            }
            let (mut stream, _) = match self.listener.accept() {
                Ok(peer) => peer,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut line = String::new();
            let parsed = (|| {
                BufReader::new(&mut stream)
                    .take(8192)
                    .read_line(&mut line)
                    .ok()?;
                let mut fields = line.split_whitespace();
                let target = fields
                    .next()
                    .filter(|method| *method == "GET")
                    .and_then(|_| fields.next())?;
                let query = target.strip_prefix("/?")?;
                let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
                let state: Vec<_> = pairs.iter().filter(|(key, _)| key == "state").collect();
                if state.len() != 1 || state[0].1 != self.nonce {
                    return None;
                }
                Some(
                    pairs
                        .into_iter()
                        .find(|(key, _)| key == "code")
                        .map(|(_, code)| code.into_owned()),
                )
            })();
            let (status, message) = match parsed {
                Some(Some(_)) => (
                    "200 OK",
                    "Google sign-in received. You can close this tab and return to Fern.",
                ),
                Some(None) => (
                    "200 OK",
                    "Google sign-in was cancelled. You can close this tab.",
                ),
                None => (
                    "400 Bad Request",
                    "This request did not match an active Fern sign-in.",
                ),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
                message.len()
            );
            if let Some(result) = parsed {
                return result.context("Google sign-in was cancelled");
            }
        }
        bail!("Google sign-in timed out");
    }
}

fn random_string() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| anyhow::anyhow!("Could not generate a sign-in challenge: {err}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    scope: String,
}

fn exchange(fields: &[(&str, &str)]) -> Result<TokenResponse> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish();
    let (_, body) = request(
        "POST",
        TOKEN,
        None,
        Some(("application/x-www-form-urlencoded", form)),
        None,
    )?
    .success("Google token exchange")?;
    serde_json::from_value(body).context("Google returned an incomplete token response")
}

fn api_url(parts: &[&str]) -> Result<Url> {
    let mut url = Url::parse(BASE)?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid Google API base URL"))?
        .pop_if_empty()
        .extend(parts);
    Ok(url)
}

struct Response {
    status: u16,
    body: Value,
}

impl Response {
    fn success(self, operation: &str) -> Result<(u16, Value)> {
        ensure!(
            (200..300).contains(&self.status),
            "{operation} failed (Google HTTP {})",
            self.status
        );
        Ok((self.status, self.body))
    }
}

fn request(
    method: &str,
    url: &str,
    access: Option<&str>,
    body: Option<(&str, String)>,
    etag: Option<&str>,
) -> Result<Response> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = ureq::http::Request::builder()
        .method(method)
        .uri(url)
        .header("Accept", "application/json");
    if let Some(token) = access {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(etag) = etag {
        request = request.header("If-Match", etag);
    }
    let payload = if let Some((content_type, data)) = body {
        request = request.header("Content-Type", content_type);
        data
    } else {
        String::new()
    };
    let request = request
        .body(payload)
        .map_err(|_| anyhow::anyhow!("Could not prepare Google Calendar request"))?;
    let mut response = agent
        .run(request)
        .map_err(|_| anyhow::anyhow!("Could not reach Google Calendar"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) || status == 204 {
        return Ok(Response {
            status,
            body: Value::Null,
        });
    }
    let text = response
        .body_mut()
        .with_config()
        .limit(MAX_REPLY)
        .read_to_string()
        .context("Incomplete Google response")?;
    let body = serde_json::from_str(&text).context("Invalid Google response")?;
    Ok(Response { status, body })
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("Google response is missing {key}"))
}

fn optional_str(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn writable(role: &str) -> bool {
    matches!(role, "owner" | "writer" | "writerWithoutPrivateAccess")
}

pub(super) struct Google {
    account: String,
    email: String,
    access: String,
}

impl Google {
    pub(super) fn connect(client: &OAuthClient, grant: &Grant) -> Result<Self> {
        let token = exchange(&[
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("refresh_token", &grant.refresh),
            ("grant_type", "refresh_token"),
        ])
        .context("Google Calendar access may need reconnecting")?;
        Ok(Self {
            account: grant.account.clone(),
            email: grant.email.clone(),
            access: token.access_token,
        })
    }

    pub(super) fn calendars(&self) -> Result<Vec<ExternalCalendar>> {
        let url = api_url(&["users", "me", "calendarList"])?;
        self.list(url, "List Google calendars")?
            .into_iter()
            .map(|entry| {
                let id = required_str(&entry, "id")?.to_owned();
                let role = required_str(&entry, "accessRole")?;
                Ok(ExternalCalendar {
                    key: CalendarKey {
                        provider: Provider::Google,
                        account: self.account.clone(),
                        calendar: id.clone(),
                    },
                    name: optional_str(&entry["summaryOverride"])
                        .or_else(|| optional_str(&entry["summary"]))
                        .unwrap_or(id),
                    account_name: self.email.clone(),
                    timezone: optional_str(&entry["timeZone"]),
                    writable: writable(role),
                    present: true,
                    visible: false,
                    busy: false,
                })
            })
            .collect()
    }

    pub(super) fn events(
        &self,
        calendar: &ExternalCalendar,
        range: DateRange,
    ) -> Result<Vec<ExternalEvent>> {
        self.check_account(&calendar.key)?;
        let bounds = range.utc_bounds(None)?;
        let provider_bounds = range.utc_bounds(calendar.timezone.as_deref())?;
        let mut url = api_url(&["calendars", &calendar.key.calendar, "events"])?;
        url.query_pairs_mut().extend_pairs([
            ("singleEvents", "true"),
            ("maxResults", "2500"),
            (
                "timeMin",
                &bounds.start.min(provider_bounds.start).to_rfc3339(),
            ),
            ("timeMax", &bounds.end.max(provider_bounds.end).to_rfc3339()),
        ]);
        match self.list(url, "Read Google events") {
            Ok(events) => events
                .into_iter()
                .filter(|event| event["status"] != "cancelled")
                .map(|event| imported_event(&event, calendar.timezone.as_deref()))
                .collect(),
            Err(err) => {
                let url = api_url(&["users", "me", "calendarList", &calendar.key.calendar])?;
                let (_, info) = request("GET", url.as_str(), Some(&self.access), None, None)?
                    .success("Read Google calendar")?;
                if info["accessRole"] == "freeBusyReader" {
                    self.free_busy(calendar, bounds)
                } else {
                    Err(err)
                }
            }
        }
    }

    fn free_busy(&self, calendar: &ExternalCalendar, slot: TimeSlot) -> Result<Vec<ExternalEvent>> {
        let url = api_url(&["freeBusy"])?;
        let body = json!({"timeMin": slot.start, "timeMax": slot.end, "items": [{"id": calendar.key.calendar}]});
        let (_, data) = self
            .json("POST", &url, &body, None)?
            .success("Read Google availability")?;
        let calendar = &data["calendars"][&calendar.key.calendar];
        ensure!(
            calendar["errors"].as_array().is_none_or(Vec::is_empty),
            "Google availability is unavailable"
        );
        calendar["busy"]
            .as_array()
            .context("Google did not return availability")?
            .iter()
            .map(|entry| {
                let start = required_str(entry, "start")?.parse()?;
                let end = required_str(entry, "end")?.parse()?;
                Ok(ExternalEvent {
                    id: format!("busy:{start}:{end}"),
                    title: String::new(),
                    location: String::new(),
                    notes: String::new(),
                    span: EventSpan::Timed {
                        slot: TimeSlot::new(start, end)?,
                    },
                    busy: true,
                })
            })
            .collect()
    }

    pub(super) fn publish(
        &self,
        publication: &Publication,
        desired: Option<TimeSlot>,
    ) -> Result<Reconcile> {
        let state = &publication.state;
        self.check_account(&state.destination)?;
        let calendar = &state.destination.calendar;
        let list = api_url(&["users", "me", "calendarList", calendar])?;
        let (_, info) = request("GET", list.as_str(), Some(&self.access), None, None)?
            .success("Check Google publishing access")?;
        ensure!(
            writable(required_str(&info, "accessRole")?),
            "Google calendar write access was revoked"
        );

        let resource = api_url(&["calendars", calendar, "events", &state.event_id])?;
        let initial = publication.reconcile(self.remote(&resource)?, desired);
        if initial != Reconcile::Write {
            return Ok(initial);
        }
        let intent = state
            .intent
            .as_ref()
            .context("The Google write was not queued")?;
        let response = match &intent.kind {
            WriteKind::Save { slot, title } => {
                let mut body = json!({
                    "start": {"dateTime": slot.start},
                    "end": {"dateTime": slot.end},
                    "extendedProperties": {"private": {
                        "fernBlockId": publication.id.to_string(),
                        "fernOperationId": intent.id.to_string()
                    }}
                });
                if let Some(etag) = &intent.expected {
                    body["start"]["date"] = Value::Null;
                    body["end"]["date"] = Value::Null;
                    self.json("PATCH", &resource, &body, Some(etag))?
                } else {
                    body["id"] = json!(state.event_id);
                    body["summary"] = json!(title);
                    body["transparency"] = json!("opaque");
                    self.json(
                        "POST",
                        &api_url(&["calendars", calendar, "events"])?,
                        &body,
                        None,
                    )?
                }
            }
            WriteKind::Delete => request(
                "DELETE",
                resource.as_str(),
                Some(&self.access),
                None,
                intent.expected.as_deref(),
            )?,
        };
        if matches!(response.status, 404 | 409 | 410 | 412) {
            // Inspect the newer version, never replace it using an unconditional retry.
            let next = publication.reconcile(self.remote(&resource)?, desired);
            if next == Reconcile::Write {
                if response.status == 409 && intent.expected.is_none() {
                    return Ok(Reconcile::Conflict(PublicationConflict::Removed));
                }
                bail!("Google Calendar changed during publication. Fern will check it again.");
            }
            return Ok(next);
        }
        let (_, body) = response.success("Publish work block")?;
        match intent.kind {
            WriteKind::Delete => Ok(Reconcile::Cancelled),
            WriteKind::Save { .. } => {
                ensure!(
                    body["id"] == state.event_id,
                    "Google returned a different event ID"
                );
                let next =
                    publication.reconcile(RemoteLookup::Event(remote_event(&body)?), desired);
                ensure!(
                    next != Reconcile::Write,
                    "Google did not confirm the publication"
                );
                Ok(next)
            }
        }
    }

    fn check_account(&self, key: &CalendarKey) -> Result<()> {
        ensure!(
            key.provider == Provider::Google && self.account == key.account,
            "Reconnect the Google account for this calendar"
        );
        Ok(())
    }

    fn remote(&self, resource: &Url) -> Result<RemoteLookup> {
        let response = request("GET", resource.as_str(), Some(&self.access), None, None)?;
        if response.status == 404 {
            return Ok(RemoteLookup::Absent);
        }
        if response.status == 410 {
            return Ok(RemoteLookup::Deleted);
        }
        let (_, value) = response.success("Read published work block")?;
        if value["status"] == "cancelled" {
            return Ok(RemoteLookup::Deleted);
        }
        remote_event(&value).map(RemoteLookup::Event)
    }

    fn json(&self, method: &str, url: &Url, body: &Value, etag: Option<&str>) -> Result<Response> {
        request(
            method,
            url.as_str(),
            Some(&self.access),
            Some(("application/json", serde_json::to_string(body)?)),
            etag,
        )
    }

    fn list(&self, url: Url, operation: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut page = url.clone();
            if let Some(token) = next.as_ref() {
                page.query_pairs_mut().append_pair("pageToken", token);
            }
            let (_, mut response) = request("GET", page.as_str(), Some(&self.access), None, None)?
                .success(operation)?;
            ensure!(
                response.is_object(),
                "Google returned an invalid calendar list"
            );
            if let Some(page) = response.get_mut("items") {
                items.append(
                    page.as_array_mut()
                        .context("Google returned an invalid calendar page")?,
                );
            }
            let previous = next;
            next = optional_str(&response["nextPageToken"]).filter(|token| !token.is_empty());
            if next.is_none() {
                return Ok(items);
            }
            ensure!(
                next != previous && items.len() < 100_000,
                "Google calendar pagination did not finish"
            );
        }
    }
}

fn event_time(time: &Value, default_zone: Option<&str>) -> Result<DateTime<Utc>> {
    let text = required_str(time, "dateTime")?;
    if let Ok(value) = DateTime::parse_from_rfc3339(text) {
        return Ok(value.to_utc());
    }
    let wall: NaiveDateTime = text.parse()?;
    wall_time(wall, time["timeZone"].as_str().or(default_zone))
        .context("Unknown Google event time zone")
}

fn imported_event(event: &Value, zone: Option<&str>) -> Result<ExternalEvent> {
    let mut busy = event["transparency"] != "transparent"
        && !event["attendees"].as_array().is_some_and(|attendees| {
            attendees.iter().any(|attendee| {
                attendee["self"] == true && attendee["responseStatus"] == "declined"
            })
        });
    let span = if let Some(start) = event["start"]["date"].as_str() {
        let start: NaiveDate = start.parse()?;
        let end = required_str(&event["end"], "date")?.parse()?;
        ensure!(start < end, "Google event has an invalid all-day range");
        EventSpan::AllDay { start, end }
    } else {
        let start = event_time(&event["start"], zone)?;
        let mut end = event_time(&event["end"], zone)?;
        if end == start {
            busy = false;
            end += chrono::TimeDelta::nanoseconds(1);
        }
        EventSpan::Timed {
            slot: TimeSlot::new(start, end)?,
        }
    };
    Ok(ExternalEvent {
        id: required_str(event, "id")?.to_owned(),
        title: optional_str(&event["summary"]).unwrap_or_default(),
        location: optional_str(&event["location"]).unwrap_or_default(),
        notes: optional_str(&event["description"]).unwrap_or_default(),
        span,
        busy,
    })
}

fn remote_event(event: &Value) -> Result<RemoteEvent> {
    let slot = if event["start"]["date"].is_string() {
        None
    } else {
        Some(TimeSlot::new(
            event_time(&event["start"], None)?,
            event_time(&event["end"], None)?,
        )?)
    };
    let properties = &event["extendedProperties"]["private"];
    Ok(RemoteEvent {
        revision: RemoteRevision {
            etag: required_str(event, "etag")?.to_owned(),
            slot,
        },
        block_id: optional_str(&properties["fernBlockId"]),
        operation_id: optional_str(&properties["fernOperationId"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn expanded_occurrence_preserves_its_id_and_adjusted_time() -> Result<()> {
        let unmodified = json!({
            "id": "series_20260308T160000Z",
            "recurringEventId": "series",
            "originalStartTime": {"dateTime": "2026-03-08T12:00:00-04:00"},
            "start": {"dateTime": "2026-03-08T13:00:00-04:00"},
            "end": {"dateTime": "2026-03-08T14:00:00-04:00"}
        });
        let event = imported_event(&unmodified, Some("America/New_York"))?;
        assert_eq!(event.id, "series_20260308T160000Z");
        assert!(event.busy);
        assert_eq!(
            event.span.busy_slot(None)?.start.to_rfc3339(),
            "2026-03-08T17:00:00+00:00"
        );

        let all_day = imported_event(
            &json!({
                "id": "holiday",
                "transparency": "transparent",
                "start": {"date": "2026-03-08"},
                "end": {"date": "2026-03-09"},
            }),
            Some("America/New_York"),
        )?;
        assert!(!all_day.busy);
        assert_eq!(all_day.span.display().start.to_string(), "2026-03-08");
        Ok(())
    }

    #[test]
    fn loopback_oauth_rejects_wrong_state_and_requires_write_consent() -> Result<()> {
        let fields =
            json!({"client_id": "test.apps.googleusercontent.com", "client_secret": "test"});
        assert!(OAuthClient::from_reader(json!({"web": &fields}).to_string().as_bytes()).is_err());
        let client = OAuthClient::from_reader(json!({"installed": fields}).to_string().as_bytes())?;
        let read = Authorization::start(client.clone(), false, None)?;
        let publish = Authorization::start(client, true, None)?;
        let scope = |flow: &Authorization| {
            flow.url
                .query_pairs()
                .find(|(name, _)| name == "scope")
                .map(|(_, value)| value.into_owned())
                .unwrap()
        };
        assert_eq!(scope(&read), format!("openid email {READ}"));
        assert_eq!(scope(&publish), format!("openid email {READ} {WRITE}"));
        assert!(
            read.url
                .query_pairs()
                .any(|(key, value)| key == "access_type" && value == "offline")
        );
        assert!(
            read.url
                .query_pairs()
                .any(|(key, value)| key == "code_challenge_method" && value == "S256")
        );
        let host = read.listener.local_addr()?;
        let nonce = read.nonce.clone();
        let browser = std::thread::spawn(move || -> Result<()> {
            for (state, expected) in [("bad", "400"), (nonce.as_str(), "200")] {
                let mut stream = TcpStream::connect(host)?;
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                write!(
                    stream,
                    "GET /?code=example&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                )?;
                let mut reply = String::new();
                stream.read_to_string(&mut reply)?;
                assert!(reply.starts_with(&format!("HTTP/1.1 {expected}")));
            }
            Ok(())
        });
        assert_eq!(read.wait_for_code()?, "example");
        browser.join().expect("loopback browser thread")?;
        Ok(())
    }
}
