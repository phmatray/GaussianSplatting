//! The side panel and its sections.

use leptos::prelude::*;

use super::sections::{CamerasSection, CaptureSection, DisplaySection, FileSection, StatsSection, VrSection};
use crate::state::use_ui;

#[component]
pub fn Panel() -> impl IntoView {
    let ui = use_ui();
    view! {
        <aside class="panel" id="panel" class:open=move || ui.panel_open.get() aria-label="Réglages">
            <div class="panel-scroll">
                <CaptureSection />
                <DisplaySection />
                <CamerasSection />
                <StatsSection />
                <VrSection />
                <FileSection />
                <p class="panel-foot">"Raccourcis\u{a0}: H masque l'interface, P met en pause."</p>
            </div>
        </aside>
    }
}
