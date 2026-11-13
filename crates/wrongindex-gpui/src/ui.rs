use crate::model::{Command, Snapshot, readable_bytes};
use crate::service::{self, Service};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, FontWeight, PathPromptOptions, SharedString,
    Timer, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wrongindex_contracts::Freshness;
use wrongindex_ipc::DataRoot;

const BG: u32 = 0x10151b;
const PANEL: u32 = 0x171e27;
const BORDER: u32 = 0x293341;
const TEXT: u32 = 0xe7edf5;
const MUTED: u32 = 0x91a0b4;
const ACCENT: u32 = 0xeab875;
const GREEN: u32 = 0x74d3ab;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Projects,
    Activity,
    Settings,
}

struct Desktop {
    service: Service,
    snapshot: Snapshot,
    selected: Option<String>,
    tab: Tab,
    root: String,
    sequence: u64,
    pending: Option<u64>,
    updated: Instant,
    stale: bool,
    focus: FocusHandle,
}

impl Desktop {
    fn new(root: DataRoot, daemon: PathBuf, cx: &mut Context<Self>) -> Self {
        let root_label = root.path().display().to_string();
        let service = service::spawn(root, daemon);
        cx.spawn(async move |view, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if view
                    .update(cx, |view, cx| {
                        if view.poll() {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            service,
            snapshot: Snapshot::default(),
            selected: None,
            tab: Tab::Overview,
            root: root_label,
            sequence: 0,
            pending: None,
            updated: Instant::now(),
            stale: false,
            focus: cx.focus_handle(),
        }
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(snapshot) = self.service.updates.try_recv() {
            if self.pending.is_some_and(|p| snapshot.acknowledged >= p) {
                self.pending = None;
            }
            self.snapshot = snapshot;
            self.updated = Instant::now();
            changed = true;
        }
        let stale = self.snapshot.connected && self.updated.elapsed() > Duration::from_secs(5);
        changed |= stale != self.stale;
        self.stale = stale;
        if self.selected.as_ref().is_some_and(|id| {
            !self
                .snapshot
                .projects
                .iter()
                .any(|p| &p.project.project_id == id)
        }) {
            self.selected = None;
        }
        if self.selected.is_none()
            && let Some(project) = self.snapshot.projects.first()
        {
            let id = project.project.project_id.clone();
            self.selected = Some(id.clone());
            self.send(Command::Select(id));
        }
        changed
    }

    fn send(&mut self, command: Command) {
        if self.pending.is_some() {
            return;
        }
        self.sequence += 1;
        match self.service.commands.try_send((self.sequence, command)) {
            Ok(()) => self.pending = Some(self.sequence),
            Err(_) => {
                self.snapshot.notice =
                    Some("The client queue is busy. Wait for the service before retrying.".into())
            }
        }
    }

    fn button(
        &self,
        label: &'static str,
        command: Command,
        accent: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(label)
            .tab_index(0)
            .tab_stop(self.pending.is_none())
            .border_1()
            .border_color(rgb(BORDER))
            .focus(|style| style.border_color(rgb(TEXT)))
            .px_4()
            .py_2()
            .rounded_md()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .bg(rgb(if accent { ACCENT } else { BORDER }))
            .text_color(rgb(if accent { BG } else { TEXT }))
            .cursor_pointer()
            .when(self.pending.is_some(), |e| e.opacity(0.45))
            .on_click(cx.listener(move |view, _, _, cx| {
                view.send(command.clone());
                cx.notify();
            }))
            .child(label)
    }

    fn add_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("add-project")
            .tab_index(0)
            .tab_stop(self.pending.is_none())
            .border_1()
            .border_color(rgb(BORDER))
            .focus(|style| style.border_color(rgb(TEXT)))
            .px_4()
            .py_2()
            .rounded_md()
            .bg(rgb(ACCENT))
            .text_color(rgb(BG))
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .on_click(cx.listener(|view, _, _, cx| {
                if view.pending.is_some() {
                    return;
                }
                let selected = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Choose a project folder".into()),
                });
                cx.spawn(async move |view, cx| {
                    if let Ok(Ok(Some(paths))) = selected.await
                        && let Some(path) = paths.into_iter().next()
                    {
                        let _ = view.update(cx, |view, cx| {
                            view.send(Command::Preview(path));
                            cx.notify();
                        });
                    }
                })
                .detach();
            }))
            .child("+ Add project")
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut nav = div()
            .w(px(205.0))
            .flex_shrink_0()
            .h_full()
            .bg(rgb(0x121922))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_5()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size_10()
                            .rounded_md()
                            .bg(rgb(ACCENT))
                            .text_color(rgb(BG))
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_weight(FontWeight::BOLD)
                            .child("WI"),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_lg()
                            .child("WrongIndex"),
                    ),
            )
            .child(div().text_xs().text_color(rgb(MUTED)).child("WORKSPACE"));
        for (label, tab) in [
            ("Overview", Tab::Overview),
            ("Projects", Tab::Projects),
            ("Activity", Tab::Activity),
            ("Settings", Tab::Settings),
        ] {
            nav = nav.child(
                div()
                    .id(label)
                    .tab_index(0)
                    .border_1()
                    .border_color(rgb(0x121922))
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .px_3()
                    .py_3()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if self.tab == tab { BORDER } else { 0x121922 }))
                    .text_color(rgb(if self.tab == tab { TEXT } else { MUTED }))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.tab = tab;
                        cx.notify();
                    }))
                    .child(label),
            );
        }
        nav.child(div().flex_1()).child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child("Local by design.\nYour code stays here."),
        )
    }

    fn summary(&self) -> impl IntoElement {
        let watching = self
            .snapshot
            .projects
            .iter()
            .filter(|p| p.watch_health == "native" || p.watch_health == "polling")
            .count();
        let online = self.snapshot.connected && !self.stale;
        let queue = self.snapshot.metrics["queue"]["queued"].as_u64();
        let running = self.snapshot.health["runningJobs"].as_u64();
        let cards = [
            (
                "REGISTERED PROJECTS",
                if online {
                    self.snapshot.projects.len().to_string()
                } else {
                    "—".into()
                },
            ),
            (
                "WATCHING",
                if online {
                    watching.to_string()
                } else {
                    "—".into()
                },
            ),
            (
                "QUEUED JOBS",
                if online {
                    queue.map_or("—".into(), |v| v.to_string())
                } else {
                    "—".into()
                },
            ),
            (
                "RUNNING JOBS",
                if online {
                    running.map_or("—".into(), |v| v.to_string())
                } else {
                    "—".into()
                },
            ),
        ];
        div()
            .flex()
            .gap_4()
            .children(cards.into_iter().map(|(label, value)| {
                div()
                    .flex_1()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(div().text_xs().text_color(rgb(MUTED)).child(label))
                    .child(
                        div()
                            .mt_3()
                            .text_3xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(value),
                    )
            }))
    }

    fn projects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("project-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        for project in &self.snapshot.projects {
            let id = project.project.project_id.clone();
            let selected = self.selected.as_ref() == Some(&id);
            let state = format!("{:?}", project.freshness);
            list = list.child(
                div()
                    .id(SharedString::from(id.clone()))
                    .tab_index(0)
                    .tab_stop(self.pending.is_none())
                    .focus(|style| style.border_color(rgb(ACCENT)))
                    .p_4()
                    .rounded_md()
                    .bg(rgb(if selected { 0x263241 } else { PANEL }))
                    .border_1()
                    .border_color(rgb(if selected { 0x60758e } else { BORDER }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if view.pending.is_none() {
                            view.selected = Some(id.clone());
                            view.send(Command::Select(id.clone()));
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(project.project.display_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(if project.freshness == Freshness::Current {
                                        GREEN
                                    } else {
                                        ACCENT
                                    }))
                                    .child(state),
                            ),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(project.project.checkout_root.clone()),
                    )
                    .child(div().mt_2().text_xs().text_color(rgb(MUTED)).child(format!(
                        "{} · {} · generation {}",
                        project.project.watch_policy.as_str(),
                        project.project.backend.as_str(),
                        project.generation
                    ))),
            );
        }
        if self.snapshot.projects.is_empty() {
            list = list.child(div().p_8().text_color(rgb(MUTED)).child(if self.snapshot.connected { "No projects yet. Add a folder to preview its index location." } else { "Connect to the service to see your projects. Existing indexes are preserved." }));
        }
        let detail = self
            .snapshot
            .projects
            .iter()
            .find(|p| self.selected.as_ref() == Some(&p.project.project_id));
        let mut panel = div()
            .id("project-detail")
            .w(px(335.0))
            .flex_shrink_0()
            .min_h_0()
            .overflow_y_scroll()
            .p_5()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .gap_4();
        if let Some(project) = detail {
            let id = project.project.project_id.clone();
            let stats = if self.snapshot.stats_project.as_deref() == Some(id.as_str()) {
                Some(&self.snapshot.stats)
            } else {
                None
            };
            panel = panel.child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(project.project.display_name.clone()))
                .child(div().text_sm().text_color(rgb(MUTED)).child(format!("{} · {}", project.activity, project.watch_health)))
                .child(div().text_xs().text_color(rgb(MUTED)).child("INDEX LOCATION"))
                .child(div().text_xs().child(project.project.index_dir.clone()))
                .child(div().flex().gap_4().child(div().text_sm().child(format!("Files  {}", stats.and_then(|s| s["totalFiles"].as_u64()).map_or("—".into(), |v| v.to_string())))).child(div().text_sm().child(format!("Symbols  {}", stats.and_then(|s| s["totalSymbols"].as_u64()).map_or("—".into(), |v| v.to_string())))))
                .child(self.button("Index now", Command::Index { project: id.clone(), kind: "full" }, true, cx))
                .child(self.button("Reconcile", Command::Index { project: id.clone(), kind: "reconcile" }, false, cx))
                .child(self.button("Check integrity", Command::Index { project: id.clone(), kind: "check" }, false, cx))
                .child(self.button(if project.project.watch_policy.as_str() == "pinned" { "Use manual policy" } else { "Pin and watch" }, Command::Policy { project: id, policy: if project.project.watch_policy.as_str() == "pinned" { "manual" } else { "pinned" } }, false, cx))
                .child(div().text_xs().text_color(rgb(MUTED)).child("Indexing claims exclusive ownership. Unknown profiles and other writers are refused."));
            if let Some(error) = &project.last_error {
                panel = panel.child(div().text_sm().text_color(rgb(ACCENT)).child(error.clone()));
            }
        } else {
            panel = panel.child(
                div()
                    .text_color(rgb(MUTED))
                    .child("Select a project to inspect its live state and actions."),
            );
        }
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .gap_5()
            .child(list)
            .child(panel)
    }

    fn preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut box_ = div().flex().flex_col().gap_3();
        if let Some(preview) = &self.snapshot.preview {
            box_ = box_
                .p_5()
                .rounded_lg()
                .border_1()
                .border_color(rgb(ACCENT))
                .bg(rgb(PANEL))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Register this project"),
                )
                .child(div().text_sm().child(preview.root.display().to_string()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(format!("Index: {}", preview.index_dir)),
                )
                .child(div().text_xs().text_color(rgb(MUTED)).child(format!(
                    "Read-only inspection: {}. No source or index data has been changed.",
                    preview.verdict
                )))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .child(self.button(
                            "Register manual",
                            Command::Register {
                                root: preview.root.clone(),
                                index_dir: preview.index_dir.clone(),
                                pinned: false,
                            },
                            true,
                            cx,
                        ))
                        .child(self.button(
                            "Register and watch",
                            Command::Register {
                                root: preview.root.clone(),
                                index_dir: preview.index_dir.clone(),
                                pinned: true,
                            },
                            false,
                            cx,
                        ))
                        .child(self.button("Cancel", Command::DismissPreview, false, cx)),
                );
        }
        box_
    }
}

impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.snapshot.connected && !self.stale;
        let title = match self.tab {
            Tab::Overview => "Overview",
            Tab::Projects => "Projects",
            Tab::Activity => "Activity",
            Tab::Settings => "Settings",
        };
        let mut content = div()
            .flex_1()
            .min_w_0()
            .p_7()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child("CODEBASE INDEX SERVICE"),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(if connected { GREEN } else { ACCENT }))
                                    .child(if connected {
                                        "● Connected"
                                    } else if self.stale {
                                        "● Reconnecting"
                                    } else {
                                        "● Offline"
                                    }),
                            )
                            .child(self.add_button(cx)),
                    ),
            )
            .child(self.preview(cx));
        if let Some(error) = &self.snapshot.error {
            content = content.child(
                div()
                    .p_4()
                    .rounded_md()
                    .bg(rgb(0x30281f))
                    .text_sm()
                    .child(error.clone())
                    .child(div().mt_3().child(self.button(
                        "Start service",
                        Command::Start,
                        true,
                        cx,
                    ))),
            );
        }
        if let Some(notice) = &self.snapshot.notice {
            content = content.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(BORDER))
                    .text_sm()
                    .child(notice.clone()),
            );
        }
        content = match self.tab {
            Tab::Overview | Tab::Projects => content
                .child(self.summary())
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Your projects"),
                        )
                        .child(self.button("Refresh", Command::Refresh, false, cx)),
                )
                .child(self.projects(cx)),
            Tab::Activity => {
                let mut jobs = div()
                    .id("job-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3();
                for job in &self.snapshot.jobs {
                    jobs = jobs.child(
                        div()
                            .p_4()
                            .rounded_md()
                            .bg(rgb(PANEL))
                            .child(format!(
                                "{}   ·   {}",
                                job["kind"].as_str().unwrap_or("job"),
                                job["state"].as_str().unwrap_or("unknown")
                            ))
                            .child(
                                div()
                                    .mt_2()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(job["jobId"].as_str().unwrap_or("").to_string()),
                            ),
                    );
                }
                content.child(jobs)
            }
            Tab::Settings => {
                let paused = self.snapshot.settings["settings"]["paused"]
                    .as_bool()
                    .unwrap_or(false);
                content.child(div().p_6().rounded_lg().bg(rgb(PANEL)).flex().flex_col().gap_4().child(div().text_lg().child("Background indexing"))
                    .child(div().text_sm().text_color(rgb(MUTED)).child("Pausing retains the service, registrations and committed indexes. Resuming reconciles changes."))
                    .child(self.button(if paused { "Resume background indexing" } else { "Pause background indexing" }, Command::Pause(!paused), true, cx))
                    .child(div().mt_5().text_xs().text_color(rgb(MUTED)).child("SERVICE DATA LOCATION")).child(div().text_sm().child(self.root.clone()))
                    .child(div().text_sm().text_color(rgb(MUTED)).child(format!("Effective parallel jobs: {}", self.snapshot.metrics["limits"]["effectiveParallelJobs"])))
                    .child(div().text_sm().text_color(rgb(MUTED)).child(format!("Native run memory cap: {}", self.snapshot.metrics["limits"]["nativeMemoryCapBytes"].as_u64().map_or("Unavailable".into(), readable_bytes)))))
            }
        };
        div()
            .id("desktop")
            .track_focus(&self.focus)
            .tab_group()
            .tab_stop(false)
            .on_key_down(|event, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function
                {
                    if modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                }
            })
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .text_base()
            .flex()
            .child(self.sidebar(cx))
            .child(
                content.child(div().mt_2().text_xs().text_color(rgb(MUTED)).child(
                    "Tab / Shift+Tab to navigate · Enter / Space to activate. Closing this window leaves indexing running.",
                )),
            )
    }
}

pub fn run(root: DataRoot, daemon: PathBuf) {
    Application::new().run(move |cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1260.0), px(850.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("WrongIndex — Native".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Desktop::new(root, daemon, cx));
                window.focus(&view.read(cx).focus);
                view
            },
        )
        .expect("Unable to open the native WrongIndex window");
        cx.activate(true);
    });
}
