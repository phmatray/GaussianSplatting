//! The "Pièces" workspace: the gallery of filmed rooms, and an open room
//! with its 3D view and the strip of its versions.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icons::Icon;
use super::refine::{ConfirmDelete, JobCard, LastJob, RefineForm};
use crate::core::with_core;
use crate::logic::rooms::{self, Room, RoomTake};
use crate::state::{use_ui, TakePlace, Ui, Workspace};

/// The rooms of everything the server keeps, recomputed when a list changes.
fn all_rooms(ui: Ui) -> Vec<Room> {
    let takes: Vec<RoomTake> = ui.takes.with(|takes| {
        takes
            .iter()
            .filter(|t| t.place == TakePlace::Server && t.scene == "capture")
            .map(|t| RoomTake { id: t.id, title: t.title.clone(), file: t.file.clone(), bytes: t.bytes, duration_s: t.duration_s })
            .collect()
    });
    ui.captures.with(|captures| ui.refined.with(|refined| rooms::rooms(captures, refined, &takes)))
}

fn photo_url(file: &str) -> Option<String> {
    let mut url = None;
    with_core(|core| url = core.capture_photo_url(file));
    url
}

/// Shows one version of the open room: a refinement is downloaded and
/// drawn, the capture is ingested again, a recording is replayed.
pub fn open_version(ui: Ui, key: String) {
    let current = ui.version.get_untracked();
    if current.as_deref() == Some(key.as_str()) {
        return;
    }
    if current.is_some() {
        ui.compare.set(current);
    }
    ui.version.set(Some(key.clone()));
    if let Some(file) = key.strip_prefix("refined:") {
        with_core(|core| core.view_refined(file));
    } else if let Some(id) = key.strip_prefix("take:").and_then(|id| id.parse().ok()) {
        with_core(|core| core.replay_take(id));
    } else if let Some(file) = key.strip_prefix("capture:") {
        with_core(|core| core.open_capture(file));
    }
}

