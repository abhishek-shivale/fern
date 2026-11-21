# macOS calendar sync handoff

Continue from Fern commit `1e48336` or later on `abhishek-shivale/fern` `main`. The code includes direct Google Desktop OAuth, iCloud reads through EventKit, local calendar caching, Google publication and conflict resolution, work-block suggestions, and follow-up reminders. Run the acceptance checks below on a Mac, fix any failures, and record which checks pass and which need account access or other input.

## Set up an isolated macOS test

1. Build the packaged app with `script/package aarch64-apple-darwin` on Apple Silicon or `script/package x86_64-apple-darwin` on Intel. The script writes a DMG under `dist/`. Use its `Fern.app` for Calendar and notification permissions.
2. Quit any running Fern instance. Create a temporary profile with `fern_test_data=$(mktemp -d)` and launch the packaged binary with `FERN_DATA_DIR="$fern_test_data" /path/to/Fern.app/Contents/MacOS/fern`. Reuse that directory for restart tests. Use dedicated test calendars and tasks.
3. In **Settings → Integrations**, select **Connect Google** if the build has a Desktop OAuth client bundled. Otherwise, import the downloaded Desktop app client JSON using **Choose Google OAuth client file**, then connect. Enable the Google Calendar API in the Google project and add the test account if the project is in Testing mode. For a release build, confirm the repository has `FERN_GOOGLE_OAUTH_CLIENT_JSON` configured. Keep OAuth credentials out of the repository and session transcript. [Build configuration](../README.md)
4. Connect iCloud in Integrations and grant macOS Calendar access. Enable the same Google account in Apple Calendar, make its dedicated test calendar visible there, and allow Fern notifications for the follow-up tests.

## First-release acceptance checks

- [ ] **Import and availability.** Add busy and free events to both a Google and an iCloud test calendar. Independently toggle **Show events** and **Count as busy**. Confirm busy events from both providers affect the proposed block, free events leave slots available, and Google events appear only once with that account also enabled in Apple Calendar.
- [ ] **Publish, move, and remove.** Authorize Google publishing separately and choose the dedicated writable test calendar. Schedule and publish a task. Confirm exactly one event is visible in Google Calendar and Apple Calendar. Move the block in Fern and confirm its Google event ID stays the same. Remove a test block and confirm only its linked event is removed. Complete a separate published task and confirm its historical block remains.
- [ ] **Retry after interruption.** Interrupt a test publish after Google accepts the create but before Fern persists the result. With a symbol-enabled test build, a debugger breakpoint before `Store::finish_publication` records success can make this repeatable. Restart with the same temporary profile and confirm the pending operation resolves to the existing Google event ID with exactly one event on the calendar. [Publication completion](../crates/core/src/calendar_sync/publication.rs)
- [ ] **External edits and deletion.** Move a published event directly in Google Calendar. On separate test blocks, verify **Accept external time** and **Restore Fern's time**. Delete another event externally and confirm Fern waits for the user's choice. Exercise both unlinking and explicit recreation using test blocks.
- [ ] **Recurring events, dates, and offline behavior.** Check a moved recurring instance, a canceled instance, an all-day event, and a timed event across a time-zone change. Revoke provider access or go offline. Confirm cached events remain visible, availability becomes stale or unknown, and Fern opens with a usable task list. Verify the last successful refresh and any pending publication errors are visible.

## Follow-on acceptance checks

- [ ] **Slot suggestions.** Set working hours, duration, and a deadline. Confirm suggested times avoid busy events from both calendar providers and fit the deadline. Preview a slot before publishing it.
- [ ] **Follow-up notifications.** Test a one-time follow-up and a repeating one. Confirm an ignored nudge repeats, Snooze delays the next one by the chosen interval, Done stops delivery, and Reschedule resets timing after the block is saved. Check that task deletion and disabling reminders cancel delivery, and that a wake or restart produces at most one overdue nudge per open task before resuming the interval.

## Verification to finish

Linux workspace binary checks and Clippy passed. Focused OAuth, calendar reconciliation, availability, suggestions, and follow-up tests passed. The full Linux test run was interrupted, and the release build and live macOS checks remain unverified.

Run the locked checks on the Mac after any fixes.

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo check --workspace --bins --locked
cargo test --workspace --locked
cargo build --profile release-fast -p fern_desktop -p fern_server --locked
```

Fern currently refreshes bounded date ranges into local SQLite. Confirm whether the spec requires persisted Google incremental sync cursors or whether these local snapshots satisfy the intended first-release behavior. [Google event refresh](../crates/desktop_common/src/calendar_accounts/google.rs)
