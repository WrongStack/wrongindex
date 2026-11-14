//! The WrongIndex window. State comes from the service actor; every change
//! is a [`Command`] whose outcome the window shows. Nothing here blocks on
//! the service.

mod detail;
mod pages;
mod projects;
mod settings;
pub mod theme;
pub mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    App, Application, Bounds, ClickEvent, Context, Div, Entity, FocusHandle, FontWeight,
    PromptLevel, SharedString, Stateful, Subscription, Timer, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use serde_json::Value;
use wrongindex_ipc::DataRoot;

use crate::input::{self, Submitted, TextInput};
use crate::logic::{self, IndexFilter, SETTING_FIELDS, Tone};
use crate::model::{Command, Snapshot};
use crate::service::{self, Service};
use crate::system::Prefs;
use widgets::Kind;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Projects,
    Indexes,
    Activity,
    Diagnostics,
    Settings,
}

impl Tab {
    const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Projects,
        Tab::Indexes,
        Tab::Activity,
        Tab::Diagnostics,
        Tab::Settings,
    ];

    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Projects => "Projects",
            Tab::Indexes => "Indexes",
            Tab::Activity => "Activity",
            Tab::Diagnostics => "Diagnostics",
            Tab::Settings => "Settings",
        }
    }
}

/// The message shown at the bottom of the window for a few seconds.
struct Toast {
    text: String,
    bad: bool,
    at: Instant,
}

pub struct Desktop {
    service: Service,
    snap: Snapshot,
    version: u64,
    updated: Instant,
    tab: Tab,
    /// The project open in Projects (its detail view).
    selected: Option<String>,
    sequence: u64,
    /// Requests sent and not answered yet, by what they are about.
    inflight: HashMap<u64, String>,
    toast: Option<Toast>,
    root: PathBuf,
    prefs: Prefs,
    focus: FocusHandle,
    path_input: Entity<TextInput>,
    search_input: Entity<TextInput>,
    setting_inputs: Vec<Entity<TextInput>>,
    /// The settings revision the fields were last filled from.
    settings_revision: Option<u64>,
    register_policy: &'static str,
    diag_redact: bool,
    index_filter: IndexFilter,
    following_update: bool,
    last_update_poll: Instant,
    announced_update: bool,
    _subscriptions: Vec<Subscription>,
}

