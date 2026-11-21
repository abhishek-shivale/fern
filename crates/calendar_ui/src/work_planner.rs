//! A work-block preview using the selected calendars and the existing Fern task schedule.
use std::rc::Rc;

use anyhow::{Context as _, Result, ensure};
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Utc};
use desktop_common::{
    calendar_accounts::Accounts,
    mac_notifications,
    text_input::{Changed, Mode, TextInput},
};
use fern_core::{
    Estimate, Id, Kind, Store, Task,
    calendar_sync::{self, Availability, DateRange, TimeSlot, WorkingHours, task_slot},
    follow_up::{FollowUpOptions, MAX_MINUTES},
};
use gpui::{Entity, EventEmitter, FontWeight};
use ui::prelude::*;

use crate::i18n;

pub(super) struct ClosePlanner;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReminderMode {
    Off,
    Once,
    Repeat,
}

pub(super) struct WorkPlanner {
    store: Rc<Store>,
    calendars: Entity<Accounts>,
    tasks: Vec<Task>,
    selected: Option<Id>,
    query: Entity<TextInput>,
    start: Entity<TextInput>,
    duration: Entity<TextInput>,
    deadline: Entity<TextInput>,
    hours_start: Entity<TextInput>,
    hours_end: Entity<TextInput>,
    weekends: bool,
    preview: Option<TimeSlot>,
    show_suggestions: bool,
    reminder_mode: ReminderMode,
    delay: Entity<TextInput>,
    interval: Entity<TextInput>,
    message: Option<(String, Color)>,
}

impl EventEmitter<ClosePlanner> for WorkPlanner {}

