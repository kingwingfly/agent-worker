//! Container calls workers-rs 0.8 does not cover.
//!
//! Its `ContainerStartupOptions` hands the runtime a JS `Map` where a plain object of
//! environment variables is expected, and it has no binding for the inactivity timeout,
//! so both go through `wasm_bindgen` here.

use wasm_bindgen::{JsCast, prelude::*};
use worker::{
    Container, Result,
    js_sys::{Object, Promise, Reflect},
    wasm_bindgen_futures::JsFuture,
};

#[wasm_bindgen]
extern "C" {
    type RawContainer;

    #[wasm_bindgen(method, catch, js_name = start)]
    fn start_with(this: &RawContainer, options: &JsValue) -> std::result::Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = setInactivityTimeout)]
    fn set_inactivity_timeout(
        this: &RawContainer,
        millis: f64,
    ) -> std::result::Result<Promise, JsValue>;
}

fn raw(container: &Container) -> &RawContainer {
    container.as_ref().unchecked_ref()
}

/// Starts the container with the given environment and outbound internet access.
/// Returns as soon as the start is accepted; the ports open later.
pub fn start(container: &Container, env: &[(&str, &str)]) -> Result<()> {
    let vars = Object::new();
    for (name, value) in env {
        Reflect::set(&vars, &JsValue::from_str(name), &JsValue::from_str(value))?;
    }
    let options = Object::new();
    Reflect::set(&options, &JsValue::from_str("env"), &vars)?;
    Reflect::set(
        &options,
        &JsValue::from_str("enableInternet"),
        &JsValue::TRUE,
    )?;
    raw(container).start_with(&options)?;
    Ok(())
}

/// Lets the container outlive its Durable Object's last activity by this long, as a
/// backstop for the alarm that normally stops it.
pub async fn keep_alive_for(container: &Container, millis: u64) -> Result<()> {
    let promise = raw(container).set_inactivity_timeout(millis as f64)?;
    JsFuture::from(promise).await?;
    Ok(())
}
