//! Production controller: one daemon owns activation and per-ONU business workers.
mod activation;
mod attribute_probe;
pub mod business;
pub mod fec;
pub mod health;
#[cfg(feature = "device-hl202")]
mod hl202;
pub mod inventory;
mod lifetime;
pub mod management;
#[cfg(feature = "mqtt")]
pub mod mqtt;
mod pon_mac;
pub mod profile;
mod runtime;
pub(crate) mod service;
mod status;
mod telemetry;
pub use runtime::run;
