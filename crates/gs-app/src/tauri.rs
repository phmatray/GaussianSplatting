//! The Tauri commands the interface uses. The binding is resolved when it
//! is called, so the module also loads in a plain browser.

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

fn error_text(error: JsValue) -> String {
    error.as_string().unwrap_or_else(|| "La fenêtre de l'application n'a pas répondu.".to_string())
}

fn arguments(pairs: &[(&str, &str)]) -> JsValue {
    let object = js_sys::Object::new();
    for (name, value) in pairs {
        let _ = js_sys::Reflect::set(&object, &JsValue::from_str(name), &JsValue::from_str(value));
    }
    object.into()
}

/// Asks the shell to write a file of the server into the user's download
/// folder (a web view has no download manager). `kind` is `recording`,
/// `share`, `refined` or `refined_share`; `name` is the file name without
/// its extension. Returns the path written; errors are sentences for the
/// user.
pub async fn export(name: &str, kind: &str) -> Result<String, String> {
    let value = invoke_raw("export_recording", arguments(&[("name", name), ("kind", kind)])).await.map_err(error_text)?;
    value.as_string().ok_or_else(|| "L'export n'a pas donné de fichier.".to_string())
}

/// Asks the shell to show something in the file manager: `recordings`,
/// `captures`, `refined`, `datasets`, `exports`, or a path [`export`]
/// returned.
pub async fn reveal(target: &str) -> Result<(), String> {
    invoke_raw("reveal", arguments(&[("target", target)])).await.map(|_| ()).map_err(error_text)
}
