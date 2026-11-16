---
name: Splat Live (web and desktop interface)
description: Night-glass control surface floating over a full-window Gaussian-splat view; a gallery of filmed rooms and the strip of their versions.
colors:
  night-ground: "#0a0d13"
  night-glass: "rgba(15, 19, 28, 0.84)"
  night-glass-strong: "rgba(12, 15, 23, 0.94)"
  dialog-slate: "#10141d"
  hairline: "rgba(255, 255, 255, 0.09)"
  hairline-strong: "rgba(255, 255, 255, 0.16)"
  control-wash: "rgba(255, 255, 255, 0.06)"
  control-wash-hover: "rgba(255, 255, 255, 0.11)"
  moonlit-text: "#e8ebf2"
  slate-muted: "#98a2b6"
  slate-faint: "#6b7489"
  signal-sky: "#5cc8ff"
  periwinkle-tail: "#7fb4ff"
  sky-ink: "#06121c"
  splat-violet: "#a78bfa"
  score-green: "#34d399"
  caution-amber: "#fbbf24"
  alarm-coral: "#f87171"
typography:
  headline:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "26px"
    fontWeight: 650
    letterSpacing: "-0.01em"
  title:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "17px"
    fontWeight: 650
    letterSpacing: "0.01em"
  title-card:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "15px"
    fontWeight: 600
  body:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.45
  meta:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "12.5px"
    fontWeight: 400
  label:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "12px"
    fontWeight: 650
    letterSpacing: "0.09em"
  score:
    fontFamily: "Inter, Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial, sans-serif"
    fontSize: "18px"
    fontWeight: 650
    fontFeature: "tnum"
  mono:
    fontFamily: "ui-monospace, SF Mono, Cascadia Mono, Menlo, Consolas, monospace"
    fontSize: "12.5px"
rounded:
  sm: "8px"
  md: "12px"
  card: "14px"
  surface: "16px"
  pill: "999px"
spacing:
  xs: "6px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "18px"
  gutter: "28px"
components:
  button:
    backgroundColor: "{colors.control-wash}"
    textColor: "{colors.moonlit-text}"
    rounded: "{rounded.sm}"
    padding: "7px 12px"
    height: "36px"
  button-hover:
    backgroundColor: "{colors.control-wash-hover}"
  button-primary:
    backgroundColor: "{colors.signal-sky}"
    textColor: "{colors.sky-ink}"
    rounded: "{rounded.sm}"
    padding: "7px 12px"
    height: "36px"
  icon-button:
    backgroundColor: "{colors.night-glass}"
    textColor: "{colors.moonlit-text}"
    rounded: "{rounded.pill}"
    size: "36px"
  workspace-tab:
    backgroundColor: "transparent"
    textColor: "{colors.slate-muted}"
    rounded: "{rounded.sm}"
    padding: "6px 13px"
  workspace-tab-active:
    backgroundColor: "rgba(92, 200, 255, 0.14)"
    textColor: "{colors.moonlit-text}"
  status-badge:
    backgroundColor: "{colors.night-glass}"
    textColor: "{colors.moonlit-text}"
    rounded: "{rounded.pill}"
    padding: "6px 12px"
  glass-card:
    backgroundColor: "{colors.night-glass}"
    textColor: "{colors.moonlit-text}"
    rounded: "{rounded.md}"
    padding: "12px 14px"
  room-card:
    backgroundColor: "rgba(255, 255, 255, 0.03)"
    textColor: "{colors.moonlit-text}"
    rounded: "{rounded.card}"
  version-card:
    backgroundColor: "rgba(255, 255, 255, 0.035)"
    textColor: "{colors.moonlit-text}"
    rounded: "{rounded.md}"
    padding: "12px 14px"
    width: "236px"
  version-card-selected:
    backgroundColor: "rgba(92, 200, 255, 0.09)"
  chip:
    backgroundColor: "transparent"
    textColor: "{colors.signal-sky}"
    rounded: "{rounded.pill}"
    padding: "2px 9px"
---

# Design System: Splat Live (web and desktop interface)

This system covers the web and desktop interface only (`crates/gs-app`, Leptos, served by gs-server). It does not apply to the iPhone app `ios/SplatCapture` (SwiftUI), which PRODUCT.md records and which has its own look.

