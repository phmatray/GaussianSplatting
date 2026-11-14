//! Reusable controls of the side panel.

use std::time::Duration;

use leptos::prelude::*;

use super::icons::Icon;
use crate::state::{use_ui, Ui};

/// How long a control keeps showing the value the user chose before it
/// falls back to the value the pipeline reports, when no State has
/// confirmed it.
const DRAFT_GRACE: Duration = Duration::from_millis(700);
const DRAFT_LIMIT: Duration = Duration::from_secs(5);

/// The value a user just chose for a control whose truth is the pipeline
/// configuration. It is shown at once, and dropped as soon as a State
/// message arrives after a short grace period: from then on the control
/// shows what the pipeline really uses, whoever changed it.
#[derive(Clone, Copy)]
struct Draft<T: Copy + Send + Sync + 'static> {
    ui: Ui,
    value: RwSignal<Option<T>>,
    /// Serial of the latest input and the State epoch it was made in.
    stamp: StoredValue<(u32, u32)>,
    /// The grace period of the latest input is over.
    settled: StoredValue<bool>,
}

impl<T: Copy + Send + Sync + 'static> Draft<T> {
    fn new() -> Self {
        let ui = use_ui();
        let draft = Self { ui, value: RwSignal::new(None), stamp: StoredValue::new((0, 0)), settled: StoredValue::new(true) };
        Effect::new(move |_| {
            let epoch = ui.state_epoch.get();
            if draft.settled.get_value() && draft.stamp.get_value().1 != epoch {
                draft.value.set(None);
            }
        });
        draft
    }

    fn get(&self) -> Option<T> {
        self.value.get()
    }

    fn set(&self, value: T) {
        let ui = self.ui;
        let draft = *self;
        let serial = self.stamp.get_value().0.wrapping_add(1);
        let epoch = ui.state_epoch.get_untracked();
        self.stamp.set_value((serial, epoch));
        self.settled.set_value(false);
        self.value.set(Some(value));
        // The control may be gone when the timers fire (its section was
        // replaced by the one of the other scene): hence the `try_` accessors.
        let latest = move || draft.stamp.try_get_value().is_some_and(|stamp| stamp.0 == serial);
        set_timeout(
            move || {
                if latest() {
                    draft.settled.try_set_value(true);
                    if ui.state_epoch.get_untracked() != epoch {
                        draft.value.try_set(None);
                    }
                }
            },
            DRAFT_GRACE,
        );
        set_timeout(
            move || {
                if latest() {
                    draft.value.try_set(None);
                }
            },
            DRAFT_LIMIT,
        );
    }
}

/// A collapsible section of the panel. The body stays in the document when
/// collapsed (the thumbnails keep their canvases), it is only hidden.
#[component]
pub fn Section(id: &'static str, title: &'static str, icon: &'static str, children: Children) -> impl IntoView {
    let open = RwSignal::new(true);
    view! {
        <section class="section" id=id class:collapsed=move || !open.get()>
            <button
                class="section-head"
                type="button"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|o| *o = !*o)
            >
                <Icon name=icon />
                <span class="section-title">{title}</span>
                <span class="section-chevron"><Icon name="chevron" /></span>
            </button>
            <div class="section-body">{children()}</div>
        </section>
    }
}

/// An on/off switch. With `remote`, the value belongs to the pipeline: the
/// switch flips at once and then follows the echoed configuration.
#[component]
pub fn Toggle(
    id: &'static str,
    label: &'static str,
    #[prop(into)] checked: Signal<bool>,
    on_change: Callback<bool>,
    #[prop(optional)] hint: Option<&'static str>,
    #[prop(optional)] remote: bool,
    #[prop(optional, into)] disabled: Signal<bool>,
) -> impl IntoView {
    let draft = remote.then(Draft::<bool>::new);
    let shown = move || draft.and_then(|d| d.get()).unwrap_or_else(|| checked.get());
    view! {
        <button
            class="toggle"
            type="button"
            role="switch"
            id=id
            aria-checked=move || shown().to_string()
            disabled=move || disabled.get()
            on:click=move |_| {
                let next = !shown();
                if let Some(draft) = draft {
                    draft.set(next);
                }
                on_change.run(next);
            }
        >
            <span class="toggle-text">
                <span class="toggle-label">{label}</span>
                {hint.map(|hint| view! { <span class="toggle-hint">{hint}</span> })}
            </span>
            <span class="switch" aria-hidden="true"></span>
        </button>
    }
}

/// A labelled slider with its value on the right. See [`Toggle`] for
/// `remote`.
#[component]
pub fn Slider(
    id: &'static str,
    label: &'static str,
    min: f64,
    max: f64,
    step: f64,
    #[prop(into)] value: Signal<f64>,
    on_input: Callback<f64>,
    format: fn(f64) -> String,
    #[prop(optional)] remote: bool,
) -> impl IntoView {
    let draft = remote.then(Draft::<f64>::new);
    let shown = move || draft.and_then(|d| d.get()).unwrap_or_else(|| value.get());
    view! {
        <label class="slider" for=id>
            <span class="slider-head">
                <span class="slider-label">{label}</span>
                <output class="slider-value" for=id>{move || format(shown())}</output>
            </span>
            <input
                type="range"
                id=id
                min=min
                max=max
                step=step
                prop:value=move || shown().to_string()
                on:input=move |ev| {
                    let Ok(next) = event_target_value(&ev).parse::<f64>() else { return };
                    if let Some(draft) = draft {
                        draft.set(next);
                    }
                    on_input.run(next);
                }
            />
        </label>
    }
}

/// A row of mutually exclusive choices. `selected` is the index of the
/// active one; ids are `{id}-{index}`.
#[component]
pub fn Segmented(
    id: &'static str,
    label: &'static str,
    options: Vec<&'static str>,
    #[prop(into)] selected: Signal<Option<usize>>,
    on_select: Callback<usize>,
    #[prop(optional)] remote: bool,
) -> impl IntoView {
    let draft = remote.then(Draft::<usize>::new);
    let shown = move || draft.and_then(|d| d.get()).or_else(|| selected.get());
    view! {
        <div class="field">
            <span class="field-label" id=format!("{id}-label")>{label}</span>
            <div class="segmented" role="radiogroup" aria-labelledby=format!("{id}-label")>
                {options
                    .into_iter()
                    .enumerate()
                    .map(|(index, text)| {
                        view! {
                            <button
                                type="button"
                                role="radio"
                                id=format!("{id}-{index}")
                                class:active=move || shown() == Some(index)
                                aria-checked=move || (shown() == Some(index)).to_string()
                                on:click=move |_| {
                                    if let Some(draft) = draft {
                                        draft.set(index);
                                    }
                                    on_select.run(index);
                                }
                            >
                                {text}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </div>
    }
}

/// A line of the statistics table.
#[component]
pub fn StatRow(id: &'static str, label: &'static str, #[prop(into)] value: Signal<String>) -> impl IntoView {
    view! {
        <div class="stat">
            <dt>{label}</dt>
            <dd id=id>{move || value.get()}</dd>
        </div>
    }
}