/// Opens a room, showing its best refinement when it has one.
fn enter_room(ui: Ui, room: &Room) {
    // The settings panel belongs to the live view.
    ui.panel_open.set(false);
    ui.room.set(Some(room.key.clone()));
    ui.version.set(None);
    ui.compare.set(None);
    let Some(best) = room.best() else { return };
    open_version(ui, format!("refined:{}", best.file));
    // Comparer works from the first view: against the next best result,
    // or the raw capture when there is only one.
    let runner_up = room
        .refined
        .iter()
        .filter(|r| r.file != best.file)
        .max_by(|a, b| a.psnr.unwrap_or(f64::MIN).partial_cmp(&b.psnr.unwrap_or(f64::MIN)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|r| format!("refined:{}", r.file))
        .or_else(|| room.capture.as_ref().map(|c| format!("capture:{}", c.file)));
    ui.compare.set(runner_up);
}

/// Leaves the shown version for the live reconstruction.
pub fn leave_versions(ui: Ui) {
    ui.room.set(None);
    ui.version.set(None);
    ui.compare.set(None);
    with_core(|core| {
        if core.is_replay() {
            core.close_replay();
        }
        core.return_to_live();
    });
}

#[component]
fn RoomCard(room: Room) -> impl IntoView {
    let ui = use_ui();
    let photo = room.capture.as_ref().and_then(|c| photo_url(&c.file));
    let failed = RwSignal::new(false);
    let refined = !room.refined.is_empty();
    let best = room.best().and_then(|r| r.psnr).map(rooms::score);
    let initial = room.title.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
    let meta = room.meta();
    let title = room.title.clone();
    let open = room.clone();
    view! {
        <li class="room-card" data-room=room.key.clone()>
            <button type="button" class="room-open" on:click=move |_| enter_room(ui, &open) aria-label=format!("Ouvrir «\u{a0}{title}\u{a0}»")>
                <span class="room-photo">
                    {match photo {
                        Some(src) => view! {
                            <Show when=move || !failed.get() fallback=move || view! { <span class="room-initial" aria-hidden="true">{initial.clone()}</span> }>
                                <img src=src.clone() alt="" loading="lazy" on:error=move |_| failed.set(true) />
                            </Show>
                        }
                        .into_any(),
                        None => view! { <span class="room-initial" aria-hidden="true">{initial.clone()}</span> }.into_any(),
                    }}
                </span>
                <span class="room-text">
                    <span class="room-title">{room.title.clone()}</span>
                    <span class="room-meta">{meta}</span>
                </span>
                <span class="room-score" class:none=best.is_none()>
                    {best.clone().unwrap_or_else(|| if refined { "affinée, sans score" } else { "pas encore affinée" }.to_string())}
                </span>
            </button>
        </li>
    }
}

/// A room known only by its results: one line, no photograph to show.
#[component]
fn OrphanRow(room: Room) -> impl IntoView {
    let ui = use_ui();
    let best = room.best().and_then(|r| r.psnr).map(rooms::score);
    let facts = room.best().map(rooms::refined_facts).unwrap_or_default();
    let open = room.clone();
    view! {
        <li>
            <button type="button" class="orphan" on:click=move |_| enter_room(ui, &open)>
                <span class="room-title">{room.title.clone()}</span>
                <span class="room-meta">{facts}</span>
                <span class="room-score" class:none=best.is_none()>{best.clone().unwrap_or_else(|| "sans score".to_string())}</span>
            </button>
        </li>
    }
}

/// The gallery: every room the server keeps.
#[component]
fn Gallery() -> impl IntoView {
    let ui = use_ui();
    let rooms = Memo::new(move |_| all_rooms(ui));
    let importing = move || ui.import.with(|i| i.as_ref().is_some_and(|i| !i.progress.done));
    let pick = move |ev: leptos::ev::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        if let Some(file) = input.files().and_then(|list| list.get(0)) {
            with_core(|core| core.import_capture(file));
        }
        input.set_value("");
    };
    let low = move || ui.storage.with(|storage| storage.as_ref().and_then(|storage| storage.low_text()));
    view! {
        <section class="gallery" id="gallery" aria-labelledby="gallery-title">
            <div class="gallery-head">
                <div>
                    <h2 id="gallery-title">"Pièces"</h2>
                    <p class="gallery-count">
                        {move || {
                            let n = rooms.with(|list| list.iter().filter(|r| r.capture.is_some()).count());
                            if n == 1 { "1 pièce filmée".to_string() } else { format!("{n} pièces filmées") }
                        }}
                    </p>
                </div>
                <Show when=move || ui.lan_admin.get()>
                    <label class="btn" class:disabled=importing for="gallery-import">
                        <Icon name="upload" />
                        "Importer un .gscap"
                    </label>
                    <input type="file" id="gallery-import" class="visually-hidden" accept=".gscap" disabled=importing on:change=pick />
                </Show>
            </div>
            <Show when=move || low().is_some()>
                <p class="warning" role="status">{low}</p>
            </Show>
            <Show when=move || ui.job.with(Option::is_some)>
                <div class="gallery-job"><JobCard /></div>
            </Show>
            <Show
                when=move || !rooms.with(Vec::is_empty)
                fallback=move || view! {
                    <div class="gallery-empty">
                        <Icon name="phone" />
                        <p>
                            {move || if ui.source.get() == Some(crate::logic::source::SourceKind::Worker) {
                                crate::logic::refine::ABSENT_HINT
                            } else {
                                "Aucune pièce pour l'instant. Filmez avec Splat Capture depuis l'onglet Direct, ou importez un fichier .gscap enregistré sur l'iPhone."
                            }}
                        </p>
                    </div>
                }
            >
                <ul class="room-grid" aria-label="Pièces filmées">
                    <For each=move || rooms.get().into_iter().filter(|r| r.capture.is_some()) key=|room| (room.key.clone(), room.versions(), room.best().and_then(|r| r.psnr).map(f64::to_bits)) let:room>
                        <RoomCard room=room />
                    </For>
                </ul>
                <Show when=move || rooms.with(|list| list.iter().any(|r| r.capture.is_none()))>
                    <h3 class="orphans-title">"Résultats dont la capture n'est pas sur cet ordinateur"</h3>
                    <ul class="orphans">
                        <For each=move || rooms.get().into_iter().filter(|r| r.capture.is_none()) key=|room| (room.key.clone(), room.versions()) let:room>
                            <OrphanRow room=room />
                        </For>
                    </ul>
                </Show>
            </Show>
        </section>
    }
}

