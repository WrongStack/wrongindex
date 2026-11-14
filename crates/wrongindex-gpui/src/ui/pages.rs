//! Overview, indexes on this machine, activity and diagnostics.

use gpui::{Context, Div, FontWeight, SharedString, div, prelude::*};
use serde_json::Value;

use super::widgets::{self, Kind};
use super::{Desktop, show_value};
use crate::logic::{self, IndexFilter, ProjectAction, Tone};
use crate::model::Command;

impl Desktop {
    pub(super) fn overview(&self, cx: &mut Context<Self>) -> Div {
        if !self.connected() {
            return div().child(self.disconnected(cx));
        }
        let ps = self.snap.projects();
        let count = |pred: &dyn Fn(&Value) -> bool| ps.iter().filter(|p| pred(p)).count();
        let h = &self.snap.health;
        let mut page = div().flex().flex_col().gap_4();
        for (i, b) in logic::banners(ps, self.snap.resources())
            .into_iter()
            .enumerate()
        {
            let mut banner = widgets::banner(if b.bad { Tone::Bad } else { Tone::Warn })
                .child(widgets::line(b.title, Tone::Normal));
            for n in b.notes {
                banner = banner.child(widgets::note(n));
            }
            if let Some((id, name)) = b.project {
                let mut row = widgets::row();
                if b.fold {
                    row = row.child(self.cmd(
                        SharedString::from(format!("fold-{i}")),
                        format!("Fold in the log of {name} now"),
                        Kind::Plain,
                        &format!("project:{id}"),
                        Command::Action {
                            project: id.clone(),
                            action: ProjectAction::Checkpoint,
                        },
                        cx,
                    ));
                }
                row = row.child(self.btn(
                    SharedString::from(format!("open-banner-{i}")),
                    format!("Open {name}"),
                    Kind::Plain,
                    true,
                    cx,
                    move |view, _, cx| view.open_project(id.clone(), cx),
                ));
                banner = banner.child(row);
            }
            page = page.child(banner);
        }
        let cards = widgets::row()
            .gap_3()
            .child(widgets::card("PROJECTS", ps.len().to_string()))
            .child(widgets::card(
                "WATCHING",
                count(&|p| p["watchHealth"].is_string() && p["watchHealth"] != "off").to_string(),
            ))
            .child(widgets::card(
                "CURRENT",
                count(&|p| p["freshness"] == "current").to_string(),
            ))
            .child(widgets::card(
                "PENDING",
                count(&|p| p["freshness"] == "pending").to_string(),
            ))
            .child(widgets::card(
                "DEGRADED",
                count(&|p| p["freshness"] == "degraded" || p["freshness"] == "unavailable")
                    .to_string(),
            ))
            .child(widgets::card(
                "QUEUED / RUNNING",
                format!(
                    "{} / {}",
                    show_value(&h["queuedJobs"]),
                    show_value(&h["runningJobs"])
                ),
            ));
        page = page.child(cards);
        let attention: Vec<Value> = ps
            .iter()
            .filter(|p| p["lastError"].is_string())
            .cloned()
            .collect();
        if !attention.is_empty() {
            page = page
                .child(widgets::heading("Needs attention"))
                .child(self.project_list(&attention, "attention", cx));
        }
        page = page.child(widgets::heading("Your projects"));
        if ps.is_empty() {
            page.child(
                widgets::panel()
                    .child(widgets::note("No projects are registered yet."))
                    .child(widgets::row().child(self.btn(
                        "first-add",
                        "Add project",
                        Kind::Primary,
                        true,
                        cx,
                        |view, _, cx| {
                            view.go(super::Tab::Projects);
                            view.browse(cx);
                        },
                    ))),
            )
        } else {
            page.child(self.project_list(ps, "overview", cx))
        }
    }