## Overview

**Creative North Star: "The Night Gallery"**

The interface is a set of dark glass cards floating over the reconstruction itself: the full-window 3D canvas is always the ground, and every control sits above it on translucent night panels with a hairline edge. Nothing competes with the splats. Colour comes from three sources only: the sky-blue signal of what is selected or primary, the green of a quality score, and the photographs and splats of the rooms themselves.

The interface has two workspaces, Direct (live capture) and Pièces (the archive). The archive is a gallery of filmed rooms, each card led by a real photograph from the capture. An opened room puts its 3D view first, with a small info card at top left, its actions at top right, and a strip of its versions along the bottom edge. Density is moderate and technical: dB scores, megabytes, gaussian counts and durations are shown as they are, in tabular figures. All UI text is French.

**Key Characteristics:**
- Full-window 3D canvas under everything; UI floats on glass (`backdrop-filter: blur(14px)` to `blur(18px)`).
- Hairline borders at 9 % white, raised to 16 % on hover and on strong surfaces.
- One signal colour (sky) for selection and the primary action, green reserved for scores.
- Real capture photographs on room cards; an initial letter placeholder only when the photo fails.
- Inter-first system stack with intermediate weights (550, 650).
- One responsive breakpoint at 760 px.

## Colors

A cold night palette with one sky signal, one score green and violet kept for the brand's splat identity.

### Primary
- **Signal Sky** (signal-sky): The selected version's outline and wash, the active workspace tab's icon, the focus ring, chips, the accent of section icons, and the primary action. Primary buttons fill with a 135° gradient from Signal Sky to **Periwinkle Tail** (periwinkle-tail), with **Sky Ink** (sky-ink) text.

### Secondary
- **Splat Violet** (splat-violet): The brand's second splat. It appears in the logo's overlapping discs, the gradient of the WebXR-ready VR button, the dot marking a recording (take) in the version strip, and the dashed outline of the comparison target.

### Tertiary
- **Score Green** (score-green): Held-out quality scores (dB) on room cards, orphan rows and version cards; the "connected" status dot; the third logo disc.

### Neutral
- **Night Ground** (night-ground): Page and gallery background; the 3D viewport fades from #121826 at its centre into this colour.
- **Night Glass** / **Night Glass Strong** (night-glass, night-glass-strong): Floating cards, tabs, badges and icon buttons; the strong one for the notice pill and the refine form.
- **Dialog Slate** (dialog-slate): The opaque modal card.
- **Hairline** / **Hairline Strong**: Every border and divider.
- **Control Wash** / **Control Wash Hover**: Resting and hover fill of ordinary buttons.
- **Moonlit Text** (moonlit-text), **Slate Muted** (slate-muted), **Slate Faint** (slate-faint): Primary text, secondary facts and meta, and tertiary hints (raw capture dot, "sans score", chevrons).
- **Caution Amber** / **Alarm Coral**: Warning and error states, connection dots, the recording pulse. Each is used at full strength for dots and text, and as a 12 % fill with a 35 % border for messages.

### Named Rules
**The One Signal Rule.** Signal Sky means "this one": the selected version, the active tab, focus, the primary action. Never use it for decoration or for a second, competing emphasis on the same screen.

**The Green Means Quality Rule.** Score Green is reserved for measured quality and healthy connection. A missing score is shown in Slate Faint ("pas encore affinée", "sans score"), never in green.

**The Violet Is Identity Rule.** Violet marks the brand, recordings, WebXR readiness and the comparison target. It never fills a primary action and never marks the selection.

## Typography

**Display/Body Font:** Inter (with Segoe UI, system-ui, -apple-system, Helvetica Neue, Arial)
**Mono Font:** ui-monospace stack, for URLs, tokens and fingerprints only

**Character:** A single neutral sans that relies on intermediate weights (550 for controls, 600 for card titles, 650 for headings and scores) rather than size jumps. Numbers that are compared use tabular figures.