impl Desktop {
    fn new(root: DataRoot, daemon: PathBuf, cx: &mut Context<Self>) -> Self {
        let root_path = root.path().to_path_buf();
        let prefs = Prefs::load(&root_path);
        let service = service::spawn(root, daemon);
        cx.spawn(async move |view, cx| {
            loop {
                Timer::after(Duration::from_millis(100)).await;
                if view
                    .update(cx, |view, cx| {
                        if view.poll(cx) {
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
        let path_input = cx.new(|cx| TextInput::new(r"C:\path\to\checkout", cx));
        let search_input = cx.new(|cx| TextInput::new("Symbol name or text", cx));
        let setting_inputs: Vec<_> = SETTING_FIELDS
            .iter()
            .map(|_| cx.new(|cx| TextInput::new("", cx)))
            .collect();
        let subscriptions = vec![
            cx.subscribe(&path_input, |view, _, _: &Submitted, cx| {
                view.check_folder(cx);
            }),
            cx.subscribe(&search_input, |view, _, _: &Submitted, cx| {
                view.run_search(cx);
            }),
        ];
        let index_filter = IndexFilter::parse(&prefs.index_filter);
        let mut desktop = Self {
            service,
            snap: Snapshot::default(),
            version: 0,
            updated: Instant::now(),
            tab: Tab::Overview,
            selected: None,
            sequence: 0,
            inflight: HashMap::new(),
            toast: None,
            root: root_path,
            prefs,
            focus: cx.focus_handle(),
            path_input,
            search_input,
            setting_inputs,
            settings_revision: None,
            register_policy: "manual",
            diag_redact: true,
            index_filter,
            following_update: false,
            last_update_poll: Instant::now(),
            announced_update: false,
            _subscriptions: subscriptions,
        };
        desktop.send("update-last", Command::UpdateLast);
        desktop.auto_check(false);
        desktop
    }

    /// Take in what the actor reported since the last frame.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(outcome) = self.service.outcomes.try_recv() {
            self.inflight.remove(&outcome.sequence);
            if let Some(message) = outcome.message {
                self.show(message, !outcome.ok);
            }
            if let Some(id) = outcome.select {
                self.open_project(id, cx);
            }
            changed = true;
        }
        let version = self.service.shared.version();
        if version != self.version {
            self.version = version;
            self.snap = self.service.shared.read();
            self.updated = Instant::now();
            changed = true;
            if self.selected.is_some()
                && self.snap.connected
                && self.snap.detail.is_none()
                && !self.is_busy("select")
            {
                // The project was removed (here or elsewhere).
                self.selected = None;
            }
            self.after_update();
        }
        if let Some(t) = &self.toast {
            let keep = if t.bad { 8 } else { 4 };
            if t.at.elapsed() > Duration::from_secs(keep) {
                self.toast = None;
                changed = true;
            }
        }
        if self.following_update && self.last_update_poll.elapsed() > Duration::from_secs(2) {
            self.last_update_poll = Instant::now();
            self.send("update-last", Command::UpdateLast);
        }
        changed
    }

    fn after_update(&mut self) {
        let last = &self.snap.update_last;
        if self.following_update && last["running"] != true && !self.is_busy("update-start") {
            self.following_update = false;
        }
        if !self.announced_update && last.is_object() {
            self.announced_update = true;
            let at = last["atMs"].as_i64().unwrap_or(0);
            if at > 0
                && last["running"] != true
                && last["pending"] != true
                && at != self.prefs.seen_update_ms
                && logic::now_ms() - at < logic::DAY_MS
            {
                self.prefs.seen_update_ms = at;
                self.prefs.save(&self.root);
                if last["result"]["updated"] == true {
                    let to = last["result"]["to"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    self.show(format!("WrongIndex was updated to {to}."), false);
                } else if let Some(e) = last["error"].as_str() {
                    let e = e.to_string();
                    self.show(format!("The last update failed: {e}"), true);
                }
            }
        }
    }

    fn show(&mut self, text: impl Into<String>, bad: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            bad,
            at: Instant::now(),
        });
    }

    fn is_busy(&self, key: &str) -> bool {
        self.inflight.values().any(|k| k == key)
    }

    fn connected(&self) -> bool {
        self.snap.connected
    }

    /// Send a request about `key` (a key also disables its buttons until
    /// the outcome arrives).
    fn send(&mut self, key: &str, command: Command) {
        self.sequence += 1;
        if self.service.send(self.sequence, command) {
            self.inflight.insert(self.sequence, key.to_string());
        } else {
            self.show(
                "The window's request queue is busy. Wait for the service before retrying.",
                true,
            );
        }
    }

    fn go(&mut self, tab: Tab) {
        self.tab = tab;
        if tab != Tab::Projects && self.selected.is_some() {
            self.selected = None;
            self.send("select", Command::Select(None));
        }
        match tab {
            Tab::Settings => {
                self.send("autostart", Command::AutostartRead);
                self.send("update-last", Command::UpdateLast);
                self.settings_revision = None;
            }
            Tab::Indexes => {
                let fresh = logic::now_ms() - self.snap.machine.at_ms < 30_000;
                if !fresh && !self.is_busy("machine") {
                    self.send("machine", Command::ScanMachine);
                }
            }
            _ => {}
        }
    }

    fn open_project(&mut self, id: String, cx: &mut Context<Self>) {
        self.tab = Tab::Projects;
        if self.selected.as_deref() != Some(id.as_str()) {
            self.search_input.update(cx, |i, cx| i.set_text("", cx));
        }
        self.selected = Some(id.clone());
        self.send("select", Command::Select(Some(id)));
    }

    fn close_project(&mut self) {
        self.selected = None;
        self.send("select", Command::Select(None));
    }

    /// The daily update check, when turned on.
    fn auto_check(&mut self, force: bool) {
        if !self.prefs.auto_check_updates {
            return;
        }
        if !force && logic::now_ms() - self.prefs.last_check_ms < logic::DAY_MS {
            return;
        }
        self.prefs.last_check_ms = logic::now_ms();
        self.prefs.save(&self.root);
        self.send("update-check", Command::UpdateCheck { quiet: true });
    }

    /// A clickable control whose handler runs on this view.
    fn clickable(
        &self,
        el: Stateful<Div>,
        enabled: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        if !enabled {
            return el;
        }
        el.on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
            f(view, window, cx);
            cx.notify();
        }))
    }

    fn btn(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        kind: Kind,
        enabled: bool,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        self.clickable(widgets::button(id, label, kind, enabled), enabled, cx, f)
    }

