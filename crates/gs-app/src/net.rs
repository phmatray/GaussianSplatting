//! Small `fetch` helpers for the server's HTTP API.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, RequestInit, Response};

/// Status and body of an HTTP answer.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn describe(error: JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "erreur réseau".to_string())
}

/// `GET url`, or `POST url` with a JSON body, given up after `timeout_ms`.
pub async fn request(url: &str, json_body: Option<&str>, timeout_ms: i32) -> Result<Answer, String> {
    let window = web_sys::window().ok_or("pas de fenêtre")?;
    let init = RequestInit::new();
    if let Some(body) = json_body {
        init.set_method("POST");
        init.set_body(&JsValue::from_str(body));
        let headers = web_sys::Headers::new().map_err(describe)?;
        headers.set("Content-Type", "application/json").map_err(describe)?;
        init.set_headers(&headers);
    }
    // A host that never answers must not keep the application waiting.
    let abort = AbortController::new().map_err(describe)?;
    init.set_signal(Some(&abort.signal()));
    let cancel = Closure::once(move || abort.abort());
    let timer = window
        .set_timeout_with_callback_and_timeout_and_arguments_0(cancel.as_ref().unchecked_ref(), timeout_ms)
        .map_err(describe)?;
    let result = async {
        let response: Response = JsFuture::from(window.fetch_with_str_and_init(url, &init)).await?.dyn_into()?;
        let body = JsFuture::from(response.text()?).await?;
        Ok::<_, JsValue>(Answer { status: response.status(), body: body.as_string().unwrap_or_default() })
    }
    .await;
    window.clear_timeout_with_handle(timer);
    result.map_err(describe)
}
