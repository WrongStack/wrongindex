//! Startup, updates and the service's own settings.

use gpui::{Context, Div, PromptLevel, div, prelude::*, px};
use serde_json::{Value, json};

use super::widgets::{self, Kind};
use super::{Desktop, show_value};
use crate::logic::{self, SETTING_FIELDS, Tone};
use crate::model::Command;

impl Desktop {
    pub(super) fn settings(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(self.startup(cx))
            .child(self.updates(cx))
            .child(self.service_settings(cx))
    }

    fn startup(&self, cx: &mut Context<Self>) -> Div {
        let mut panel = widgets::panel().child(widgets::heading("Startup").mt_0());
        match &self.snap.autostart {
            None => panel.child(widgets::note("Reading settings…")),
            Some(Err(e)) => panel.child(widgets::line(e.clone(), Tone::Bad)),
            Some(Ok(a)) => {
                let on = a["enabled"] == true;
                panel = panel
                    .child(self.clickable(
                        widgets::toggle(
                            "autostart",
                            "Start the index service when I sign in",
                            on,
                            !self.is_busy("autostart"),
                        ),
                        !self.is_busy("autostart"),
                        cx,
                        move |view, _, _| view.send("autostart", Command::Autostart(!on)),
                    ))
                    .child(widgets::note("Only the background service starts; this window does not open. Closing this window never stops the service."));
                if on && a["matchesThisInstall"] != true {
                    panel = panel.child(widgets::line(
                        format!(
                            "The sign-in entry points to another copy: {}. Turn the option off and on to use this one.",
                            show_value(&a["command"])
                        ),
                        Tone::Bad,
                    ));
                }
                panel
            }
        }
    }

    fn updates(&self, cx: &mut Context<Self>) -> Div {
        let l = &self.snap.update_last;
        let checking = self.is_busy("update-check");
        let starting = self.is_busy("update-start") || self.following_update;
        let busy = checking || starting || l["running"] == true;
        let mut panel = widgets::panel()
            .child(widgets::heading("Updates").mt_0())
            .child(widgets::line(
                format!("This copy: WrongIndex {}", env!("CARGO_PKG_VERSION")),
                Tone::Normal,
            ))
            .child(widgets::row().child(self.btn(
                "check-updates",
                "Check for updates",
                Kind::Plain,
                !busy,
                cx,
                |view, _, _| {
                    view.prefs.last_check_ms = logic::now_ms();
                    view.prefs.save(&view.root);
                    view.send("update-check", Command::UpdateCheck { quiet: false });
                },
            )));
        if checking {
            panel = panel.child(widgets::note("Checking GitHub for a newer release…"));
        } else {
            match &self.snap.update_check {
                Some(Err(e)) => {
                    panel = panel.child(widgets::line(
                        format!("The update check failed: {e}"),
                        Tone::Bad,
                    ))
                }
                Some(Ok(c)) if c["updateAvailable"] == true => {
                    let latest = show_value(&c["latest"]);
                    panel = panel
                        .child(widgets::line(
                            format!(
                                "WrongIndex {latest} is available (this copy: {}).",
                                show_value(&c["current"])
                            ),
                            Tone::Good,
                        ))
                        .child(widgets::mono(show_value(&c["release"])));
                    panel = if c["installed"] == true {
                        panel.child(widgets::row().child(self.btn(
                            "start-update",
                            format!("Update to {latest}…"),
                            Kind::Primary,
                            !busy,
                            cx,
                            move |_, window, cx| {
                                let answer = window.prompt(
                                    PromptLevel::Info,
                                    &format!("Update to WrongIndex {latest}?"),
                                    Some("The release is downloaded from GitHub and its SHA-256 checked before anything is installed. The index service stops while the files are replaced and starts again if it was running. This window closes and opens again."),
                                    &["Cancel", "Update"],
                                    cx,
                                );
                                cx.spawn(async move |view, cx| {
                                    if answer.await == Ok(1) {
                                        let _ = view.update(cx, |view, cx| {
                                            view.following_update = true;
                                            view.send("update-start", Command::UpdateStart);
                                            cx.notify();
                                        });
                                    }
                                })
                                .detach();
                            },
                        )))
                    } else {
                        panel.child(widgets::note("This copy was not installed by the WrongIndex installer, so it cannot update itself. Install the release from GitHub instead."))
                    };
                }
                Some(Ok(c)) => {
                    panel = panel.child(widgets::line(
                        format!(
                            "This is the latest release ({}).",
                            show_value(&c["current"])
                        ),
                        Tone::Normal,
                    ))
                }
                None => {}
            }
        }
        let last_line = if l["running"] == true || starting {
            Some((
                "Updating… This window closes while the new version is installed and opens again afterwards.".to_string(),
                Tone::Warn,
            ))
        } else if l["atMs"].is_number() && l["result"].is_object() {
            let r = &l["result"];
            Some((
                if r["updated"] == true {
                    format!(
                        "Last update {}: {} → {}{}.",
                        logic::when(&l["atMs"]),
                        show_value(&r["from"]),
                        show_value(&r["to"]),
                        if r["serviceRestarted"] == true {
                            ", service restarted"
                        } else {
                            ""
                        }
                    )
                } else {
                    format!(
                        "Last update {}: already the latest release ({}).",
                        logic::when(&l["atMs"]),
                        show_value(&r["current"])
                    )
                },
                Tone::Muted,
            ))
        } else if l["atMs"].is_number() && l["error"].is_string() {
            Some((
                format!(
                    "The update started {} failed: {}",
                    logic::when(&l["atMs"]),
                    show_value(&l["error"])
                ),
                Tone::Bad,
            ))
        } else if l["atMs"].is_number() && l["pending"] == true {
            Some((
                format!(
                    "An update started {} has no result yet; it may still be running.",
                    logic::when(&l["atMs"])
                ),
                Tone::Muted,
            ))
        } else {
            None
        };
        if let Some((t, tone)) = last_line {
            panel = panel.child(widgets::line(t, tone));
        }
        let auto = self.prefs.auto_check_updates;
        panel
            .child(self.clickable(
                widgets::toggle("auto-update-check", "Check for updates once a day", auto, true),
                true,
                cx,
                move |view, _, _| {
                    view.prefs.auto_check_updates = !auto;
                    view.prefs.save(&view.root);
                    if !auto {
                        view.auto_check(true);
                    }
                },
            ))
            .child(widgets::note("Checks ask GitHub (api.github.com) for the latest WrongIndex release; nothing else is sent. Off unless you turn it on. Updates are verified against the release's SHA-256 before anything is installed."))
    }