/// One card of the version strip.
#[component]
fn VersionCard(key: String, label: String, facts: String, #[prop(optional)] score: Option<String>, #[prop(optional)] kind: &'static str) -> impl IntoView {
    let ui = use_ui();
    let selected = {
        let key = key.clone();
        Signal::derive(move || ui.version.with(|v| v.as_deref() == Some(key.as_str())))
    };
    let loading = {
        let key = key.clone();
        move || ui.refined_loading.with(|l| l.as_deref().is_some_and(|f| key.strip_prefix("refined:") == Some(f)))
    };
    let other = {
        let key = key.clone();
        Signal::derive(move || ui.compare.with(|c| c.as_deref() == Some(key.as_str())))
    };
    let open = key.clone();
    // The selected version is brought into view, also when the strip is
    // wider than the screen.
    let item: NodeRef<leptos::html::Li> = NodeRef::new();
    Effect::new(move |_| {
        if selected.get() {
            if let Some(li) = item.get() {
if let Some(strip) = li.parent_element() {
                    let strip_left = strip.get_bounding_client_rect().left();
                    let rect = li.get_bounding_client_rect();
                    let centre = rect.left() - strip_left + strip.scroll_left() - (f64::from(strip.client_width()) - rect.width()) / 2.0;
                    strip.set_scroll_left(centre.max(0.0));
                }
            }
        }
    });
    view! {
        <li class="version" node_ref=item class:on=selected class:other=other class:loading=loading data-kind=kind>
            <button type="button" class="version-open" aria-pressed=move || selected.get().to_string() on:click=move |_| open_version(ui, open.clone())>
                <span class="version-label">{label}</span>
                <span class="version-facts">{facts}</span>
                {score.map(|s| view! { <span class="version-score">{s}</span> })}
            </button>
        </li>
    }
}

/// What can be done with the version on show: download, share, delete.
#[component]
fn VersionActions(room: Room) -> impl IntoView {
    let ui = use_ui();
    let room = StoredValue::new(room);
    let shown = move || {
        let key = ui.version.get()?;
        let file = key.strip_prefix("refined:")?.to_string();
        room.with_value(|room| room.refined.iter().find(|r| r.file == file).cloned())
    };
    let capture = move || {
        let key = ui.version.get()?;
        key.strip_prefix("capture:").map(str::to_string)
    };
    view! {
        {move || capture().filter(|_| ui.lan_admin.get()).map(|file| view! {
            <div class="version-actions" role="group" aria-label="Actions sur la capture brute">
                <ConfirmDelete class="room-delete" what="la capture brute".to_string() on_delete=Callback::new(move |()| {
                    ui.version.set(None);
                    ui.room.set(None);
                    with_core(|core| {
                        core.return_to_live();
                        core.delete_capture(&file);
                    });
                }) />
            </div>
        })}
        {move || shown().map(|result| {
            let (download, share, delete) = (result.clone(), result.clone(), result.file.clone());
            let what = rooms::refined_label(&result);
            let group_label = format!("Actions sur «\u{a0}{what}\u{a0}»");
            let what = StoredValue::new(what);
            view! {
                <div class="version-actions" role="group" aria-label=group_label>
                    <button type="button" class="icon-btn" title="Télécharger le fichier .ply" aria-label="Télécharger" on:click=move |_| with_core(|core| core.download_refined(&download))>
                        <Icon name="download" />
                    </button>
                    <Show when=move || ui.lan_admin.get()>
                        <button type="button" class="icon-btn" title="Partager" aria-label="Partager" on:click={
                            let share = share.clone();
                            move |_| with_core(|core| core.share_refined(&share))
                        }>
                            <Icon name="send" />
                        </button>
                        <ConfirmDelete class="version-delete" what=what.get_value() on_delete={
                            let delete = delete.clone();
                            Callback::new(move |()| {
                                ui.version.set(None);
                                with_core(|core| core.delete_refined(&delete));
                            })
                        } />
                    </Show>
                </div>
            }
        })}
    }
}

