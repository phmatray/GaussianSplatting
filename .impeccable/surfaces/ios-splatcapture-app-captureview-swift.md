---
version: 1
slug: "ios-splatcapture-app-captureview-swift"
primary_target: "ios/SplatCapture/App/CaptureView.swift"
related_targets: []
---

# Écran de capture (SplatCapture, iPhone)

Scope: the capture tab of the iPhone app, portrait and landscape. Mode: Operate.
Job: film a room while walking and judge the capture live; start, pause,
start over, finish. Constraints: native iOS controls, Liquid Glass from
iOS 26 (system material before), French copy, numbers always visible while
filming, the camera image first.

## Direction contract

THESIS: the capture's health reads in one glance, as a frieze of one tick per
frame, green once the server confirmed it; refuses scattered boxes and a
dashboard over the image.

OWN-WORLD: one Liquid Glass panel at the bottom, SF Pro with tabular digits,
white on glass; red reserved for recording (shutter, recording dot), green for
confirmed and connected, orange for dropped; small glass pill at top.

STORY: the owner starts filming, watches the frieze stay green, reads frames
and splats under it, and stops with the thumb.

FIRST VIEWPORT: portrait, a pill under the Dynamic Island (recording dot, clock, mode,
link dot); a bottom panel: frieze of the last 64 frames, caption line,
four numbers, then pause, shutter, start over. The tab bar hides while
filming. Landscape: same panel on the right third.

FORM: Frise des images, position 5 of the grounded list, seed:5b40fef3ca46e450 (re-roll 1).

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