impl WorkPlanner {
    pub(super) fn new(
        store: Rc<Store>,
        calendars: Entity<Accounts>,
        selected: Option<Id>,
        date: NaiveDate,
        cx: &mut Context<Self>,
    ) -> Self {
        let hours = store.working_hours().unwrap_or_default();
        let now = Local::now().date_naive();
        let text = |placeholder, value: String, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input = TextInput::new(Mode::SingleLine, placeholder, cx);
                input.set_text(value, cx);
                input
            })
        };
        let start = text(
            "YYYY-MM-DD HH:MM",
            wall_text(date.and_time(hours.start)),
            cx,
        );
        let duration = text("30", calendar_sync::DEFAULT_BLOCK_MINUTES.to_string(), cx);
        let deadline = text(
            "YYYY-MM-DD HH:MM",
            wall_text((now + TimeDelta::days(7)).and_time(hours.end)),
            cx,
        );
        let hours_start = text("09:00", hours.start.format("%H:%M").to_string(), cx);
        let hours_end = text("17:00", hours.end.format("%H:%M").to_string(), cx);
        let query = text(i18n::t(cx).work.pick_task, String::new(), cx);
        let delay = text("15", "15".into(), cx);
        let interval = text("30", "30".into(), cx);

        for input in [&start, &duration] {
            cx.subscribe(input, |this: &mut Self, _, _: &Changed, cx| {
                this.preview = None;
                this.show_suggestions = false;
                this.message = None;
                this.check_preview(cx);
                cx.notify();
            })
            .detach();
        }
        for input in [&deadline, &hours_start, &hours_end] {
            cx.subscribe(input, |this: &mut Self, _, _: &Changed, cx| {
                this.show_suggestions = false;
                cx.notify();
            })
            .detach();
        }
        cx.subscribe(&query, |_, _, _: &Changed, cx| cx.notify())
            .detach();
        cx.observe(&calendars, |_, _, cx| cx.notify()).detach();
        cx.observe_global::<desktop_common::data_version::DataVersion>(|this: &mut Self, cx| {
            this.load_tasks();
            cx.notify();
        })
        .detach();
        let mut view = Self {
            store,
            calendars,
            tasks: Vec::new(),
            selected: None,
            query,
            start,
            duration,
            deadline,
            hours_start,
            hours_end,
            weekends: hours.weekends,
            preview: None,
            show_suggestions: false,
            reminder_mode: ReminderMode::Off,
            delay,
            interval,
            message: None,
        };
        view.load_tasks();
        if let Some(id) = selected {
            view.pick_task(id, cx);
        }
        view
    }

    fn load_tasks(&mut self) {
        match self.store.tasks() {
            Ok(tasks) => {
                self.tasks = tasks
                    .into_iter()
                    .filter(|t| t.kind == Kind::Task && !t.is_closed() && !t.is_deleted())
                    .collect()
            }
            Err(error) => self.message = Some((format!("{error:#}"), Color::Error)),
        }
        if self
            .selected
            .is_some_and(|id| self.tasks.iter().all(|task| task.id != id))
        {
            self.selected = None;
            self.preview = None;
            self.show_suggestions = false;
        }
    }

    fn pick_task(&mut self, id: Id, cx: &mut Context<Self>) {
        let Some(task) = self.tasks.iter().find(|t| t.id == id) else {
            return;
        };
        let slot = task_slot(task).ok().flatten();
        let due = task
            .due_date
            .filter(|date| *date >= Local::now().date_naive());
        let estimate = match task.estimate {
            Some(Estimate::Minutes(minutes)) => Some(i64::from(minutes)),
            Some(Estimate::Pomos(count)) => Some(i64::from(count) * 25),
            None => None,
        };
        self.selected = Some(id);
        self.preview = slot;
        self.show_suggestions = false;
        self.message = None;
        let duration = slot
            .map(|slot| (slot.end - slot.start).num_minutes())
            .or(estimate)
            .unwrap_or(calendar_sync::DEFAULT_BLOCK_MINUTES)
            .clamp(1, 1440);
        self.duration
            .update(cx, |input, cx| input.set_text(duration.to_string(), cx));
        let hours = self.store.working_hours().unwrap_or_default();
        if let Some(slot) = slot {
            let start = slot.start.with_timezone(&Local).naive_local();
            self.start
                .update(cx, |input, cx| input.set_text(wall_text(start), cx));
        } else if let Some(day) = due {
            self.start.update(cx, |input, cx| {
                input.set_text(wall_text(day.and_time(hours.start)), cx)
            });
        }
        let deadline = if slot.is_none() { due } else { None }
            .unwrap_or(Local::now().date_naive() + TimeDelta::days(7));
        self.deadline.update(cx, |input, cx| {
            input.set_text(wall_text(deadline.and_time(hours.end)), cx)
        });
        self.query.update(cx, |input, cx| input.set_text("", cx));
        self.reminder_mode = ReminderMode::Off;
        let follow_up = self.store.follow_up_options(id);
        match follow_up {
            Ok(options) => {
                let delay = options.map_or(15, |settings| settings.delay_minutes);
                let interval = options.and_then(|settings| settings.interval_minutes);
                self.reminder_mode = match (options.is_some(), interval.is_some()) {
                    (false, _) => ReminderMode::Off,
                    (true, false) => ReminderMode::Once,
                    (true, true) => ReminderMode::Repeat,
                };
                self.delay
                    .update(cx, |input, cx| input.set_text(delay.to_string(), cx));
                self.interval.update(cx, |input, cx| {
                    input.set_text(interval.unwrap_or(30).to_string(), cx)
                });
            }
            Err(error) => self.message = Some((format!("{error:#}"), Color::Error)),
        }
        self.check_preview(cx);
        cx.notify();
    }

    fn duration(&self, cx: &App) -> Result<TimeDelta> {
        Ok(TimeDelta::minutes(i64::from(minutes(
            &self.duration,
            1440,
            cx,
        )?)))
    }

    fn candidate(&self, cx: &App) -> Result<TimeSlot> {
        if let Some(slot) = self.preview {
            return Ok(slot);
        }
        let start = parse_local(self.start.read(cx).text())?;
        let end = start
            .checked_add_signed(self.duration(cx)?)
            .context("The work block ends outside the calendar")?;
        TimeSlot::new(start, end)
    }

    fn check_preview(&self, cx: &mut Context<Self>) {
        if let Ok(slot) = self.candidate(cx) {
            self.calendars
                .update(cx, |service, _| service.check_dates(slot.local_days()));
        }
    }

    fn availability(&self, task: Id, slot: TimeSlot, cx: &App) -> Availability {
        let calendar = self.calendars.read(cx).calendar_availability(task, slot);
        let locals = self
            .tasks
            .iter()
            .filter(|other| other.id != task)
            .filter_map(|task| task_slot(task).ok().flatten())
            .filter(|other| other.overlaps(slot));
        let mut result = calendar;
        result.busy.extend(locals);
        result
    }

    fn suggestion_inputs(&self, cx: &App) -> Result<(TimeSlot, WorkingHours, TimeDelta)> {
        let start = calendar_sync::minute_ceil(Utc::now());
        let limit = DateRange::planning(Local::now().date_naive())
            .utc_bounds(None)?
            .end;
        let end = parse_local(self.deadline.read(cx).text())?.min(limit);
        let hours = WorkingHours {
            start: NaiveTime::parse_from_str(self.hours_start.read(cx).text().trim(), "%H:%M")?,
            end: NaiveTime::parse_from_str(self.hours_end.read(cx).text().trim(), "%H:%M")?,
            weekends: self.weekends,
        };
        ensure!(
            hours.start < hours.end,
            "Working hours must end after they start"
        );
        Ok((
            TimeSlot::new(start, end).context("Choose a deadline after the current time")?,
            hours,
            self.duration(cx)?,
        ))
    }

    fn request_suggestions(&mut self, cx: &mut Context<Self>) {
        self.show_suggestions = false;
        self.message = None;
        let result = self
            .suggestion_inputs(cx)
            .and_then(|(_, hours, _)| self.store.save_working_hours(hours));
        match result {
            Ok(()) => self.show_suggestions = true,
            Err(error) => self.message = Some((format!("{error:#}"), Color::Error)),
        }
        cx.notify();
    }

    fn preview_suggestion(&mut self, slot: TimeSlot, cx: &mut Context<Self>) {
        self.preview = Some(slot);
        self.start.update(cx, |input, cx| {
            input.set_text(
                wall_text(slot.start.with_timezone(&Local).naive_local()),
                cx,
            )
        });
        self.check_preview(cx);
        self.message = None;
        cx.notify();
    }

    fn save(&mut self, publish: bool, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            let task = self.selected.context("Choose an open task")?;
            let slot = self.candidate(cx)?;
            let destination = self.calendars.read(cx).destination.as_ref();
            let write = if publish {
                Some(destination.context("Choose a publishing calendar in Integrations")?)
            } else {
                None
            };
            self.store.schedule_work(task, slot, write)
        })();
        let strings = &i18n::t(cx).work;
        match result {
            Ok(()) => {
                self.message = Some((
                    (if publish {
                        strings.queued
                    } else {
                        strings.scheduled
                    })
                    .into(),
                    Color::Accent,
                ));
                self.load_tasks();
                desktop_common::data_version::data_changed(cx.entity_id(), cx);
                if publish {
                    self.calendars
                        .update(cx, |service, cx| service.refresh_now(cx));
                }
            }
            Err(error) => self.message = Some((format!("{error:#}"), Color::Error)),
        }
        cx.notify();
    }

    fn save_reminder(&mut self, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            let task = self.selected.context("Choose an open task")?;
            let options = if self.reminder_mode == ReminderMode::Off {
                None
            } else {
                Some(FollowUpOptions {
                    delay_minutes: minutes(&self.delay, MAX_MINUTES, cx)?,
                    interval_minutes: (self.reminder_mode == ReminderMode::Repeat)
                        .then(|| minutes(&self.interval, MAX_MINUTES, cx))
                        .transpose()?,
                })
            };
            self.store.set_follow_up(task, options)?;
            if options.is_some() && mac_notifications::permission() == 0 {
                mac_notifications::request_permission(i18n::t(cx));
            }
            Ok(())
        })();
        self.message = Some(match result {
            Ok(()) => (i18n::t(cx).work.follow_saved.to_owned(), Color::Accent),
            Err(error) => (format!("{error:#}"), Color::Error),
        });
        desktop_common::data_version::data_changed(cx.entity_id(), cx);
        cx.notify();
    }

    fn input(&self, input: &Entity<TextInput>, cx: &App) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .min_w_0()
            .h(px(29.))
            .px_2()
            .rounded(px(5.))
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().background)
            .child(input.clone())
    }

    fn field(&self, label: &'static str, input: &Entity<TextInput>, cx: &App) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(self.input(input, cx))
    }

    fn render_reminder(&self, cx: &Context<Self>) -> impl IntoElement {
        let strings = &i18n::t(cx).work;
        v_flex()
            .gap_2()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(strings.follow_up).weight(FontWeight::MEDIUM))
            .child(
                h_flex().gap_1().children(
                    [
                        ("follow-off", strings.follow_off, ReminderMode::Off),
                        ("follow-once", strings.follow_once, ReminderMode::Once),
                        ("follow-repeat", strings.follow_repeat, ReminderMode::Repeat),
                    ]
                    .map(|(id, title, mode)| {
                        work_button(id, title, self.reminder_mode == mode, cx).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.reminder_mode = mode;
                                cx.notify();
                            }),
                        )
                    }),
                ),
            )
            .when(self.reminder_mode != ReminderMode::Off, |el| {
                el.child(self.field(strings.follow_delay, &self.delay, cx))
                    .when(self.reminder_mode == ReminderMode::Repeat, |el| {
                        el.child(self.field(strings.follow_interval, &self.interval, cx))
                    })
                    .when(mac_notifications::permission() < 0, |el| {
                        el.child(
                            Label::new(strings.follow_permission)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(mac_notifications::permission() == 0, |el| {
                        el.child(
                            work_button("allow-follow-up", strings.follow_prompt, false, cx)
                                .on_click(cx.listener(|_, _, _, cx| {
                                    mac_notifications::request_permission(i18n::t(cx))
                                })),
                        )
                    })
            })
            .child(
                work_button("save-follow-up", strings.follow_save, false, cx)
                    .on_click(cx.listener(|this, _, _, cx| this.save_reminder(cx))),
            )
    }

    fn render_plan(&self, task: Id, cx: &Context<Self>) -> impl IntoElement {
        let strings = i18n::t(cx);
        let text = &strings.work;
        let candidate = self.candidate(cx);
        let slots = self
            .show_suggestions
            .then(|| self.suggestion_inputs(cx).ok())
            .flatten()
            .map(|(search, hours, duration)| {
                let available = self.availability(task, search, cx);
                (!available.stale)
                    .then(|| calendar_sync::suggest_slots(search, hours, duration, &available.busy))
            });
        let linked = self
            .calendars
            .read(cx)
            .publications
            .iter()
            .any(|p| p.publication.task_id == Some(task));
        let publish = !linked && self.calendars.read(cx).destination.is_some();
        v_flex()
            .gap_2()
            .child(self.field(text.start, &self.start, cx))
            .child(self.field(text.duration, &self.duration, cx))
            .when_some(candidate.as_ref().ok().copied(), |el, slot| {
                let result = self.availability(task, slot, cx);
                el.child(Label::new(format_slot(slot)).size(LabelSize::Small))
                    .when(result.stale, |el| {
                        el.child(
                            Label::new(text.unknown)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .when(!result.busy.is_empty(), |el| {
                        el.child(
                            Label::new(
                                text.overlaps
                                    .replace("{count}", &result.busy.len().to_string()),
                            )
                            .size(LabelSize::Small)
                            .color(Color::Error),
                        )
                    })
                    .when(!result.stale && result.busy.is_empty(), |el| {
                        el.child(
                            Label::new(text.free)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(
                                work_button("save-work-block", strings.save, true, cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                            )
                            .when(publish, |el| {
                                el.child(
                                    work_button("publish-work-block", text.publish, false, cx)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.save(true, cx)),
                                        ),
                                )
                            }),
                    )
            })
            .children(candidate.err().map(|error| {
                Label::new(format!("{error:#}"))
                    .size(LabelSize::Small)
                    .color(Color::Error)
            }))
            .child(
                v_flex()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.field(text.deadline, &self.deadline, cx))
                    .child(
                        Label::new(text.working_hours)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().child(self.input(&self.hours_start, cx)))
                            .child(div().flex_1().child(self.input(&self.hours_end, cx))),
                    )
                    .child(
                        work_button("work-on-weekends", text.weekends, false, cx)
                            .child(Switch::new().toggle_state(self.weekends))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.weekends = !this.weekends;
                                this.show_suggestions = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Label::new(text.suggestion_window)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        work_button("suggest-work-slots", text.suggest, false, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.request_suggestions(cx))),
                    )
                    .children(slots.map(|slots| {
                        let Some(slots) = slots else {
                            return Label::new(text.unknown)
                                .size(LabelSize::Small)
                                .color(Color::Error)
                                .into_any_element();
                        };
                        if slots.is_empty() {
                            return Label::new(text.no_slots)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .into_any_element();
                        }
                        v_flex()
                            .gap_1()
                            .children(slots.into_iter().enumerate().map(|(index, slot)| {
                                work_button(
                                    ("suggested-slot", index),
                                    format_slot(slot),
                                    self.preview == Some(slot),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.preview_suggestion(slot, cx),
                                ))
                            }))
                            .into_any_element()
                    })),
            )
            .child(self.render_reminder(cx))
    }
}

impl Render for WorkPlanner {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = &i18n::t(cx).work;
        let theme = cx.theme().colors().clone();
        let query = self.query.read(cx).text().trim().to_lowercase();
        let selected = self
            .selected
            .and_then(|id| self.tasks.iter().find(|task| task.id == id));
        v_flex()
            .w(px(325.))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(theme.border)
            .pt(crate::title_bar::header_inset())
            .bg(theme.surface_background)
            .child(
                h_flex()
                    .h(px(49.))
                    .px_3()
                    .justify_between()
                    .flex_none()
                    .child(Label::new(strings.plan).weight(FontWeight::MEDIUM))
                    .child(
                        IconButton::new("close-work-planner", IconName::X)
                            .tip(i18n::t(cx).close, ui::TipSide::Below)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ClosePlanner))),
                    ),
            )
            .child(
                v_flex()
                    .id("work-plan-scroll")
                    .overflow_y_scroll()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .p_3()
                    .child(self.input(&self.query, cx))
                    .when(selected.is_none() || !query.is_empty(), |el| {
                        el.children(
                            self.tasks
                                .iter()
                                .filter(|task| task.title.to_lowercase().contains(&query))
                                .take(10)
                                .map(|task| {
                                    let id = task.id;
                                    work_button(
                                        SharedString::from(format!("plan-task-{id}")),
                                        task.title.clone(),
                                        self.selected == Some(id),
                                        cx,
                                    )
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.pick_task(id, cx)),
                                    )
                                }),
                        )
                    })
                    .children(
                        selected
                            .map(|task| Label::new(task.title.clone()).weight(FontWeight::MEDIUM)),
                    )
                    .children(self.message.as_ref().map(|(message, color)| {
                        Label::new(message.clone())
                            .size(LabelSize::Small)
                            .color(*color)
                    }))
                    .children(selected.map(|task| self.render_plan(task.id, cx))),
            )
    }
}