    pub(super) fn indexes(&self, cx: &mut Context<Self>) -> Div {
        let m = &self.snap.machine;
        let busy = self.is_busy("machine");
        let now = logic::now_ms();
        let mut filters = widgets::row();
        for f in IndexFilter::ALL {
            let active = self.index_filter == f;
            filters = filters.child(self.clickable(
                widgets::segment(format!("filter-{}", f.key()), f.short(), active, true),
                !active,
                cx,
                move |view, _, _| {
                    view.index_filter = f;
                    view.prefs.index_filter = f.key().to_string();
                    view.prefs.save(&view.root);
                },
            ));
        }
        let scanned = m.scan.as_ref().map(|s| {
            format!(
                "Scanned {} in {:.1} s",
                logic::ago(&Value::from(m.at_ms)),
                s["tookMs"].as_f64().unwrap_or(0.0) / 1000.0
            )
        });
        let mut page = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(widgets::note(format!(
                "Every WrongStack index{}, read without changing anything. WrongStack writes most of them with its own engine; WrongIndex writes only the projects registered here. Sizes include each index's write-ahead log; counts marked * leave out rows still only in the log of a large index.",
                m.scan
                    .as_ref()
                    .and_then(|s| s["wrongstackHome"].as_str())
                    .map(|h| format!(" under {h}"))
                    .unwrap_or_default()
            )))
            .child(
                widgets::row()
                    .child(self.cmd(
                        "scan",
                        if busy { "Scanning…" } else { "Scan again" },
                        Kind::Plain,
                        "machine",
                        Command::ScanMachine,
                        cx,
                    ))
                    .child(filters)
                    .children(scanned.map(widgets::note)),
            );
        if let Some(e) = &m.error {
            return page.child(widgets::line(format!("The scan failed: {e}"), Tone::Bad));
        }
        let Some(scan) = &m.scan else {
            return page.child(widgets::note(if busy {
                "Scanning…"
            } else {
                "Not scanned yet."
            }));
        };
        let all = scan["indexes"].as_array().cloned().unwrap_or_default();
        let count = |pred: &dyn Fn(&Value) -> bool| all.iter().filter(|i| pred(i)).count();
        let service = scan["serviceConnected"] == true;
        page = page.child(
            widgets::row()
                .gap_3()
                .child(widgets::card("INDEXES", all.len().to_string()))
                .child(widgets::card(
                    "TOTAL SIZE",
                    logic::megabytes(scan["totalBytes"].as_f64().unwrap_or(0.0)),
                ))
                .child(widgets::card(
                    "WRITTEN IN 24 H",
                    count(&|i| IndexFilter::Day.keeps(i, now)).to_string(),
                ))
                .child(widgets::card(
                    "IN USE NOW",
                    count(&|i| logic::writer_of(i, now).0).to_string(),
                ))
                .child(widgets::card(
                    "IN WRONGINDEX",
                    if service {
                        count(&|i| !i["registered"].is_null()).to_string()
                    } else {
                        "—".into()
                    },
                ))
                .child(widgets::card(
                    "CHECKOUT GONE",
                    count(&|i| i["checkoutExists"] == false).to_string(),
                )),
        );
        let shown: Vec<&Value> = all
            .iter()
            .filter(|i| self.index_filter.keeps(i, now))
            .collect();
        page = page.child(widgets::heading(format!(
            "{} ({})",
            self.index_filter.title(),
            shown.len()
        )));
        if shown.is_empty() {
            return page.child(widgets::empty("No index matches."));
        }
        let connected = self.connected();
        let mut list = div().flex().flex_col().gap_2();
        for (n, i) in shown.iter().take(500).enumerate() {
            let (active, writer) = logic::writer_of(i, now);
            let mut place = Vec::new();
            if let Some(w) = i["worktree"].as_str() {
                place.push(format!("linked worktree {w}"));
            }
            if i["checkoutSource"] == "files" {
                place.push("folder of the indexed files".into());
            }
            if i["checkoutExists"] == false {
                place.push("no longer exists".into());
            }
            let counts = if i["empty"] == true {
                widgets::note("empty file (created, never written)")
            } else if let Some(e) = i["readError"].as_str() {
                widgets::line(format!("unreadable: {e}"), Tone::Bad)
            } else {
                widgets::note(format!(
                    "{} files / {} symbols{}",
                    i["files"].as_u64().unwrap_or(0),
                    i["symbols"].as_u64().unwrap_or(0),
                    if i["countsIncludeLog"] == true {
                        ""
                    } else {
                        " *"
                    }
                ))
            };
            let action = if let Some(id) = i["registered"]["projectId"].as_str() {
                let id = id.to_string();
                self.btn(
                    SharedString::from(format!("index-open-{n}")),
                    format!("Open {}", show_value(&i["registered"]["displayName"])),
                    Kind::Plain,
                    true,
                    cx,
                    move |view, _, cx| view.open_project(id.clone(), cx),
                )
                .into_any_element()
            } else if connected
                && i["checkoutExists"] == true
                && i["checkoutSource"] != "files"
                && let Some(root) = i["checkoutRoot"].as_str()
            {
                let root = std::path::PathBuf::from(root);
                self.btn(
                    SharedString::from(format!("index-add-{n}")),
                    "Add…",
                    Kind::Plain,
                    !self.is_busy("preview"),
                    cx,
                    move |view, _, cx| {
                        let text = root.display().to_string();
                        view.path_input.update(cx, |i, cx| i.set_text(text, cx));
                        view.go(super::Tab::Projects);
                        view.close_project();
                        view.send("preview", Command::Preview(root.clone()));
                    },
                )
                .into_any_element()
            } else {
                widgets::note(if connected { "" } else { "service not running" }).into_any_element()
            };
            list = list.child(
                widgets::list_item().child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    widgets::row()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(logic::index_name(i)),
                                        )
                                        .child(widgets::badge(
                                            logic::megabytes(
                                                i["dbBytes"].as_f64().unwrap_or(0.0)
                                                    + i["walBytes"].as_f64().unwrap_or(0.0),
                                            ),
                                            Tone::Muted,
                                        ))
                                        .child(widgets::badge(
                                            format!("written {}", logic::ago(&i["lastWriteMs"])),
                                            Tone::Muted,
                                        )),
                                )
                                .child(widgets::mono(
                                    i["checkoutRoot"]
                                        .as_str()
                                        .map(str::to_string)
                                        .unwrap_or_else(|| {
                                            format!(
                                                "(checkout unknown) {}",
                                                show_value(&i["projectDir"])
                                            )
                                        }),
                                ))
                                .when(!place.is_empty(), |c| {
                                    c.child(widgets::line(
                                        place.join(" · "),
                                        if i["checkoutExists"] == false {
                                            Tone::Bad
                                        } else {
                                            Tone::Muted
                                        },
                                    ))
                                })
                                .child(counts)
                                .child(widgets::line(
                                    writer,
                                    if active { Tone::Good } else { Tone::Muted },
                                )),
                        )
                        .child(action),
                ),
            );
        }
        page = page.child(list);
        if shown.len() > 500 {
            page = page.child(widgets::note(format!(
                "The first 500 of {} are shown.",
                shown.len()
            )));
        }
        if scan["truncated"] == true {
            page = page.child(widgets::note("More indexes exist than the scan lists."));
        }
        page
    }

    pub(super) fn activity(&self) -> Div {
        let mut page = div().flex().flex_col().gap_4().child(widgets::note(format!(
            "The last {} service events seen by this window. After a reconnect the views are re-read from the service; missed events are not replayed here.",
            crate::model::ACTIVITY_CAP
        )));
        if self.snap.activity.is_empty() {
            return page.child(widgets::empty("No events yet."));
        }
        let mut list = div().flex().flex_col().gap_2();
        for e in &self.snap.activity {
            let project = e["projectId"]
                .as_str()
                .map(|id| self.snap.project_name(id))
                .unwrap_or_else(|| "—".into());
            let data = if e["data"].is_null() {
                String::new()
            } else {
                let mut s = e["data"].to_string();
                if s.len() > 400 {
                    let mut cut = 400;
                    while !s.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    s.truncate(cut);
                    s.push('…');
                }
                s
            };
            list = list.child(
                widgets::list_item()
                    .child(
                        widgets::row()
                            .child(widgets::badge(
                                format!("#{}", show_value(&e["seq"])),
                                Tone::Muted,
                            ))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(show_value(&e["kind"])),
                            )
                            .child(widgets::note(project))
                            .child(widgets::note(logic::when(&e["atMs"]))),
                    )
                    .when(!data.is_empty(), |c| c.child(widgets::mono(data))),
            );
        }
        page = page.child(list);
        page
    }

    pub(super) fn diagnostics(&self, cx: &mut Context<Self>) -> Div {
        let mut page = div().flex().flex_col().gap_4();
        if !self.connected() {
            page = page.child(self.disconnected(cx));
        } else {
            let h = &self.snap.health;
            let backends = h["backendInventory"]
                .as_array()
                .map(|a| {
                    a.iter().fold(div().flex().flex_col(), |c, b| {
                        c.child(widgets::mono(show_value(b)))
                    })
                })
                .unwrap_or_else(|| div());
            page = page.child(
                widgets::panel()
                    .child(widgets::field(
                        "Instance",
                        widgets::mono(show_value(&h["daemonInstanceId"])),
                    ))
                    .child(widgets::field("Version", show_value(&h["daemonVersion"])))
                    .child(widgets::field(
                        "Process",
                        format!(
                            "pid {}, started {}",
                            show_value(&h["pid"]),
                            logic::when(&h["startedAtMs"])
                        ),
                    ))
                    .child(widgets::field("SQLite", show_value(&h["sqliteVersion"])))
                    .child(widgets::field(
                        "Data root",
                        widgets::mono(self.root.display().to_string()),
                    ))
                    .child(widgets::field(
                        "Event position",
                        format!("seq {}", show_value(&self.snap.events["seq"])),
                    ))
                    .child(widgets::field("Backends", backends))
                    .child(widgets::note(
                        "This view never shows the service credential.",
                    )),
            );
        }
        let busy = self.is_busy("diagnostics");
        let redact = self.diag_redact;
        let mut export = widgets::panel()
            .child(widgets::heading("Export diagnostics").mt_0())
            .child(widgets::note("One bounded JSON bundle: versions, service health, project state, recent jobs, ownership history, read-only index inspection and the tail of the service log. Never included: the service credential, source code, index contents. Your home folder is always shown as ~."))
            .child(self.clickable(
                widgets::toggle(
                    "redact",
                    "Also replace project paths and names with short hashes",
                    redact,
                    true,
                ),
                true,
                cx,
                |view, _, _| {
                    view.diag_redact = !view.diag_redact;
                },
            ))
            .child(
                widgets::row()
                    .child(self.cmd(
                        "diag-preview",
                        "Preview contents",
                        Kind::Plain,
                        "diagnostics",
                        Command::DiagnosticsPreview { redact },
                        cx,
                    ))
                    .child(self.btn("diag-export", if busy { "Working…" } else { "Export…" }, Kind::Primary, !busy, cx, |view, _, cx| {
                        let dir = std::env::var_os("USERPROFILE")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|| view.root.clone());
                        let picked = cx.prompt_for_new_path(&dir, Some("wrongindex-diagnostics.json"));
                        let redact = view.diag_redact;
                        cx.spawn(async move |view, cx| {
                            if let Ok(Ok(Some(path))) = picked.await {
                                let _ = view.update(cx, |view, cx| {
                                    view.send("diagnostics", Command::ExportDiagnostics { redact, path });
                                    cx.notify();
                                });
                            }
                        })
                        .detach();
                    })),
            );
        if let Some(cats) = self
            .snap
            .diagnostics
            .as_ref()
            .and_then(|d| d["categories"].as_array())
        {
            for c in cats {
                export = export.child(widgets::field(
                    show_value(&c["category"]),
                    format!("{:.1} KiB", c["bytes"].as_f64().unwrap_or(0.0) / 1024.0),
                ));
            }
        }
        page.child(export)
    }
}
