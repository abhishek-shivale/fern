use anyhow::Result;
use fern_core::calendar_sync::{CalendarKey, DateRange, ExternalCalendar, ExternalEvent};
use futures::channel::oneshot;

use super::google::Credentials;

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{CString, c_char};
    use std::sync::Mutex;

    use anyhow::{Context as _, ensure};
    use fern_core::calendar_sync::Provider;
    use serde::Deserialize;

    use super::*;
    use crate::mac_notifications::parse_native_json;

    unsafe extern "C" {
        fn fern_calendar_access(completion: extern "C" fn(i32));
        fn fern_calendar_monitor_network();
        fn fern_calendar_connection() -> i32;
        fn fern_icloud_calendar_access() -> i32;
        fn fern_icloud_calendars() -> *mut c_char;
        fn fern_icloud_events(
            source: *const c_char,
            identifier: *const c_char,
            start: f64,
            end: f64,
        ) -> *mut c_char;
        fn fern_google_keychain_get(profile: *const c_char) -> *mut c_char;
        fn fern_google_keychain_set(profile: *const c_char, data: *const c_char) -> *mut c_char;
    }

    static ACCESS: Mutex<Option<oneshot::Sender<i32>>> = Mutex::new(None);

    pub(in crate::calendar_accounts) fn start_network_monitor() {
        unsafe { fern_calendar_monitor_network() };
    }

    pub(in crate::calendar_accounts) fn connection() -> (bool, bool) {
        unsafe {
            (
                fern_calendar_connection() == 1,
                fern_icloud_calendar_access() == 1,
            )
        }
    }

    extern "C" fn finish_access(result: i32) {
        if let Ok(mut waiting) = ACCESS.lock()
            && let Some(sender) = waiting.take()
        {
            let _ = sender.send(result);
        }
    }

    pub(in crate::calendar_accounts) fn request_access() -> Result<oneshot::Receiver<i32>> {
        let (send, receive) = oneshot::channel();
        {
            let mut access = ACCESS
                .lock()
                .map_err(|_| anyhow::anyhow!("Could not request Calendar access"))?;
            ensure!(
                access.is_none(),
                "macOS Calendar access is already being requested"
            );
            *access = Some(send);
        }
        unsafe { fern_calendar_access(finish_access) };
        Ok(receive)
    }

    #[derive(Deserialize)]
    struct Calendar {
        source: String,
        source_name: String,
        id: String,
        name: String,
    }

    pub(in crate::calendar_accounts) fn calendars() -> Result<Vec<ExternalCalendar>> {
        let calendars: Vec<Calendar> = unsafe { parse_native_json(fern_icloud_calendars()) }?;
        Ok(calendars
            .into_iter()
            .map(|calendar| ExternalCalendar {
                key: CalendarKey {
                    provider: Provider::ICloud,
                    account: calendar.source,
                    calendar: calendar.id,
                },
                name: calendar.name,
                account_name: calendar.source_name,
                timezone: None,
                writable: false,
                present: true,
                visible: false,
                busy: false,
            })
            .collect())
    }

    pub(in crate::calendar_accounts) fn events(
        key: &CalendarKey,
        range: DateRange,
    ) -> Result<Vec<ExternalEvent>> {
        let source = CString::new(key.account.as_str())?;
        let calendar = CString::new(key.calendar.as_str())?;
        let slot = range.utc_bounds(None)?;
        let events = unsafe {
            parse_native_json(fern_icloud_events(
                source.as_ptr(),
                calendar.as_ptr(),
                slot.start.timestamp() as f64,
                slot.end.timestamp() as f64,
            ))
        }?;
        let events: Vec<ExternalEvent> = events;
        for event in &events {
            event
                .span
                .busy_slot(None)
                .context("EventKit returned an invalid event range")?;
        }
        Ok(events)
    }

    pub(in crate::calendar_accounts) fn credentials(profile: &str) -> Result<Credentials> {
        let profile = CString::new(profile)?;
        let stored: Option<String> =
            unsafe { parse_native_json(fern_google_keychain_get(profile.as_ptr())) }?;
        stored.map_or_else(
            || Ok(Credentials::default()),
            |data| Ok(serde_json::from_str(&data)?),
        )
    }

    pub(in crate::calendar_accounts) fn save_credentials(
        profile: &str,
        credentials: &Credentials,
    ) -> Result<()> {
        let profile = CString::new(profile)?;
        let data = CString::new(serde_json::to_string(credentials)?)?;
        unsafe { parse_native_json(fern_google_keychain_set(profile.as_ptr(), data.as_ptr())) }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;
    use anyhow::bail;

    pub(in crate::calendar_accounts) fn start_network_monitor() {}
    pub(in crate::calendar_accounts) fn connection() -> (bool, bool) {
        (false, false)
    }

    pub(in crate::calendar_accounts) fn request_access() -> Result<oneshot::Receiver<i32>> {
        bail!("EventKit requires macOS")
    }
    pub(in crate::calendar_accounts) fn calendars() -> Result<Vec<ExternalCalendar>> {
        bail!("EventKit requires macOS")
    }
    pub(in crate::calendar_accounts) fn events(
        _: &CalendarKey,
        _: DateRange,
    ) -> Result<Vec<ExternalEvent>> {
        bail!("EventKit requires macOS")
    }
    pub(in crate::calendar_accounts) fn credentials(_: &str) -> Result<Credentials> {
        bail!("Keychain requires macOS")
    }
    pub(in crate::calendar_accounts) fn save_credentials(_: &str, _: &Credentials) -> Result<()> {
        bail!("Keychain requires macOS")
    }
}
pub(super) use platform::*;