/// An open room: what it is, what to do with it, and its versions.
#[component]
fn RoomView(room: Room) -> impl IntoView {
    let ui = use_ui();
    let adding = RwSignal::new(false);
    let capture_file = room.capture.as_ref().map(|c| c.file.clone());
    let refinable = room.capture.as_ref().is_some_and(|c| c.refinable()) && ui.lan_admin.get_untracked();
    let job_here = {
        let file = capture_file.clone();
        move || ui.job.with(|job| job.as_ref().is_some_and(|j| file.as_deref() == Some(j.capture.as_str())))
    };
    let compare_label = {
        let room = room.clone();
        Signal::derive(move || {
            let other = ui.compare.get()?;
            let name = match other.strip_prefix("refined:") {
                Some(file) => room.refined.iter().find(|r| r.file == file).map(rooms::refined_label),
                None if other.starts_with("capture:") => Some("Capture brute".to_string()),
                None => other
                    .strip_prefix("take:")
                    .and_then(|id| room.takes.iter().find(|t| t.id.to_string() == id))
                    .map(|t| t.label().to_lowercase()),
            }?;
            Some(name)
        })
    };
    let subtitle = room.capture.as_ref().map_or_else(
        || "Capture absente de cet ordinateur : seuls ses résultats sont là.".to_string(),
        |c| rooms::capture_facts(c),
    );
    let capture_file = StoredValue::new(capture_file);
    view! {
        <div class="room-view" id="room-view" data-room=room.key.clone()>
            <div class="room-info">
                <h2>{room.title.clone()}</h2>
                <p>{subtitle}</p>
            </div>
            <div class="room-tools">
                <VersionActions room=room.clone() />
                <button
                    type="button"
                    id="room-compare"
                    class="btn primary"
                    disabled=move || compare_label.get().is_none() || ui.refined_loading.with(Option::is_some)
                    title=move || compare_label.get().map_or_else(|| "Revenir à la version vue juste avant, pour comparer".to_string(), |name| format!("Comparer avec {name}"))
                    on:click=move |_| {
                        if let Some(other) = ui.compare.get_untracked() {
                            open_version(ui, other);
                        }
                    }
                >
                    <Icon name="loop" />
                    <span class="compare-text">
                        {move || if compare_label.get().is_some() { "Comparer" } else { "Comparer deux versions" }}
                    </span>
                </button>
            </div>
            <section class="room-strip" aria-label="Versions de cette pièce">
                <h3>"Versions de cette pièce"</h3>
                <ul class="versions">
                    {room.capture.clone().map(|c| view! {
                        <VersionCard key=format!("capture:{}", c.file) label="Capture brute".to_string() facts=format!("Fusion en direct · {}", crate::logic::replay::size(c.size)) kind="capture" />
                    })}
                    {room.refined.iter().map(|r| {
                        let best = room.refined.len() > 1 && room.best().is_some_and(|b| b.file == r.file && b.psnr.is_some());
                        let score = r.psnr.map(|p| if best { format!("{} · meilleure", rooms::score(p)) } else { rooms::score(p) }).unwrap_or_default();
                        view! { <VersionCard key=format!("refined:{}", r.file) label=rooms::refined_label(r) facts=rooms::refined_facts(r) score=score kind="refined" /> }
                    }).collect_view()}
                    {room.takes.iter().map(|t| view! {
                        <VersionCard key=format!("take:{}", t.id) label=t.label().to_string() facts=t.facts() kind="take" />
                    }).collect_view()}
                    <Show when=job_here.clone()>
                        <li class="version job"><JobCard /></li>
                    </Show>
                    <Show when=move || refinable && !job_here()>
                        <li class="version add">
                            <button type="button" class="version-open" aria-expanded=move || adding.get().to_string() on:click=move |_| adding.update(|a| *a = !*a)>
                                <Icon name="sparkle" />
                                <span class="version-label">"Nouvel affinage"</span>
                            </button>
                        </li>
                    </Show>
                </ul>
                <Show when=move || ui.job.with(Option::is_none) && ui.job_end.with(Option::is_some)>
                    <div class="room-last"><LastJob /></div>
                </Show>
            </section>
            <Show when=move || adding.get() && capture_file.with_value(Option::is_some)>
                <div class="room-refine" role="dialog" aria-label="Nouvel affinage">
                    <div class="room-refine-head">
                        <h3>"Nouvel affinage"</h3>
                        <button type="button" class="icon-btn" aria-label="Fermer" on:click=move |_| adding.set(false)><Icon name="close" /></button>
                    </div>
                    <RefineForm file=capture_file.get_value().unwrap_or_default() />
                </div>
            </Show>
        </div>
    }
}

/// The workspace itself: the gallery, or the open room.
#[component]
pub fn Rooms() -> impl IntoView {
    let ui = use_ui();
    let open = Memo::new(move |_| {
        let key = ui.room.get()?;
        all_rooms(ui).into_iter().find(|room| room.key == key)
    });
    view! {
        <Show when=move || ui.workspace.get() == Workspace::Rooms>
            {move || match open.get() {
                Some(room) => view! { <RoomView room=room /> }.into_any(),
                None => view! { <Gallery /> }.into_any(),
            }}
        </Show>
    }
}
