//! Google and iCloud setup, calendar selection, and publication conflicts.
use chrono::Utc;
use desktop_common::calendar_accounts::{Accounts, PublicationStatus, accounts, last_refresh};
use fern_core::calendar_sync::{DateRange, Provider, PublicationConflict, Resolution};
use gpui::{Entity, FontWeight, PathPromptOptions};
use ui::prelude::*;

use crate::i18n;
use crate::work_planner::{format_slot, work_button};

pub fn settings_panel(cx: &mut App) -> Option<Entity<CalendarSettings>> {
    let service = accounts(cx)?;
    Some(cx.new(|cx| {
        cx.observe(&service, |_, _, cx| cx.notify()).detach();
        CalendarSettings { service }
    }))
}

pub struct CalendarSettings {
    service: Entity<Accounts>,
}

impl CalendarSettings {
    fn choose_client(&mut self, cx: &mut Context<Self>) {
        let choose = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Some(path) = choose
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|paths| paths.into_iter().next())
            {
                this.update(cx, |this, cx| {
                    this.service
                        .update(cx, |service, cx| service.configure_google(path, cx));
                })
                .ok();
            }
        })
        .detach();
    }

    fn conflict_actions(
        &self,
        status: &PublicationStatus,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let strings = &i18n::t(cx).work;
        let id = status.publication.id;
        let conflict = status.publication.state.conflict.as_ref()?;
        let (message, remote, restore) = match conflict {
            PublicationConflict::Changed(revision) => (
                strings.external_change,
                revision.slot,
                Some(if status.local_slot.is_some() {
                    strings.restore_time
                } else {
                    strings.cancel_publication
                }),
            ),
            PublicationConflict::Removed => (
                strings.external_delete,
                None,
                status.local_slot.map(|_| strings.recreate),
            ),
            PublicationConflict::OtherEvent => (
                strings.external_replace,
                None,
                status.local_slot.map(|_| strings.recreate),
            ),
        };
        let button = |name: &str, label: &'static str, action| {
            work_button(
                SharedString::from(format!("publication-{id}-{name}")),
                label,
                false,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.service
                    .update(cx, |service, cx| service.resolve(id, action, cx));
            }))
        };
        Some(
            v_flex()
                .gap_1()
                .child(Label::new(message).color(Color::Error))
                .children(status.local_slot.map(|slot| {
                    Label::new(format!("Fern · {}", format_slot(slot)))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                }))
                .children(remote.map(|slot| {
                    Label::new(format!("Google · {}", format_slot(slot)))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                }))
                .child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .when(remote.is_some() && status.local_slot.is_some(), |el| {
                            el.child(button(
                                "accept",
                                strings.accept_time,
                                Resolution::AcceptTime,
                            ))
                        })
                        .children(
                            restore.map(|label| button("restore", label, Resolution::RestoreFern)),
                        )
                        .child(button("unlink", strings.unlink, Resolution::Unlink)),
                )
                .into_any_element(),
        )
    }
}