    fn service_settings(&self, cx: &mut Context<Self>) -> Div {
        let mut panel = widgets::panel().child(widgets::heading("Index service").mt_0());
        if !self.connected() {
            return panel.child(widgets::note(
                "Service settings are shown while the service runs.",
            ));
        }
        let sv = &self.snap.settings;
        let applied = &sv["settings"];
        let defaults = &sv["defaults"];
        let busy = self.is_busy("settings");
        let paused = applied["paused"] == true;
        panel = panel
            .child(self.clickable(
                widgets::toggle("paused", "Pause background indexing", paused, !busy),
                !busy,
                cx,
                move |view, _, _| {
                    view.send(
                        "settings",
                        Command::ApplySettings {
                            patch: json!({"paused": !paused}),
                            revision: None,
                        },
                    )
                },
            ))
            .child(widgets::note("While paused, edits wait and projects are not shown as current. Searches and explicit index requests still run. Resuming reconciles what changed."));
        let drafts: Vec<(usize, String)> = self
            .setting_inputs
            .iter()
            .enumerate()
            .map(|(i, input)| (i, input.read(cx).text().to_string()))
            .collect();
        let changed = drafts.iter().any(|(i, d)| {
            applied[SETTING_FIELDS[*i].key]
                .as_u64()
                .map(|v| v.to_string())
                .as_deref()
                != Some(d.trim())
        });
        let mut fields = div().flex().flex_col().gap_3();
        for (i, f) in SETTING_FIELDS.iter().enumerate() {
            let default = &defaults[f.key];
            let differs = !default.is_null() && applied[f.key] != *default;
            fields = fields.child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        div()
                            .w(px(300.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(f.label))
                            .when(!f.note.is_empty(), |c| c.child(widgets::label(f.note))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(div().w(px(140.)).child(self.setting_inputs[i].clone()))
                            .child(widgets::note(if f.unit.is_empty() {
                                format!("{}–{}", f.min, f.max)
                            } else {
                                format!("{} · {}–{}", f.unit, f.min, f.max)
                            }))
                            .when(differs, |c| {
                                c.child(widgets::note(format!("default {}", show_value(default))))
                            }),
                    ),
            );
        }
        let revision = sv["revision"].as_u64();
        let applied_for_patch: Value = applied.clone();
        panel
            .child(fields)
            .child(
                widgets::row()
                    .child(self.btn(
                        "apply-settings",
                        "Apply",
                        Kind::Primary,
                        !busy && changed,
                        cx,
                        move |view, _, cx| {
                            let drafts: Vec<(usize, String)> = view
                                .setting_inputs
                                .iter()
                                .enumerate()
                                .map(|(i, input)| (i, input.read(cx).text().to_string()))
                                .collect();
                            match logic::settings_patch(&applied_for_patch, &drafts) {
                                Ok(patch) if patch.as_object().is_some_and(|o| o.is_empty()) => {}
                                Ok(patch) => {
                                    // Refill from the reply, whatever the service applied.
                                    view.settings_revision = None;
                                    view.send(
                                        "settings",
                                        Command::ApplySettings { patch, revision },
                                    )
                                }
                                Err(e) => view.show(e, true),
                            }
                        },
                    ))
                    .child(self.btn(
                        "discard-settings",
                        "Discard changes",
                        Kind::Plain,
                        !busy && changed,
                        cx,
                        |view, _, cx| {
                            view.settings_revision = None;
                            view.sync_setting_inputs(cx);
                        },
                    ))
                    .child(widgets::note(format!(
                        "Revision {}",
                        show_value(&sv["revision"])
                    ))),
            )
            .child(widgets::field(
                "Service data location",
                widgets::mono(self.root.display().to_string()),
            ))
            .child(widgets::field(
                "Effective parallel jobs",
                show_value(&self.snap.metrics["limits"]["effectiveParallelJobs"]),
            ))
            .child(widgets::field(
                "Native run memory cap",
                self.snap.metrics["limits"]["nativeMemoryCapBytes"]
                    .as_u64()
                    .map_or("Unavailable".into(), logic::readable_bytes),
            ))
    }
}
