//! The views: a top bar, a collapsible side panel and a few overlays on
//! top of the full-window 3D canvas.

mod icons;
mod overlays;
mod panel;
mod phone;
mod refine;
mod sections;
mod takes;
mod timeline;
mod topbar;
mod widgets;

pub use overlays::Overlays;
pub use panel::Panel;
pub use takes::ShareDialog;
pub use timeline::Timeline;
pub use topbar::TopBar;