### Hierarchy
- **Headline** (650, 26px, -0.01em): The workspace page title ("Pièces"), one per screen.
- **Title** (650, 17px): Brand name, modal titles; 16px in the room info card.
- **Title Card** (600, 15px): Room card and orphan row titles, single line with ellipsis.
- **Body** (400, 14px, 1.45): Base size; controls in 550.
- **Meta** (400, 12.5px, Slate Muted): Facts lines under titles ("Pièce · 135 Mo · 5 versions"), hints.
- **Label** (650 or 600, 12px, 0.09–0.1em, uppercase, Slate Muted): Headings for a group of cards or a panel section ("Versions de cette pièce", panel section heads). A label is the group's only heading.
- **Score** (650, 18px, tabular, Score Green): The dB score on version cards; room cards show it at body size.

### Named Rules
**The Weight Not Size Rule.** Hierarchy inside cards comes from weight and colour (600 title, muted meta, green score), not from a larger size.

## Layout

The 3D canvas is `position: fixed; inset: 0` under every surface. The top bar (56px minimum) is a three-column grid: brand and workspace tabs at the left, breadcrumb or mode switch at the centre, status badge and icon buttons at the right. It sits on a gradient that fades from 92 % night to transparent rather than a solid bar.

- **Gallery:** an opaque Night Ground sheet over the canvas, padded `topbar + 20px` / 28px, content capped at 1480px and centred. Room cards fill `repeat(auto-fill, minmax(280px, 1fr))` with an 18px gap. Rooms without a local capture follow as full-width rows under a label.
- **Opened room:** the canvas fills the window. The info card is pinned at top left and the tools at top right (both `topbar + 8px`, 14px inset). The version strip is pinned to the bottom edge (168px tall) on a gradient that fades upward, with horizontally scrolling 236px cards and a 12px gap. The settings panel (332px wide, right side) pushes the tools and the strip clear of it when open.
- **Rhythm:** 12px is the base gap. 8px between buttons, 14–16px card insets, 18px between gallery cards, 28px gallery gutter.
- **Breakpoint, 760px and below:** workspace tabs collapse to icons, the brand name hides, the breadcrumb takes the centre slot's grid area, gallery cards drop to `minmax(220px, 1fr)` with a 12px gap, room tools move above the strip and wrap, the strip grows to 196px with 200px cards, and the HUD chips hide in an opened room.

## Elevation & Depth

Depth comes from translucency and blur over the live 3D scene first, and from soft black shadows second. Resting cards in the gallery are flat (3 % white fill, hairline). Shadows belong to floating layers and to hover.

### Shadow Vocabulary
- **Panel float** (`box-shadow: 0 18px 50px rgba(0, 0, 0, 0.45)`): The settings panel and the refine form (0.5).
- **Pill float** (`box-shadow: 0 10px 30px rgba(0, 0, 0, 0.4)`): The centred notice pill.
- **Modal** (`box-shadow: 0 30px 80px rgba(0, 0, 0, 0.6)`): The dialog card over a 66 % night scrim with 4px blur.
- **Card lift** (`box-shadow: 0 14px 34px rgba(0, 0, 0, 0.45)`): Room card hover, with `translateY(-2px)`.
- **Selection ring** (`box-shadow: 0 0 0 1px rgba(92, 200, 255, 0.35)`): Doubles the selected version's sky border.
- **Status halo** (`box-shadow: 0 0 0 3px <status colour at 18 %>`): Around connection dots.

### Named Rules
**The Glass Over Scene Rule.** Anything that floats over the 3D view is Night Glass with a blur and a hairline. Opaque surfaces are only the gallery sheet and the modal.

## Shapes

Soft rectangles and full pills. Controls use gently rounded 8px corners; floating cards and version cards use 12px; room cards 14px; the panel and the modal 16px. Badges, the mode switch, icon buttons, HUD chips and the notice pill are full pills (999px). Room photographs are clipped by their card at a 4:3 ratio. The "+ Nouvel affinage" slot uses a dashed hairline border, and the comparison target uses a dashed violet border.

## Components

