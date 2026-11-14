//! The one Tauri command the interface uses. The binding is resolved when
//! it is called, so the module also loads in a plain browser.

use wasm_bindgen::prelude::*;

use crate::logic::source::ServerInfo;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn invoke_raw(command: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

/// True inside the Tauri shell (`withGlobalTauri`).
pub fn is_tauri() -> bool {
    web_sys::window()
        .and_then(|w| js_sys::Reflect::get(&w, &"__TAURI__".into()).ok())
        .is_some_and(|v| v.is_object())
}

/// Info of the server the shell started in-process.
pub async fn server_info() -> Result<ServerInfo, String> {
    let value = invoke_raw("server_info", js_sys::Object::new().into())
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    // The command returns the info document, as an object or as JSON text.
    let json = match value.as_string() {
        Some(text) => text,
        None => js_sys::JSON::stringify(&value).ok().and_then(|s| s.as_string()).unwrap_or_default(),
    };
    ServerInfo::parse(&json)
}
