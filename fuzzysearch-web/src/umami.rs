use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = umami, js_name = trackEvent)]
    pub fn track_event(event_name: &str, event_type: &str);
}
