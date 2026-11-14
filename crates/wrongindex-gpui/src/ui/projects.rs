//! Project list, adding a project and the read-only preview before it is
//! registered.

use gpui::{Context, Div, FontWeight, SharedString, div, prelude::*, px, rgb};
use serde_json::Value;
use wrongindex_contracts::WatchPolicy;

use super::widgets::{self, Kind};
use super::{Desktop, theme};
use crate::logic::{self, Tone};
use crate::model::Command;

impl Desktop {
    /// Projects as clickable rows that open their detail view.
    pub(super) fn project_list(&self, ps: &[Value], prefix: &str, cx: &mut Context<Self>) -> Div {
        let mut list = div().flex().flex_col().gap_2();
        for p in ps {
            let rec = &p["project"];
            let id = rec["projectId"].as_str().unwrap_or_default().to_string();
            let freshness = p["freshness"].as_str().unwrap_or("unknown");
            let error = p["lastError"].as_str();
            let row = div()
                .id(SharedString::from(format!("{prefix}-{id}")))
                .tab_index(0)
                .p_4()
                .rounded_md()
                .bg(rgb(theme::PANEL))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .focus(|s| s.border_color(rgb(theme::ACCENT)))
                .hover(|s| s.border_color(rgb(theme::SELECTED_BORDER)))
                .cursor_pointer()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .min_w_0()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(rec["displayName"].as_str().unwrap_or("?").to_string()),
                        )
                        .child(
                            widgets::row()
                                .flex_none()
                                .when(error.is_some(), |r| {
                                    r.child(widgets::badge("⚠ error", Tone::Bad))
                                })
                                .child(widgets::badge(
                                    logic::ownership_label(p["ownership"].as_str().unwrap_or("")),
                                    Tone::Muted,
                                ))
                                .child(widgets::badge(
                                    logic::freshness_label(freshness),
                                    logic::freshness_tone(freshness),
                                )),
                        ),
                )
                .child(widgets::mono(
                    rec["checkoutRoot"].as_str().unwrap_or("").to_string(),
                ))
                .child(widgets::note(format!(
                    "{} · {} · {} · generation {} · {} pending · last indexed {}",
                    rec["watchPolicy"].as_str().unwrap_or(""),
                    p["activity"].as_str().unwrap_or(""),
                    p["watchHealth"].as_str().unwrap_or(""),
                    p["generation"],
                    p["pendingPaths"],
                    logic::ago(&p["lastIndexedMs"]),
                )));
            list = list.child(self.clickable(row, true, cx, move |view, _, cx| {
                view.open_project(id.clone(), cx)
            }));
        }
        list
    }

    pub(super) fn disconnected(&self, cx: &mut Context<Self>) -> Div {
        widgets::banner(Tone::Warn)
            .child(widgets::line(
                "The WrongIndex service is not running.",
                Tone::Normal,
            ))
            .child(widgets::note(self.snap.reason.clone().unwrap_or_default()))
            .child(widgets::note(
                "No live values are shown while disconnected. Registrations and indexes are kept.",
            ))
            .child(widgets::row().child(self.cmd(
                "start-service-inline",
                if self.is_busy("service") {
                    "Starting…"
                } else {
                    "Start service"
                },
                Kind::Primary,
                "service",
                Command::Start,
                cx,
            )))
    }

    pub(super) fn projects(&self, cx: &mut Context<Self>) -> Div {
        let mut page = div().flex().flex_col().gap_4();
        if !self.connected() {
            page = page.child(self.disconnected(cx));
        }
        let add = widgets::panel()
            .child(widgets::heading("Add project").mt_0())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.btn("browse", "Browse…", Kind::Primary, true, cx, |view, _, cx| {
                        view.browse(cx)
                    }))
                    .child(div().flex_1().min_w_0().child(self.path_input.clone()))
                    .child(self.btn(
                        "check-folder",
                        "Check folder",
                        Kind::Plain,
                        !self.is_busy("preview"),
                        cx,
                        |view, _, cx| view.check_folder(cx),
                    )),
            )
            .child(widgets::note(
                "Choose a folder or paste its path. Checking is read-only: it resolves the WrongStack index location and inspects any existing index without changing it.",
            ));
        page = page.child(add).children(self.preview_panel(cx));
        let ps = self.snap.projects();
        page = page.child(widgets::heading(format!("Registered ({})", ps.len())));
        if ps.is_empty() {
            page.child(widgets::empty(if self.connected() {
                "No projects are registered yet."
            } else {
                "Start the service to see your projects."
            }))
        } else {
            page.child(self.project_list(ps, "projects", cx))
        }
    }

    fn preview_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        let pv = self.snap.preview.as_ref()?;
        let ins = &pv.inspection;
        let verdict = ins["verdict"].as_str().unwrap_or("unknown");
        let admissible = ins["writeAdmissible"] == true;
        let (existing, tone) = if verdict == "missing" {
            (
                "No index yet — the first index run will create it.".to_string(),
                Tone::Normal,
            )
        } else if admissible {
            (format!("Existing index: {verdict} (reusable)"), Tone::Good)
        } else {
            (
                format!("Existing index: {verdict} (not writable by this version)"),
                Tone::Bad,
            )
        };
        let root = pv.resolved["projectRoot"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| pv.path.display().to_string());
        let index_dir = pv.resolved["indexDir"].as_str().unwrap_or("").to_string();
        let mut policies = widgets::row();
        for (value, label) in [
            ("manual", "Manual"),
            ("session", "Session"),
            ("pinned", "Pinned (always watch)"),
        ] {
            let active = self.register_policy == value;
            policies = policies.child(self.clickable(
                widgets::segment(format!("reg-policy-{value}"), label, active, true),
                !active,
                cx,
                move |view, _, _| view.register_policy = value,
            ));
        }
        let policy = WatchPolicy::parse(self.register_policy).unwrap_or(WatchPolicy::Manual);
        Some(
            widgets::panel()
                .border_color(rgb(theme::ACCENT))
                .child(widgets::heading("Register this project").mt_0())
                .child(widgets::field("Checkout", widgets::mono(root.clone())))
                .child(widgets::field("Index directory", widgets::mono(index_dir.clone())))
                .child(widgets::field(
                    "Linked worktree",
                    if pv.resolved["linkedWorktree"].is_null()
                        || pv.resolved["linkedWorktree"] == false
                    {
                        "no"
                    } else {
                        "yes"
                    },
                ))
                .child(widgets::field("Index", widgets::line(existing, tone)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(widgets::note("Watch policy"))
                        .child(policies),
                )
                .child(widgets::note(
                    logic::policy_label(self.register_policy),
                ))
                .child(
                    widgets::row()
                        .child(self.cmd(
                            "register",
                            "Register project",
                            Kind::Primary,
                            "register",
                            Command::Register {
                                root,
                                index_dir,
                                policy,
                            },
                            cx,
                        ))
                        .child(self.cmd(
                            "dismiss-preview",
                            "Cancel",
                            Kind::Plain,
                            "preview",
                            Command::DismissPreview,
                            cx,
                        )),
                )
                .child(
                    widgets::note("Read-only inspection: no source or index data has been changed. Registering does not index yet; claim ownership in the project view. Indexing work is shown as jobs.")
                        .max_w(px(900.)),
                ),
        )
    }
}
