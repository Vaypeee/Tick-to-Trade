//! TLS setup.
//!
//! `rustls` refuses to pick a cryptographic backend for you when more than one
//! could be linked, and `tokio-tungstenite` does not select one either, so the
//! choice has to be made here. It is made once, explicitly, rather than left
//! to a feature flag that would fail at the first websocket handshake.
//!
//! `ring` is chosen over `aws-lc-rs` because it builds without a C toolchain,
//! which keeps the VPS deployments this project depends on reproducible.

/// Installs the process-wide provider. Safe to call more than once.
pub fn install_crypto_provider() {
    // Installing twice is not an error worth surfacing: the first caller wins
    // and every later caller ends up with the same provider either way.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