fn minutes(input: &Entity<TextInput>, max: u32, cx: &App) -> Result<u32> {
    let minutes: u32 = input
        .read(cx)
        .text()
        .trim()
        .parse()
        .context("Enter a duration in minutes")?;
    ensure!(
        (1..=max).contains(&minutes),
        "Minutes must be between 1 and {max}"
    );
    Ok(minutes)
}

fn parse_local(value: &str) -> Result<chrono::DateTime<Utc>> {
    let wall = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M")
        .context("Use YYYY-MM-DD HH:MM for the local date and time")?;
    calendar_sync::wall_time(wall, None).context("That local time does not exist")
}

fn wall_text(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%d %H:%M").to_string()
}

pub(super) fn format_slot(slot: TimeSlot) -> String {
    let from = slot.start.with_timezone(&Local).naive_local();
    let to = slot.end.with_timezone(&Local).naive_local();
    let end = if from.date() == to.date() {
        to.format("%H:%M").to_string()
    } else {
        wall_text(to)
    };
    format!("{} – {end}", wall_text(from))
}

pub(super) fn work_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<ui::HorizontalStack> {
    let colors = cx.theme().colors();
    h_flex()
        .id(id)
        .tab_index(0)
        .min_h(px(28.))
        .px_2()
        .gap_1()
        .rounded(px(5.))
        .border_1()
        .border_color(colors.border)
        .cursor_pointer()
        .when(selected, |el| {
            el.bg(colors.element_selected).border_color(colors.accent)
        })
        .hover(|el| el.bg(colors.element_hover))
        .focus_visible(|el| el.border_color(colors.accent))
        .child(Label::new(label).size(LabelSize::Small).truncate())
}
