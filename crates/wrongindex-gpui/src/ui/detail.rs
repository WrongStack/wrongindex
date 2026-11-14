//! One project: its live state and every action on it.

use gpui::{Context, Div, FontWeight, SharedString, div, prelude::*, rgb};
use serde_json::Value;
use wrongindex_contracts::{BackendPreference, WatchMode, WatchPolicy};

use super::widgets::{self, Kind};
use super::{Desktop, show_value, theme};
use crate::logic::{self, ProjectAction, Tone};
use crate::model::{Command, Detail};

fn act(project: &str, action: ProjectAction) -> Command {
    Command::Action {
        project: project.to_string(),
        action,
    }
}

impl Desktop {
    pub(super) fn detail(&self, cx: &mut Context<Self>) -> Div {
        let back = self.btn(
            "back",
            "← All projects",
            Kind::Plain,
            true,
            cx,
            |view, _, _| view.close_project(),
        );
        let id = self.selected.clone().unwrap_or_default();
        let Some(d) = self.snap.detail.as_ref().filter(|d| d.project_id == id) else {
            let mut page = div()
                .flex()
                .flex_col()
                .gap_4()
                .child(widgets::row().child(back));
            page = if self.connected() {
                page.child(widgets::note("Reading the project from the service…"))
            } else {
                page.child(self.disconnected(cx))
            };
            return page;
        };
        let s = &d.status;
        let p = &s["project"];
        let name = p["displayName"].as_str().unwrap_or("?").to_string();
        let owned = s["ownership"] == "owned";
        let key = format!("project:{id}");

        let mut page = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(widgets::row().child(back))
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name.clone()),
            );

        // ── state ──
        let freshness = s["freshness"].as_str().unwrap_or("unknown");
        let mut fresh = widgets::row().child(widgets::badge(
            logic::freshness_label(freshness),
            logic::freshness_tone(freshness),
        ));
        if let Some(reason) = s["reconcileReason"].as_str() {
            fresh = fresh.child(widgets::note(format!("reconcile: {reason}")));
        }
        let mut watch = div().flex().flex_col().gap_1().child(format!(
            "{} · {} · {} session(s)",
            s["watchHealth"].as_str().unwrap_or(""),
            logic::policy_label(p["watchPolicy"].as_str().unwrap_or("")),
            s["sessions"]
        ));
        if let Some((t, tone)) = logic::retry_note(s) {
            watch = watch.child(widgets::line(t, tone));
        }
        if let Some((t, tone)) =
            logic::polling_note(&s["polling"], p["watchMode"].as_str().unwrap_or("auto"))
        {
            watch = watch.child(widgets::line(t, tone));
        }
        let integrity = logic::integrity_lines(&s["lastCheck"])
            .into_iter()
            .fold(div().flex().flex_col(), |c, (t, tone)| {
                c.child(widgets::line(t, tone))
            });
        let stats = match &d.stats {
            Some(st) => format!(
                "{} files · {} symbols",
                show_value(&st["totalFiles"]),
                show_value(&st["totalSymbols"])
            ),
            None => "—".into(),
        };
        let mut state = widgets::panel()
            .child(widgets::field("Freshness", fresh))
            .child(widgets::field(
                "Ownership",
                widgets::badge(
                    logic::ownership_label(s["ownership"].as_str().unwrap_or("")),
                    if owned { Tone::Good } else { Tone::Muted },
                ),
            ))
            .child(widgets::field("Activity", show_value(&s["activity"])))
            .child(widgets::field("Watch", watch))
            .child(widgets::field("Integrity", integrity))
            .child(widgets::field("Generation", show_value(&s["generation"])))
            .child(widgets::field("Published index", stats))
            .child(widgets::field(
                "Pending paths",
                show_value(&s["pendingPaths"]),
            ))
            .child(widgets::field(
                "Last indexed",
                logic::when(&s["lastIndexedMs"]),
            ))
            .child(widgets::field(
                "Last verified",
                if s["lastVerifiedMs"].is_null() {
                    "Not yet".to_string()
                } else {
                    format!("{} · whole project", logic::when(&s["lastVerifiedMs"]))
                },
            ))
            .child(widgets::field(
                "Written by",
                logic::backend_label(p["backend"].as_str().unwrap_or("")),
            ))
            .child(widgets::field(
                "Checkout",
                widgets::mono(show_value(&p["checkoutRoot"])),
            ))
            .child(widgets::field(
                "Index directory",
                widgets::mono(show_value(&p["indexDir"])),
            ))
            .child(widgets::field(
                "Configuration revision",
                show_value(&p["configurationRevision"]),
            ));
        if let Some(err) = s["lastError"].as_str() {
            state = state.child(widgets::field(
                "Last error",
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::line(err.to_string(), Tone::Bad))
                    .child(widgets::note(logic::next_action(err))),
            ));
        }
        if s["progress"].is_object() {
            state = state.child(widgets::field(
                "Running now",
                widgets::line(logic::progress_text(&s["progress"]), Tone::Warn),
            ));
        }
        page = page.child(state);

        // ── actions ──
        let mut actions = widgets::row();
        actions = if owned {
            actions.child(self.cmd(
                "release",
                "Release ownership",
                Kind::Plain,
                &key,
                act(&id, ProjectAction::Release),
                cx,
            ))
        } else {
            actions.child(self.cmd(
                "claim",
                "Claim ownership",
                Kind::Primary,
                &key,
                act(&id, ProjectAction::Claim),
                cx,
            ))
        };
        actions = actions
            .child(self.cmd(
                "index-now",
                "Index now",
                if owned { Kind::Primary } else { Kind::Plain },
                &key,
                act(&id, ProjectAction::IndexNow),
                cx,
            ))
            .child(self.cmd(
                "check",
                "Check index",
                Kind::Plain,
                &key,
                act(&id, ProjectAction::Check),
                cx,
            ));
        let mut needs_owner = |label: &'static str, id_: &'static str, action: ProjectAction| {
            if owned {
                self.cmd(id_, label, Kind::Plain, &key, act(&id, action), cx)
            } else {
                widgets::button(id_, label, Kind::Plain, false)
            }
        };
        actions = actions
            .child(needs_owner("Reconcile", "reconcile", ProjectAction::Reconcile))
            .child(needs_owner(
                "Fold in write-ahead log",
                "checkpoint",
                ProjectAction::Checkpoint,
            ))
            .child(self.cmd(
                "open-folder",
                "Open folder",
                Kind::Plain,
                "open-folder",
                Command::OpenFolder(id.clone()),
                cx,
            ))
            .child(self.confirm_cmd(
                "rebuild",
                "Rebuild index…",
                &key,
                "Rebuild index?".into(),
                format!(
                    "A fresh index of {name} is written from the source files and replaces the current one, even if it is damaged, unreadable, from another WrongStack version, or carries extra data. The current file is kept next to it as index.db.wrongindex-backup-<time> in {}; nothing is deleted. Use this when regular indexing refuses the existing index.",
                    show_value(&p["indexDir"])
                ),
                "Rebuild",
                act(&id, ProjectAction::Rebuild),
                cx,
            ))
            .child(self.confirm_cmd(
                "remove",
                "Remove…",
                &key,
                "Remove registration?".into(),
                format!(
                    "{name} will no longer be watched or indexed by WrongIndex. The source folder and the index at {} are kept.",
                    show_value(&p["indexDir"])
                ),
                "Remove",
                act(&id, ProjectAction::Remove),
                cx,
            ));
        let mut action_panel = widgets::panel()
            .child(widgets::heading("Actions").mt_0())
            .child(actions);
        if !owned {
            action_panel = action_panel.child(widgets::note(
                "Reconcile and folding in the log need ownership; Index now claims it first. Indexing claims exclusive ownership; unknown profiles and other writers are refused.",
            ));
        }

        // Choices apply as soon as one is picked; the service is the authority.
        let current_policy = p["watchPolicy"].as_str().unwrap_or("");
        let mut policies = widgets::row();
        for (value, label) in logic::POLICIES {
            let active = current_policy == value;
            let enabled = !active && !self.is_busy(&key);
            let (id2, key2) = (id.clone(), key.clone());
            policies = policies.child(self.clickable(
                widgets::segment(format!("policy-{value}"), label, active, enabled || active),
                enabled,
                cx,
                move |view, _, _| {
                    if let Some(p) = WatchPolicy::parse(value) {
                        view.send(&key2, act(&id2, ProjectAction::Policy(p)));
                    }
                },
            ));
        }
        let current_engine = p["backendPreference"].as_str().unwrap_or("auto");
        let mut engines = widgets::row();
        for (value, label) in logic::ENGINES {
            let active = current_engine == value;
            let enabled = !active && !self.is_busy(&key);
            let (id2, key2) = (id.clone(), key.clone());
            engines = engines.child(self.clickable(
                widgets::segment(format!("engine-{value}"), label, active, enabled || active),
                enabled,
                cx,
                move |view, _, _| {
                    if let Some(b) = BackendPreference::parse(value) {
                        view.send(&key2, act(&id2, ProjectAction::Backend(b)));
                    }
                },
            ));
        }
        let current_mode = p["watchMode"].as_str().unwrap_or("auto");
        let mut modes = widgets::row();
        for (value, label) in logic::WATCH_MODES {
            let active = current_mode == value;
            let enabled = !active && !self.is_busy(&key);
            let (id2, key2) = (id.clone(), key.clone());
            modes = modes.child(self.clickable(
                widgets::segment(format!("mode-{value}"), label, active, enabled || active),
                enabled,
                cx,
                move |view, _, _| {
                    if let Some(m) = WatchMode::parse(value) {
                        view.send(&key2, act(&id2, ProjectAction::WatchMode(m)));
                    }
                },
            ));
        }
        action_panel = action_panel
            .child(widgets::field(
                "Watch policy",
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(policies)
                    .child(widgets::note(logic::policy_label(current_policy))),
            ))
            .child(widgets::field(
                "Engine",
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(engines)
                    .child(widgets::note(logic::engine_label(current_engine))),
            ))
            .child(widgets::field(
                "Find changes by",
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(modes)
                    .child(widgets::note(logic::watch_mode_label(current_mode))),
            ));
        page = page.child(action_panel);

        page.child(self.files_and_errors(d, &key, cx))
            .child(self.backups(d, &name, owned, &key, cx))
            .child(self.search_panel(d, cx))
            .child(self.jobs_panel("Jobs", &d.jobs, cx))
            .children(ownership_history(&d.ownership))
    }

    fn files_and_errors(&self, d: &Detail, key: &str, cx: &mut Context<Self>) -> Div {
        let f = logic::files_and_errors(&d.jobs);
        let mut panel = widgets::panel().child(widgets::heading("Files and errors").mt_0());
        let lists: [(&Vec<String>, String); 3] = [
            (
                &f.oversized,
                format!(
                    "{} file(s) over the 5 MiB read limit were left out (as WrongStack does). Shrinking or splitting them brings them back on the next run.",
                    f.oversized.len()
                ),
            ),
            (
                &f.unreadable,
                format!(
                    "{} file(s) could not be read in the last run (in use by another program, or access denied). What was indexed for them before is kept; the next change or reconciliation reads them again.",
                    f.unreadable.len()
                ),
            ),
            (
                &f.unparsed,
                format!(
                    "{} TypeScript/JavaScript file(s) need Bun or Node.js to be parsed and were left as they were.",
                    f.unparsed.len()
                ),
            ),
        ];
        let mut any = false;
        for (files, text) in lists {
            if files.is_empty() {
                continue;
            }
            any = true;
            panel = panel.child(widgets::line(text, Tone::Normal)).child(
                files
                    .iter()
                    .take(50)
                    .fold(div().flex().flex_col().pl_3(), |c, file| {
                        c.child(widgets::mono(file.clone()))
                    }),
            );
        }
        if !f.degraded.is_empty() {
            any = true;
            panel = panel
                .child(widgets::line("The last run indexed some languages with the simpler regex extractor because a helper was missing or failed. Install it, restart the service and rebuild the index for full detail.", Tone::Warn))
                .child(widgets::note(f.degraded.iter().take(20).cloned().collect::<Vec<_>>().join(", ")));
        }
        for (i, g) in f.failures.iter().enumerate() {
            any = true;
            let project = d.project_id.clone();
            let job = g.latest_job.clone();
            let key2 = key.to_string();
            panel = panel.child(
                widgets::list_item()
                    .child(widgets::line(
                        g.key.clone(),
                        if f.recovered { Tone::Normal } else { Tone::Bad },
                    ))
                    .when(f.recovered, |c| {
                        c.child(widgets::note("Recovered: a later run succeeded."))
                    })
                    .child(widgets::note(format!(
                        "{} time(s) · latest {} · {}",
                        g.count,
                        logic::when(&g.latest_at),
                        logic::next_action(&g.key)
                    )))
                    .child(widgets::row().child(self.btn(
                        SharedString::from(format!("retry-{i}")),
                        "Retry",
                        Kind::Plain,
                        !self.is_busy(key),
                        cx,
                        move |view, _, _| {
                            view.send(&key2, act(&project, ProjectAction::Retry(job.clone())))
                        },
                    ))),
            );
        }
        if !any {
            panel = panel.child(widgets::note("No file or job errors in the recent jobs."));
        }
        panel
    }

    fn backups(
        &self,
        d: &Detail,
        name: &str,
        owned: bool,
        key: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut panel = widgets::panel()
            .child(widgets::heading("Index backups").mt_0())
            .child(widgets::note("Taken each time the service took ownership of this index, and when you back it up. Only the newest are kept (Settings: Index backups kept per project)."))
            .child(widgets::row().child(if owned {
                self.cmd(
                    "backup-now",
                    "Back up now",
                    Kind::Plain,
                    key,
                    act(&d.project_id, ProjectAction::Backup),
                    cx,
                )
            } else {
                widgets::button("backup-now", "Back up now (claim ownership first)", Kind::Plain, false)
            }));
        if d.backups.is_empty() {
            return panel.child(widgets::note("No backups yet."));
        }
        for (i, b) in d.backups.iter().enumerate() {
            let file = b["name"].as_str().unwrap_or("").to_string();
            let restore = if owned && logic::is_backup_name(&file) {
                self.confirm_cmd(
                    SharedString::from(format!("restore-{i}")),
                    "Restore…",
                    key,
                    "Restore this backup?".into(),
                    format!("The index of {name} is replaced with {file}. The current index is first saved as another backup, so nothing is lost. Clients see the restored generation."),
                    "Restore",
                    act(&d.project_id, ProjectAction::Restore(file.clone())),
                    cx,
                )
            } else {
                widgets::button(
                    SharedString::from(format!("restore-{i}")),
                    "Restore…",
                    Kind::Plain,
                    false,
                )
            };
            panel = panel.child(
                widgets::list_item().child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(div().min_w_0().child(widgets::mono(file.clone())).child(
                            widgets::note(format!(
                                "{} · {:.1} MiB",
                                logic::when(&b["modifiedAtMs"]),
                                b["bytes"].as_f64().unwrap_or(0.0) / 1024.0 / 1024.0
                            )),
                        ))
                        .child(restore),
                ),
            );
        }
        panel
    }

    fn search_panel(&self, d: &Detail, cx: &mut Context<Self>) -> Div {
        let busy = self.is_busy("search");
        let mut panel = widgets::panel()
            .child(widgets::heading("Search inspector").mt_0())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.search_input.clone()))
                    .child(self.btn(
                        "search",
                        if busy { "Searching…" } else { "Search" },
                        Kind::Plain,
                        !busy,
                        cx,
                        |view, _, cx| view.run_search(cx),
                    )),
            );
        let result = self
            .snap
            .search
            .as_ref()
            .filter(|s| s.project_id == d.project_id);
        let Some(sr) = result else {
            return panel.child(widgets::note(
                "Searches the published index read-only, the way WrongStack clients do.",
            ));
        };
        let r = &sr.result;
        let rows = r["result"]["results"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let total = r["result"]["total"].as_u64().unwrap_or(rows.len() as u64);
        panel = panel.child(widgets::note(format!(
            "“{}”: {total} match(es) in generation {} · served by the {} reader. Read-only.",
            sr.query,
            show_value(&r["generation"]),
            show_value(&r["backend"])
        )));
        if rows.is_empty() {
            return panel.child(widgets::note("No matches."));
        }
        let root = self
            .snap
            .detail
            .as_ref()
            .and_then(|d| d.status["project"]["checkoutRoot"].as_str())
            .unwrap_or("")
            .trim_end_matches(['\\', '/'])
            .to_lowercase();
        for x in rows.iter().take(50) {
            let file = x["file"].as_str().unwrap_or("");
            let rel = if !root.is_empty() && file.to_lowercase().starts_with(&root) {
                file.get(root.len() + 1..).unwrap_or(file)
            } else {
                file
            };
            panel = panel.child(
                widgets::list_item()
                    .child(
                        widgets::row()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(show_value(&x["name"])),
                            )
                            .child(widgets::badge(show_value(&x["kind"]), Tone::Muted))
                            .child(widgets::mono(format!("{rel}:{}", show_value(&x["line"])))),
                    )
                    .when(x["signature"].is_string(), |c| {
                        c.child(widgets::mono(show_value(&x["signature"])))
                    }),
            );
        }
        panel
    }

    /// Jobs, newest first, with Cancel for a queued one.
    pub(super) fn jobs_panel(&self, title: &str, jobs: &[Value], cx: &mut Context<Self>) -> Div {
        let mut panel = widgets::panel().child(widgets::heading(title.to_string()).mt_0());
        if jobs.is_empty() {
            return panel.child(widgets::note("No jobs yet."));
        }
        for (i, j) in jobs.iter().enumerate() {
            let state = j["state"].as_str().unwrap_or("unknown");
            let job_id = j["jobId"].as_str().unwrap_or("").to_string();
            let project = j["projectId"]
                .as_str()
                .map(|p| self.snap.project_name(p))
                .unwrap_or_default();
            let result = if j["error"].is_object() {
                widgets::line(
                    format!(
                        "{}: {}",
                        show_value(&j["error"]["code"]),
                        show_value(&j["error"]["message"])
                    ),
                    Tone::Bad,
                )
            } else {
                widgets::note(logic::job_summary(j))
            };
            let mut top = widgets::row()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(logic::job_kind(j)),
                )
                .child(widgets::badge(
                    state.to_string(),
                    logic::job_state_tone(state),
                ));
            if !project.is_empty() {
                top = top.child(widgets::note(project));
            }
            if state == "queued" {
                let id = job_id.clone();
                top = top.child(self.btn(
                    SharedString::from(format!("cancel-{i}-{job_id}")),
                    "Cancel",
                    Kind::Plain,
                    !self.is_busy("cancel"),
                    cx,
                    move |view, _, _| view.send("cancel", Command::CancelJob(id.clone())),
                ));
            }
            panel = panel.child(
                widgets::list_item()
                    .child(top)
                    .child(widgets::note(format!(
                        "admitted {} · finished {}",
                        logic::when(&j["admittedAtMs"]),
                        logic::when(&j["completedAtMs"])
                    )))
                    .child(result)
                    .child(widgets::mono(job_id)),
            );
        }
        panel
    }
}

fn ownership_history(ownership: &Value) -> Option<Div> {
    let journal = ownership["journal"].as_array().filter(|a| !a.is_empty())?;
    let mut panel = widgets::panel().child(widgets::heading("Ownership history").mt_0());
    for j in journal {
        panel = panel.child(
            widgets::list_item()
                .child(widgets::line(
                    format!(
                        "{} · epoch {} · {}",
                        logic::when(&j["atMs"]),
                        show_value(&j["epoch"]),
                        show_value(&j["phase"])
                    ),
                    Tone::Normal,
                ))
                .child(widgets::mono(format!(
                    "{} → {}",
                    show_value(&j["oldOwner"]),
                    show_value(&j["newOwner"])
                ))),
        );
    }
    Some(panel.text_color(rgb(theme::TEXT)))
}
