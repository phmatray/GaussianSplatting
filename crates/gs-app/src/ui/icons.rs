//! Line icons, drawn inline so that the application ships no image files.

use leptos::prelude::*;

/// Path data of an icon on a 24 x 24 grid, drawn with a round 1.8 px stroke.
fn path(name: &str) -> &'static str {
    match name {
        "play" => "M8 5.5v13l11-6.5z",
        "pause" => "M8.5 5v14M15.5 5v14",
        "panel" => "M4 7h9M17 7h3M4 12h3M11 12h9M4 17h11M19 17h1M15 5v4M9 10v4M17 15v4",
        "chevron" => "M6 9l6 6 6-6",
        "close" => "M6 6l12 12M18 6L6 18",
        "vr" => "M4 8h16a1.5 1.5 0 0 1 1.5 1.5v6A1.5 1.5 0 0 1 20 17h-3.6l-2.2-2.6h-4.4L7.6 17H4a1.5 1.5 0 0 1-1.5-1.5v-6A1.5 1.5 0 0 1 4 8zM8 11.5v1M16 11.5v1",
        "capture" => "M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17zM12 9a3 3 0 1 0 0 6a3 3 0 0 0 0-6z",
        "eye" => "M2.5 12s3.6-6.5 9.5-6.5S21.5 12 21.5 12s-3.6 6.5-9.5 6.5S2.5 12 2.5 12zM12 9.2a2.8 2.8 0 1 0 0 5.6a2.8 2.8 0 0 0 0-5.6z",
        "camera" => "M4 8h3.2l1.6-2.5h6.4L16.8 8H20v10.5H4zM12 10.2a3.2 3.2 0 1 0 0 6.4a3.2 3.2 0 0 0 0-6.4z",
        "chart" => "M4 20V11M9.5 20V5M15 20v-7M20.5 20V8",
        "file" => "M6.5 3.5h7l4 4V20.5h-11zM13.5 3.5v4h4",
        "download" => "M12 4v11M7.5 11l4.5 4.5 4.5-4.5M5 20h14",
        "upload" => "M12 15.5V4.5M7.5 9l4.5-4.5 4.5 4.5M5 20h14",
        "share" => "M4.5 11.5a10.5 10.5 0 0 1 15 0M7.5 14.5a6.5 6.5 0 0 1 9 0M12 18.5v0",
        "restart" => "M4.5 12a7.5 7.5 0 1 0 2.3-5.4M4.5 4.5v4.5h4.5",
        "finish" => "M6 21V4.5h11.5l-2.5 4 2.5 4H6",
        "alert" => "M12 4.5l8.5 15h-17zM12 10.5v4M12 17.2v0",
        "house" => "M4 11.2l8-6.7 8 6.7V20H4zM10 20v-5.5h4V20",
        "stage" => "M12 4.5a2.2 2.2 0 1 0 0 4.4a2.2 2.2 0 0 0 0-4.4zM12 9.5v5M8 11.5l4 1 4-2.5M12 14.5l-2.5 5M12 14.5l3 4.5",
        "check" => "M5 12.5l4.5 4.5L19 7.5",
        "back" => "M9 6l-6 6 6 6M3 12h18",
        _ => "",
    }
}

/// An inline SVG icon; `name` is one of the names of [`path`].
#[component]
pub fn Icon(name: &'static str) -> impl IntoView {
    view! {
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
            <path d=path(name)></path>
        </svg>
    }
}