    /// A button that sends one request; disabled while one about `key` runs.
    fn cmd(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        kind: Kind,
        key: &str,
        command: Command,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let enabled = !self.is_busy(key);
        let key = key.to_string();
        self.btn(id, label, kind, enabled, cx, move |view, _, _| {
            view.send(&key, command.clone())
        })
    }

    /// Ask first (a native dialog), then send. Cancel comes first, so it is
    /// the dialog's default button: a stray Enter never confirms.
    #[allow(clippy::too_many_arguments)]
    fn confirm_cmd(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        key: &str,
        title: String,
        body: String,
        ok: &'static str,
        command: Command,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let enabled = !self.is_busy(key);
        let key = key.to_string();
        self.btn(id, label, Kind::Plain, enabled, cx, move |_, window, cx| {
            let answer = window.prompt(
                PromptLevel::Warning,
                &title,
                Some(&body),
                &["Cancel", ok],
                cx,
            );
            let key = key.clone();
            let command = command.clone();
            cx.spawn(async move |view, cx| {
                if answer.await == Ok(1) {
                    let _ = view.update(cx, |view, cx| {
                        view.send(&key, command);
                        cx.notify();
                    });
                }
            })
            .detach();
        })
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut nav = div()
            .w(px(210.0))
            .flex_shrink_0()
            .h_full()
            .bg(rgb(theme::SIDEBAR))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .p_5()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .mb_4()
                    .child(
                        div()
                            .size_10()
                            .rounded_md()
                            .bg(rgb(theme::ACCENT))
                            .text_color(rgb(theme::BG))
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
            .child(widgets::label("WORKSPACE").mb_1());
        for tab in Tab::ALL {
            let active = self.tab == tab;
            let item = div()
                .id(tab.title())
                .tab_index(0)
                .border_1()
                .border_color(rgb(theme::SIDEBAR))
                .focus(|style| style.border_color(rgb(theme::ACCENT)))
                .px_3()
                .py_2()
                .rounded_md()
                .cursor_pointer()
                .bg(rgb(if active {
                    theme::BORDER
                } else {
                    theme::SIDEBAR
                }))
                .text_color(rgb(if active { theme::TEXT } else { theme::MUTED }))
                .hover(|s| s.text_color(rgb(theme::TEXT)))
                .child(tab.title());
            nav = nav.child(self.clickable(item, true, cx, move |view, _, _| view.go(tab)));
        }
        nav.child(div().flex_1())
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(format!(
                "Local by design.\nYour code stays here.\n\nv{}",
                env!("CARGO_PKG_VERSION")
            )))
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stale = self.snap.connected && self.updated.elapsed() > Duration::from_secs(8);
        let h = &self.snap.health;
        let (status, tone) = if !self.snap.connected {
            ("● Service not running".to_string(), Tone::Bad)
        } else if stale {
            ("● Reconnecting".into(), Tone::Warn)
        } else if h["draining"] == true {
            ("● Service stopping".into(), Tone::Warn)
        } else if h["paused"] == true {
            ("● Background indexing paused".into(), Tone::Warn)
        } else {
            ("● Service running".into(), Tone::Good)
        };
        let meta = if self.snap.connected {
            format!(
                "v{} · up {} · {} queued · {} running",
                h["daemonVersion"].as_str().unwrap_or("?"),
                logic::duration(&h["uptimeMs"]),
                h["queuedJobs"],
                h["runningJobs"]
            )
        } else {
            String::new()
        };
        let mut actions = widgets::row();
        if let Some(Ok(c)) = &self.snap.update_check
            && c["updateAvailable"] == true
        {
            actions = actions.child(self.btn(
                "update-available",
                format!("Update available: {}", c["latest"].as_str().unwrap_or("")),
                Kind::Plain,
                true,
                cx,
                |view, _, _| view.go(Tab::Settings),
            ));
        }
        actions = if self.snap.connected {
            actions.child(self.confirm_cmd(
                "stop-service",
                "Stop service…",
                "service",
                "Stop the index service?".into(),
                "Indexing and watching stop for all projects after the current work drains. Other clients lose access until it is started again. Registrations and indexes are kept.".into(),
                "Stop service",
                Command::Stop,
                cx,
            ))
        } else {
            actions.child(self.cmd(
                "start-service",
                if self.is_busy("service") {
                    "Starting…"
                } else {
                    "Start service"
                },
                Kind::Primary,
                "service",
                Command::Start,
                cx,
            ))
        };
        actions = actions.child(self.btn(
            "add-project",
            "+ Add project",
            Kind::Primary,
            true,
            cx,
            |view, _, cx| {
                view.go(Tab::Projects);
                view.close_project();
                view.browse(cx);
            },
        ));
        div()
            .flex()
            .items_start()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .min_w_0()
                    .child(widgets::label("CODEBASE INDEX SERVICE"))
                    .child(
                        div()
                            .mt_1()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.tab.title()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .flex()
                            .gap_3()
                            .text_sm()
                            .child(div().text_color(rgb(theme::tone(tone))).child(status))
                            .child(div().text_color(rgb(theme::MUTED)).child(meta)),
                    ),
            )
            .child(actions)
    }

    /// The native folder picker; the chosen folder is checked read-only.
    fn browse(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a project folder".into()),
        });
        cx.spawn(async move |view, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = view.update(cx, |view, cx| {
                    let text = path.display().to_string();
                    view.path_input.update(cx, |i, cx| i.set_text(text, cx));
                    view.send("preview", Command::Preview(path));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn check_folder(&mut self, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            self.show("Choose a folder or paste its path.", true);
        } else {
            self.send("preview", Command::Preview(PathBuf::from(text)));
        }
        cx.notify();
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.selected.clone() else {
            return;
        };
        let query = self.search_input.read(cx).text().to_string();
        match logic::search_term(&query) {
            Ok(_) => self.send("search", Command::Search { project, query }),
            Err(e) => self.show(e, true),
        }
        cx.notify();
    }

    /// Fill the settings fields from the applied values, unless typed in.
    fn sync_setting_inputs(&mut self, cx: &mut Context<Self>) {
        let revision = self.snap.settings["revision"].as_u64();
        if revision.is_none() || revision == self.settings_revision {
            return;
        }
        self.settings_revision = revision;
        let applied = self.snap.settings["settings"].clone();
        for (field, input) in SETTING_FIELDS.iter().zip(self.setting_inputs.clone()) {
            let value = applied[field.key]
                .as_u64()
                .map(|v| v.to_string())
                .unwrap_or_default();
            input.update(cx, |i, cx| i.set_text(value, cx));
        }
    }

    fn toast_view(&self) -> Option<Div> {
        let t = self.toast.as_ref()?;
        Some(
            widgets::banner(if t.bad { Tone::Bad } else { Tone::Good })
                .child(widgets::line(t.text.clone(), Tone::Normal)),
        )
    }
}

impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.tab == Tab::Settings {
            self.sync_setting_inputs(cx);
        }
        let body: gpui::AnyElement = match self.tab {
            Tab::Overview => self.overview(cx).into_any_element(),
            Tab::Projects => match &self.selected {
                Some(_) => self.detail(cx).into_any_element(),
                None => self.projects(cx).into_any_element(),
            },
            Tab::Indexes => self.indexes(cx).into_any_element(),
            Tab::Activity => self.activity().into_any_element(),
            Tab::Diagnostics => self.diagnostics(cx).into_any_element(),
            Tab::Settings => self.settings(cx).into_any_element(),
        };
        let content = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(div().px_7().pt_6().pb_4().child(self.header(cx)))
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_7()
                    .pb_6()
                    .child(div().flex().flex_col().gap_4().child(body)),
            )
            .children(self.toast_view().map(|t| div().px_7().pb_3().child(t)))
            .child(
                div()
                    .px_7()
                    .pb_3()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child("Tab / Shift+Tab to move · Enter / Space to activate · Closing this window leaves indexing running."),
            );
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
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT))
            .font_family("Segoe UI")
            .text_base()
            .flex()
            .child(self.sidebar(cx))
            .child(content)
    }
}

/// Open the window. Closing it ends this program only; the service and its
/// watchers keep running.
pub fn run(root: DataRoot, daemon: PathBuf) {
    Application::new().run(move |cx: &mut App| {
        input::bind_keys(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(900.0), px(600.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("WrongIndex".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Desktop::new(root, daemon, cx));
                window.focus(&view.read(cx).focus);
                view
            },
        );
        if let Err(e) = opened {
            eprintln!("WrongIndex: cannot open the window: {e}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

/// `value` as text, or "—".
fn show_value(v: &Value) -> String {
    match v {
        Value::Null => "—".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
