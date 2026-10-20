//! A native or browser monotonic clock for actual elapsed stage measurements.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use std::time::Instant;

/// Browser timestamp in milliseconds since the current page time origin.
#[cfg(all(target_arch = "wasm32", feature = "web"))]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    /// Read the host's monotonic performance clock, including in a worker.
    #[wasm_bindgen::prelude::wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;
}

/// Monotonic timestamp compatible with the elapsed native stage-timing calls.
#[cfg(target_arch = "wasm32")]
pub(crate) struct Instant(f64);

#[cfg(target_arch = "wasm32")]
impl Instant {
    /// Capture the actual browser clock when browser support is enabled.
    pub(crate) fn now() -> Self {
        Self(now())
    }

    /// Report elapsed time without calling unsupported WASM platform clocks.
    pub(crate) fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(((now() - self.0) / 1_000.0).max(0.0))
    }
}

/// Read a browser clock; non-browser WASM has no host timing service installed.
#[cfg(target_arch = "wasm32")]
fn now() -> f64 {
    #[cfg(feature = "web")]
    {
        performance_now()
    }
    #[cfg(not(feature = "web"))]
    {
        0.0
    }
}
