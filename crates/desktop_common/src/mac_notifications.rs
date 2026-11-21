//! Actionable macOS notifications for work-block follow-ups.
use fern_core::{
    Id,
    follow_up::{FollowUpAction, FollowUpNotice},
};
use fern_i18n::Strings;

#[derive(Clone, Copy)]
pub enum Interaction {
    Open,
    Action(FollowUpAction),
}

pub struct Reply {
    pub task: Id,
    pub notice: Id,
    pub interaction: Interaction,
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn parse_native_json<T: serde::de::DeserializeOwned>(
    pointer: *mut std::ffi::c_char,
) -> anyhow::Result<T> {
    use std::ffi::{CStr, c_char};
    unsafe extern "C" {
        fn fern_calendar_free(text: *mut c_char);
    }
    anyhow::ensure!(
        !pointer.is_null(),
        "macOS returned an incomplete calendar response"
    );
    // The Objective-C bridge returns an owned, NUL-terminated allocation.
    let decoded =
        serde_json::from_slice::<serde_json::Value>(unsafe { CStr::from_ptr(pointer) }.to_bytes());
    unsafe { fern_calendar_free(pointer) };
    let mut decoded = decoded?;
    if let Some(error) = decoded["error"].as_str() {
        anyhow::bail!("{error}");
    }
    Ok(serde_json::from_value(decoded["value"].take())?)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::collections::BTreeMap;
    use std::ffi::{CString, c_char};

    use super::*;
    use serde::Deserialize;

    unsafe extern "C" {
        fn fern_follow_up_configure(
            done: *const c_char,
            snooze: *const c_char,
            minutes: *const c_char,
            reschedule: *const c_char,
        );
        fn fern_follow_up_request();
        fn fern_follow_up_permissions();
        fn fern_follow_up_permission() -> i32;
        fn fern_follow_up_actions() -> *mut c_char;
        fn fern_follow_up_show(
            task: *const c_char,
            notice: *const c_char,
            title: *const c_char,
            body: *const c_char,
        );
        fn fern_follow_up_prune(valid: *const c_char);
    }

    fn text(value: &str) -> CString {
        CString::new(value.replace('\0', "")).unwrap_or_default()
    }

    pub fn configure(strings: &Strings) {
        let (done, snooze, minutes, reschedule) = (
            text(strings.mark_done),
            text(strings.snooze),
            text(strings.work.snooze_minutes),
            text(strings.work.reschedule),
        );
        unsafe {
            fern_follow_up_configure(
                done.as_ptr(),
                snooze.as_ptr(),
                minutes.as_ptr(),
                reschedule.as_ptr(),
            )
        };
    }

    pub fn request_permission(strings: &Strings) {
        configure(strings);
        unsafe { fern_follow_up_request() };
    }

    pub fn poll_permission() {
        unsafe { fern_follow_up_permissions() };
    }

    pub fn permission() -> i32 {
        unsafe { fern_follow_up_permission() }
    }

    #[derive(Deserialize)]
    struct NativeReply {
        task: Id,
        notice: Id,
        action: String,
        minutes: String,
    }

    pub fn replies() -> anyhow::Result<Vec<Reply>> {
        let replies: Vec<NativeReply> = unsafe { parse_native_json(fern_follow_up_actions()) }?;
        Ok(replies
            .into_iter()
            .filter_map(|reply| {
                let interaction = match reply.action.as_str() {
                    "done" => Interaction::Action(FollowUpAction::Done),
                    "snooze" => {
                        let minutes: u32 = reply.minutes.trim().parse().ok()?;
                        if !(1..=fern_core::follow_up::MAX_MINUTES).contains(&minutes) {
                            return None;
                        }
                        Interaction::Action(FollowUpAction::Snooze(minutes))
                    }
                    "reschedule" => Interaction::Action(FollowUpAction::Reschedule),
                    "open" => Interaction::Open,
                    _ => return None,
                };
                Some(Reply {
                    task: reply.task,
                    notice: reply.notice,
                    interaction,
                })
            })
            .collect())
    }

    pub fn show(notice: &FollowUpNotice, body: &str) {
        let task = text(&notice.task.to_string());
        let id = text(&notice.notice.to_string());
        let title = text(&notice.title);
        let body = text(body);
        unsafe { fern_follow_up_show(task.as_ptr(), id.as_ptr(), title.as_ptr(), body.as_ptr()) };
    }

    pub fn prune(valid: &[(Id, Id)]) -> anyhow::Result<()> {
        let entries: BTreeMap<Id, Id> = valid.iter().copied().collect();
        let keys = CString::new(serde_json::to_string(&entries)?)?;
        unsafe { fern_follow_up_prune(keys.as_ptr()) };
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn configure(_: &Strings) {}
    pub fn request_permission(_: &Strings) {}
    pub fn poll_permission() {}
    pub fn permission() -> i32 {
        -2
    }
    pub fn replies() -> anyhow::Result<Vec<Reply>> {
        Ok(Vec::new())
    }
    pub fn show(_: &FollowUpNotice, _: &str) {}
    pub fn prune(_: &[(Id, Id)]) -> anyhow::Result<()> {
        Ok(())
    }
}

pub use platform::*;