impl Render for CalendarSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strings = &i18n::t(cx).work;
        let colors = cx.theme().colors().clone();
        let service = self.service.read(cx);
        let running = service.account_action_running;
        let google_status = service.google.account.as_deref();
        let today = chrono::Local::now().date_naive();
        let now = Utc::now().timestamp_millis();
        let planning = DateRange::planning(today);
        let pending = service
            .publications
            .iter()
            .filter(|status| status.pending)
            .count();
        let provider_errors = Provider::ALL.into_iter().filter_map(|provider| {
            service
                .provider_error(provider)
                .map(|err| format!("{} · {err}", provider.label()))
        });
        let calendar_rows: Vec<_> = service
            .calendars
            .iter()
            .enumerate()
            .map(|(index, cached)| {
                let calendar = &cached.calendar;
                let visible = calendar.visible;
                let busy = calendar.busy;
                let destination = service.destination.as_ref() == Some(&calendar.key);
                let key_visible = calendar.key.clone();
                let key_busy = calendar.key.clone();
                let key_dest = calendar.key.clone();
                let stale = (visible || busy) && service.calendar_stale(cached, planning, now);
                let toggle = |id, label, state| {
                    work_button(
                        SharedString::from(format!("calendar-selection-{index}-{id}")),
                        label,
                        false,
                        cx,
                    )
                    .child(Switch::new().toggle_state(state))
                };
                v_flex()
                    .py_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .child(Label::new(calendar.name.clone()).weight(FontWeight::MEDIUM))
                    .child(
                        Label::new(format!(
                            "{} · {}",
                            calendar.key.provider.label(),
                            calendar.account_name
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(toggle("show", strings.show, visible).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.service.update(cx, |service, cx| {
                                        service.set_selection(&key_visible, !visible, busy, cx)
                                    });
                                },
                            )))
                            .child(toggle("busy", strings.busy, busy).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.service.update(cx, |service, cx| {
                                        service.set_selection(&key_busy, visible, !busy, cx)
                                    });
                                },
                            )))
                            .when(
                                destination || (calendar.writable && calendar.present),
                                |el| {
                                    el.child(
                                        work_button(
                                            SharedString::from(format!(
                                                "calendar-destination-{index}"
                                            )),
                                            if destination {
                                                strings.destination
                                            } else {
                                                strings.use_destination
                                            },
                                            destination,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.service.update(cx, |service, cx| {
                                                    service.set_destination(
                                                        (!destination).then_some(&key_dest),
                                                        cx,
                                                    )
                                                });
                                            }),
                                        ),
                                    )
                                },
                            ),
                    )
                    .children(last_refresh(cached).map(|time| {
                        Label::new(
                            strings
                                .last_refresh
                                .replace("{time}", &time.format("%Y-%m-%d %H:%M").to_string()),
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                    }))
                    .when(stale, |el| {
                        el.child(
                            Label::new(strings.stale)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .children(
                        cached
                            .windows
                            .values()
                            .find_map(|window| window.error.as_ref())
                            .map(|error| {
                                Label::new(error.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Error)
                            }),
                    )
                    .into_any_element()
            })
            .collect();
        let publications: Vec<_> = service
            .publications
            .iter()
            .map(|status| {
                let state = &status.publication.state;
                let calendar = service
                    .calendars
                    .iter()
                    .find(|cached| cached.calendar.key == state.destination)
                    .map_or(state.destination.calendar.as_str(), |cached| {
                        cached.calendar.name.as_str()
                    });
                let title = if status.task_title.is_empty() {
                    strings.removed_block
                } else {
                    &status.task_title
                };
                v_flex()
                    .gap_1()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .py_2()
                    .child(Label::new(title.to_owned()).weight(FontWeight::MEDIUM))
                    .child(
                        Label::new(format!(
                            "{} · {calendar}",
                            if status.pending {
                                strings.pending
                            } else {
                                strings.published
                            }
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .children(self.conflict_actions(status, cx))
                    .children(state.error.as_ref().map(|error| {
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error)
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .mb_4()
            .gap_2()
            .child(Label::new(strings.accounts).weight(FontWeight::MEDIUM))
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.))
                    .bg(colors.surface_background)
                    .children(google_status.map(|email| {
                        Label::new(email.to_owned())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    }))
                    .when(running, |el| {
                        el.child(Label::new(strings.working).color(Color::Muted))
                    })
                    .when(!running, |el| {
                        el.child(
                            h_flex()
                                .gap_1()
                                .flex_wrap()
                                .when(service.google.configured, |el| {
                                    el.child(
                                        work_button(
                                            "connect-google",
                                            if google_status.is_some() {
                                                strings.reconnect_google
                                            } else {
                                                strings.connect_google
                                            },
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.service.update(cx, |service, cx| {
                                                    service.connect_google(false, cx)
                                                })
                                            }),
                                        ),
                                    )
                                })
                                .when(google_status.is_none(), |el| {
                                    el.child(
                                        work_button(
                                            "choose-google-client",
                                            strings.choose_google_client,
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.choose_client(cx)),
                                        ),
                                    )
                                })
                                .when(google_status.is_some(), |el| {
                                    el.child(
                                        work_button(
                                            "connect-google-publishing",
                                            if service.google.publishing {
                                                strings.reauthorize_publish
                                            } else {
                                                strings.enable_publish
                                            },
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.service.update(cx, |service, cx| {
                                                    service.connect_google(true, cx)
                                                })
                                            }),
                                        ),
                                    )
                                    .child(
                                        work_button(
                                            "disconnect-google",
                                            strings.disconnect_google,
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.service.update(cx, |service, cx| {
                                                    service.disconnect_google(cx)
                                                })
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    work_button(
                                        "connect-icloud",
                                        if service.icloud_connected {
                                            strings.reconnect_icloud
                                        } else {
                                            strings.connect_icloud
                                        },
                                        false,
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.service.update(cx, |service, cx| {
                                                service.connect_icloud(cx)
                                            })
                                        },
                                    )),
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                work_button(
                                    "refresh-calendar-accounts",
                                    if service.refresh_running {
                                        strings.refreshing
                                    } else {
                                        strings.refresh
                                    },
                                    false,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |_, _, _, cx| {
                                        if let Some(service) = accounts(cx) {
                                            service
                                                .update(cx, |service, cx| service.refresh_now(cx));
                                        }
                                    },
                                )),
                            )
                            .when(
                                service.destination.is_some()
                                    && !service.calendars.iter().any(|c| {
                                        Some(&c.calendar.key) == service.destination.as_ref()
                                    }),
                                |el| {
                                    el.child(
                                        work_button(
                                            "clear-missing-destination",
                                            strings.clear_destination,
                                            false,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.service.update(cx, |service, cx| {
                                                    service.set_destination(None, cx)
                                                })
                                            }),
                                        ),
                                    )
                                },
                            ),
                    )
                    .children(service.action_error.as_ref().map(|error| {
                        Label::new(error.clone())
                            .size(LabelSize::Small)
                            .color(Color::Error)
                    }))
                    .children(
                        provider_errors.map(|error| {
                            Label::new(error).size(LabelSize::Small).color(Color::Error)
                        }),
                    )
                    .when(
                        service.icloud_connected
                            && service.provider_error(Provider::ICloud).is_none()
                            && !service.calendars.iter().any(|calendar| {
                                calendar.calendar.present
                                    && calendar.calendar.key.provider == Provider::ICloud
                            }),
                        |el| {
                            el.child(
                                Label::new(strings.missing_icloud)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        },
                    )
                    .children(calendar_rows),
            )
            .when(!publications.is_empty(), |el| {
                el.child(
                    h_flex()
                        .justify_between()
                        .child(Label::new(strings.publication).weight(FontWeight::MEDIUM))
                        .child(
                            Label::new(
                                strings
                                    .pending_count
                                    .replace("{count}", &pending.to_string()),
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    v_flex()
                        .px_3()
                        .rounded(px(8.))
                        .bg(colors.surface_background)
                        .children(publications),
                )
            })
    }
}
