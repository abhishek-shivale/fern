# macOS calendar sync handoff

Continue from Fern commit `1e48336` or later on `abhishek-shivale/fern` `main`. The code includes direct Google Desktop OAuth, iCloud reads through EventKit, local calendar caching, Google publication and conflict resolution, work-block suggestions, and follow-up reminders. Run the acceptance checks below on a Mac, fix any failures, and record which checks pass and which need account access or other input.

## Set up an isolated macOS test

1. Run `script/package aarch64-apple-darwin` on Apple Silicon or `script/package x86_64-apple-darwin` on Intel. The script writes a DMG under `dist/`. Use its `Fern.app` for Calendar and notification permissions. macOS builds use GPUI's runtime shader feature by default. [GPUI shader build](https://github.com/zed-industries/zed/blob/56934ec0560bce8929025497411de7f5322fb6ca/crates/gpui_apple/build.rs)
2. Quit any running Fern instance. Mount the DMG and copy its `Fern.app` into a fresh writable directory beside a `.portable` marker. Release builds use portable mode for isolation. Run the following from the repository root, replacing the source path with the mounted app's path.

   ```bash
   fern_test_dir=$(mktemp -d "$PWD/target/fern-calendar-test.XXXXXX") &&
     ditto "/path/to/mounted/Fern.app" "$fern_test_dir/Fern.app" &&
     touch "$fern_test_dir/.portable" &&
     "$fern_test_dir/Fern.app/Contents/MacOS/fern"
   ```

   In **Settings → Data & Sync**, verify that the data directory is `$fern_test_dir/Data` before connecting accounts. Reuse that app copy and directory for restart tests. Use dedicated test calendars and tasks. [Portable mode](../README.md)
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

Linux workspace binary checks and Clippy passed before this handoff, along with focused OAuth, calendar reconciliation, availability, suggestions, and follow-up tests. The full Linux test run was interrupted.

On Apple Silicon, all 224 `fern_core` tests passed, including calendar reconciliation, cached availability, slot suggestions, and follow-up scheduling. The test binary was compiled directly with `rustc` using the dependencies from the partial Cargo build. The native calendar bridge also compiled and linked against the macOS SDK. An ad hoc signed smoke bundle using the package's `Info.plist` and entitlements resolved the correct bundle identifier and used the adjacent `Data/` profile on two launches.

The original macOS Cargo build stopped because the active Command Line Tools lack `metal`. The desktop manifest now enables runtime shaders on macOS. Full workspace checks and the release build are pending. In the sandbox, a packaging smoke test also stopped at `iconutil` with `Invalid Iconset`, and a standalone `hdiutil create` probe returned `Device not configured`. DMG creation needs verification from a regular macOS session.

All live acceptance checks above remain open. Import, publication, retry, conflict resolution, recurring events, and cross-provider suggestions need the packaged app and dedicated Google and iCloud calendars. Follow-ups need the packaged app and macOS notification permission.

The Google recurring-event parser test passed on macOS. The OAuth loopback test timed out on one run and passed on a diagnostic rerun. Live sign-in remains unverified.

Run the locked checks on the Mac after any fixes.

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo check --workspace --bins --locked
cargo test --workspace --locked
cargo build --profile release-fast -p fern_desktop -p fern_server --locked
```

The checked-in first-release checks cover bounded refresh and offline availability. Use the SQLite snapshots to verify that behavior. Confirm persisted Google incremental sync cursors separately if another release spec requires them. [Google event refresh](../crates/desktop_common/src/calendar_accounts/google.rs)
