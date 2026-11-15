# Product

<!-- impeccable:product-schema 1 -->

## Platform

ios

The record covers the iPhone companion app, `ios/SplatCapture` (SwiftUI,
iOS 17+, iPhone and iPad with LiDAR). The desktop and web interface
(`crates/gs-app`, Leptos) is a separate surface with its own look; nothing
here binds it.

## Users

One user for now: the project owner, scanning his own rooms with an iPhone 15
Pro Max, the Mac running the Splat Live server nearby on the same Wi-Fi. He
walks through a room holding the phone in portrait or landscape, filming,
while the reconstruction grows on the Mac. He knows the project inside out;
no onboarding audience is confirmed.

## Product Purpose

SplatCapture films a space with the camera, the LiDAR depth and ARKit's pose,
and streams every frame to the Splat Live server, which turns it into
Gaussian splats in real time; or it records the capture on the phone (`.gscap`)
to send later. Success: a capture that verifies, reconstructs live without
dropped frames, and is kept for offline refinement.

## Positioning

The phone is a sensor for a reconstruction that happens elsewhere, live. The
app does not build or show the 3D model itself; the Mac does.

## Operating Context

- Pairing with the server by scanning a QR code (`splatlive://pair?…`), or
  manual entry; the token changes at every server start.
- Four capture modes: pièce (room), objet, personne, direct. Objet and
  personne use a region the user places by touching the subject, with a
  radius.
- During a capture: pause, resume, finish, reset. Frames sent, acknowledged
  and skipped; frames and splats on the server side; tracking advice from
  ARKit; connection state.
- Recording without a server (offline toggle), recordings list, settings.
- Used while moving, one hand on the phone, sometimes in a dim room.

## Capabilities and Constraints

- The capture formats are fixed by `docs/ARCHITECTURE.md`, `tools/gscap.py`
  and the fixtures; UI work never changes them.
- The orientation byte comes from the camera pose; the interface may rotate
  freely.
- All user-facing text is French; code and comments are English.
- Live counters must stay visible during a capture: the owner watches them
  to judge the capture (confirmed 2026-11-15).

## Brand Commitments

- Name: Splat Live; the app is SplatCapture.
- The app icon reuses the desktop app's icon (dark, blue, violet and green
  blobs).
- The iPhone app is free to follow iOS conventions first; it need not copy
  the web interface's look (confirmed 2026-11-15).

## Evidence on Hand

- First real captures: `out/captures/premiere-capture-reelle-2026-11-15.gscap`
  (outside git).
- No screenshots of the app in the repository yet; no users other than the
  owner.

## Product Principles

1. The camera image comes first; controls never hide what is being filmed.
2. The numbers that judge a capture are always in sight while filming.
3. Native iOS behaviour over invented controls.
4. Nothing the user does in the interface may alter the recorded data.
