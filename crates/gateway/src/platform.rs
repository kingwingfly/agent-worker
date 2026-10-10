//! Small pieces of the Workers runtime the rest of the crate needs.

use wasm_bindgen::prelude::*;
use worker::{Date, Error, Response, Result};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = crypto, js_name = randomUUID)]
    fn crypto_random_uuid() -> String;
}

/// A random version 4 UUID from the runtime's CSPRNG.
pub fn random_uuid() -> String {
    crypto_random_uuid()
}

pub fn now_ms() -> u64 {
    Date::now().as_millis()
}

pub fn error(message: impl Into<String>) -> Error {
    Error::RustError(message.into())
}

/// Passes a successful response through and turns anything else into an error that
/// carries the status and body.
pub async fn expect_ok(mut response: Response, what: &str) -> Result<Response> {
    let status = response.status_code();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(error(format!("{what} failed with {status}: {body}")))
}