### Buttons
- **Shape:** gently rounded (8px), 36px minimum height, 550 weight, icon and text gap 8px.
- **Default:** Control Wash fill with a hairline border; hover raises the fill to Control Wash Hover and the border to Hairline Strong (0.15s).
- **Primary:** Signal Sky to Periwinkle Tail gradient at 135°, Sky Ink text, no border; hover lightens both stops (#7dd4ff to #9cc5ff). One per view ("Comparer").
- **Icon button:** 36px glass circle with blur; active state is a 55 % sky border with sky icon.
- **Focus:** 2px Signal Sky outline, 2px offset, on every focusable element.
- **Disabled:** 42 % opacity.

### Workspace Tabs (Direct | Pièces)
A segmented glass container (3px inset, 2px gap) holding two tabs with a line icon and a label. Resting tabs are Slate Muted; hover turns text Moonlit; the active tab gets a 14 % sky wash, Moonlit text and a sky icon. At 760px and below only the icons remain.

### Breadcrumb
Muted "Pièces" link (4px 6px padding, Control Wash on hover), a faint chevron, then the current room in 600 Moonlit with ellipsis. It takes the centre of the top bar when a room is open.

### Status Badge
Glass pill with a coloured dot (amber connecting, green connected, coral failed, each with a 3px halo) and the server name; the state word in muted text.

### Room Card (signature)
- **Corner Style:** 14px, clipping a 4:3 capture photograph (`object-fit: cover`).
- **Background:** 3 % white with a hairline border.
- **Content:** title (Title Card), meta line (mode · size · versions), best score in Score Green or "pas encore affinée" in Slate Faint.
- **Hover:** border goes to 45 % sky, the card lifts 2px with the card-lift shadow (0.18s, `cubic-bezier(0.16, 1, 0.3, 1)`).
- **Fallback:** when the photo is missing or fails, a large faint initial letter on a dark radial gradient.

### Version Strip (signature)
The bottom strip of an opened room, headed by a Label ("Versions de cette pièce"). It runs from the raw capture to each refinement and recording, then a dashed "+ Nouvel affinage" slot.
- **Card:** 12px corners, 3.5 % white fill, hairline, 104px minimum height, label (600), facts (Meta, single line with ellipsis), score pinned to the bottom (Score style, "· meilleure" for the best).
- **Kind dot:** 7px dot before the label, Slate Faint for the raw capture and Splat Violet for a recording.
- **Selected:** 70 % sky border, 9 % sky wash, 1px sky selection ring; `aria-pressed="true"`.
- **Comparison target:** 60 % violet border, dashed.
- **Loading:** pulse (opacity to 0.6, 0.9s).
- **Hover:** border Hairline Strong, fill 6 % white.

### Glass Cards / Containers
The room info card and similar overlays: Night Glass, hairline, 12px corners, 12px 14px padding, 14px blur. The settings panel is the same material at 16px corners with an 18px blur, a 1.2 saturation boost, and the panel-float shadow. It slides in from the right (0.22s ease).

### Chips and HUD
- **Chip:** transparent, 40 % sky hairline, sky text, 12px, full pill.
- **HUD chip:** glass pill, muted 12px tabular text (frame rate, splat count, camera mode), not interactive.

### Gallery Rows (orphans)
Full-width rows, 12px corners, 3 % white fill, hairline; title, facts, score in a three-column grid; hover turns the border 45 % sky. Below 760px the facts column hides.

## Do's and Don'ts

### Do:
- **Do** keep the 3D canvas full-window under every surface and float controls on Night Glass with blur and a hairline.
- **Do** mark the selected item with a sky border, a 9 % sky wash and the 1px sky ring, and the comparison target with a dashed violet border.
- **Do** show quality as a dB figure in Score Green with tabular numerals, and the absence of a score in Slate Faint text.
- **Do** lead every room card with a real photograph taken from the capture, at 4:3.
- **Do** write all interface text in French and keep the technical facts (Mo, M gaussiennes, min, dB) as they are.
- **Do** draw icons as inline 24×24 line SVGs with a round 1.8px stroke in `currentColor`.
- **Do** collapse to the 760px layout: icon-only tabs, a taller strip, tools above the strip.

### Don't:
- **Don't** fill a primary action or mark a selection with violet; violet is identity, recordings, WebXR readiness and the comparison target.
- **Don't** use green for anything but measured quality or a healthy connection.
- **Don't** put a solid opaque bar or panel over the 3D view; only the gallery sheet and the modal are opaque.
- **Don't** add a stock or generated image to a room card; when the capture photo is missing, show the initial letter.
- **Don't** put an uppercase label above another heading as an eyebrow; a label is the only heading of its group.
- **Don't** apply this system to the iPhone app (`ios/SplatCapture`).
