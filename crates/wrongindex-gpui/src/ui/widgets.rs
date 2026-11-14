//! Small building blocks shared by the views.

use gpui::{Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb};

use super::theme;
use crate::logic::Tone;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Plain,
}

/// A button. The caller attaches the click handler when it is enabled.
pub fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    kind: Kind,
    enabled: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let (bg, fg) = match kind {
        Kind::Primary => (theme::ACCENT, theme::BG),
        Kind::Plain => (theme::BORDER, theme::TEXT),
    };
    div()
        .id(id)
        .tab_index(0)
        .tab_stop(enabled)
        .flex_none()
        .border_1()
        .border_color(rgb(bg))
        .focus(|style| style.border_color(rgb(theme::TEXT)))
        .px_3()
        .py(px(6.))
        .rounded_md()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .when(enabled, |b| {
            b.cursor_pointer()
                .hover(|s| s.border_color(rgb(theme::MUTED)))
        })
        .when(!enabled, |b| b.opacity(0.45))
        .child(label.into())
}

/// One choice of a segmented control.
pub fn segment(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    active: bool,
    enabled: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .tab_index(0)
        .tab_stop(enabled)
        .px_3()
        .py(px(5.))
        .text_sm()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active {
            theme::SELECTED_BORDER
        } else {
            theme::BORDER
        }))
        .focus(|style| style.border_color(rgb(theme::ACCENT)))
        .bg(rgb(if active {
            theme::SELECTED
        } else {
            theme::PANEL
        }))
        .text_color(rgb(if active { theme::TEXT } else { theme::MUTED }))
        .when(enabled && !active, |b| b.cursor_pointer())
        .when(!enabled, |b| b.opacity(0.45))
        .child(label.into())
}

/// A switch with its label; the caller attaches the click handler.
pub fn toggle(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on: bool,
    enabled: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let label: SharedString = label.into();
    div()
        .id(id)
        .tab_index(0)
        .tab_stop(enabled)
        .flex()
        .items_center()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::PANEL))
        .focus(|style| style.border_color(rgb(theme::ACCENT)))
        .p_1()
        .when(enabled, |b| b.cursor_pointer())
        .when(!enabled, |b| b.opacity(0.45))
        .child(
            div()
                .w(px(36.))
                .h(px(20.))
                .flex_none()
                .rounded_full()
                .bg(rgb(if on { theme::ACCENT } else { theme::BORDER }))
                .flex()
                .items_center()
                .when(on, |d| d.justify_end())
                .px(px(3.))
                .child(div().size(px(14.)).rounded_full().bg(rgb(if on {
                    theme::BG
                } else {
                    theme::MUTED
                }))),
        )
        .child(div().text_sm().child(label))
}

pub fn panel() -> Div {
    div()
        .p_5()
        .rounded_lg()
        .bg(rgb(theme::PANEL))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .gap_2()
}

pub fn heading(text: impl Into<SharedString>) -> Div {
    div()
        .mt_2()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .child(text.into())
}

pub fn label(text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .text_color(rgb(theme::MUTED))
        .child(text.into())
}

pub fn note(text: impl Into<SharedString>) -> Div {
    div()
        .text_sm()
        .text_color(rgb(theme::MUTED))
        .child(text.into())
}

pub fn line(text: impl Into<SharedString>, tone: Tone) -> Div {
    div()
        .text_sm()
        .text_color(rgb(theme::tone(tone)))
        .child(text.into())
}

pub fn mono(text: impl Into<SharedString>) -> Div {
    div()
        .text_xs()
        .font_family("Cascadia Mono")
        .text_color(rgb(theme::MUTED))
        .child(text.into())
}

/// A summary number with its caption.
pub fn card(caption: impl Into<SharedString>, value: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w(px(130.))
        .p_4()
        .rounded_lg()
        .bg(rgb(theme::PANEL))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(label(caption))
        .child(
            div()
                .mt_2()
                .text_2xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(value.into()),
        )
}

/// A labelled value in a two-column list.
pub fn field(key: impl Into<SharedString>, value: impl IntoElement) -> Div {
    div()
        .flex()
        .gap_4()
        .py(px(1.))
        .child(
            div()
                .w(px(190.))
                .flex_none()
                .text_sm()
                .text_color(rgb(theme::MUTED))
                .child(key.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .text_sm()
                .child(value),
        )
}

pub fn badge(text: impl Into<SharedString>, tone: Tone) -> Div {
    div()
        .flex_none()
        .px_2()
        .py(px(2.))
        .rounded_md()
        .text_xs()
        .bg(rgb(theme::RAISED))
        .text_color(rgb(theme::tone(tone)))
        .child(text.into())
}

/// A message across the view: bad (red), warning or plain.
pub fn banner(tone: Tone) -> Div {
    let (bg, border) = match tone {
        Tone::Bad => (theme::BAD_BG, 0x6b3236),
        Tone::Warn => (theme::WARN_BG, 0x5c4a30),
        Tone::Good => (theme::GOOD_BG, 0x2f5546),
        _ => (theme::RAISED, theme::BORDER),
    };
    div()
        .p_4()
        .rounded_md()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .flex()
        .flex_col()
        .gap_2()
}

pub fn row() -> Div {
    div().flex().flex_wrap().items_center().gap_2()
}

/// A list row inside a panel.
pub fn list_item() -> Div {
    div()
        .p_3()
        .rounded_md()
        .bg(rgb(theme::RAISED))
        .flex()
        .flex_col()
        .gap_1()
}

pub fn empty(text: impl Into<SharedString>) -> Div {
    div()
        .p_6()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .text_sm()
        .text_color(rgb(theme::MUTED))
        .child(text.into())
}
