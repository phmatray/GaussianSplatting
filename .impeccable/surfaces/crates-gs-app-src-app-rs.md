---
version: 1
slug: "crates-gs-app-src-app-rs"
primary_target: "crates/gs-app/src/app.rs"
related_targets: ["crates/gs-app/style.css","crates/gs-app/src/ui"]
---

# Splat Live server interface (gs-app)

Scope: the desktop and web interface served by gs-server (`crates/gs-app`, Leptos). Mode: Operate.

Audience and job: the owner alone for now, technical vocabulary accepted. Two jobs of equal weight: watch a live capture from the iPhone, and find, open, compare and refine past captures. Files are organised by filmed room: a capture is the root, its refinements and recordings hang under it.

Constraints: the existing visual world stays (night ground, glass panels, blue/violet accent, Inter/system type). French UI text. The 3D canvas stays full-window under the interface.

## Direction contract

THESIS: the server is a gallery of filmed rooms, each opening onto its 3D view and the strip of its versions; it refuses the single long control column where files, settings and the live view compete.

OWN-WORLD: night ground #0a0d13, glass cards rgba(15,19,28,.84) with 9 % white hairlines, accent #5cc8ff for the selected version and the primary action, green #34d399 for quality scores, violet as the brand gradient only. Room cards carry a real photograph taken from the capture.

STORY: the owner sees every room he filmed at a glance, opens one, sees which version is best by its held-out score, views it, compares two versions, starts a new refinement, or goes back to Direct to film.

FIRST VIEWPORT: top bar with the logo, Direct | Pièces tabs, breadcrumb and phone status. Pièces home: a grid of room cards (photo, title, mode, images, size, best score, versions count). An opened room: the 3D view across the top two thirds, an info card top left, actions top right (Comparer deux versions, Partager, Affiner…), and a bottom strip "Versions de cette pièce": capture brute → each refinement with its score, the selected one outlined in accent, "+ Nouvel affinage" at the end.

FORM: Galerie de pièces, third of seven on my ordered list, dealt by seed:a8eecc01dd993e74.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance

Signature interaction: the version strip; selecting a version swaps the model in the view, and Comparer flips between two chosen versions in place (A/B) without leaving the room.
