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
        "record" => "M12 5a7 7 0 1 0 0 14a7 7 0 0 0 0-14zM12 9.5a2.5 2.5 0 1 0 0 5a2.5 2.5 0 0 0 0-5z",
        "stop" => "M7 7h10v10H7z",
        "loop" => "M17 3.5l3 3-3 3M4 11.5v-2a3 3 0 0 1 3-3h13M7 20.5l-3-3 3-3M20 12.5v2a3 3 0 0 1-3 3H4",
        "fullscreen" => "M4 9V4h5M15 4h5v5M20 15v5h-5M9 20H4v-5",
        "fullscreen-exit" => "M9 4v5H4M20 9h-5V4M15 20v-5h5M4 15h5v5",
        "link" => "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1.2 1.2M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1.2-1.2",
        "copy" => "M9 9h10.5v11.5H9zM5 15H4.5V3.5H15V5",
        "trash" => "M5 7h14M9.5 7V4.5h5V7M7 7l1 13h8l1-13M10.5 10.5v6M13.5 10.5v6",
        "phone" => "M8 3h8a1.5 1.5 0 0 1 1.5 1.5v15A1.5 1.5 0 0 1 16 21H8a1.5 1.5 0 0 1-1.5-1.5v-15A1.5 1.5 0 0 1 8 3zM11 18h2",
        "send" => "M12 15V4M8 8l4-4 4 4M5 13v5.5A1.5 1.5 0 0 0 6.5 20h11a1.5 1.5 0 0 0 1.5-1.5V13",
        "film" => "M4 5h16v14H4zM8 5v14M16 5v14M4 9.5h4M4 14.5h4M16 9.5h4M16 14.5h4",
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
