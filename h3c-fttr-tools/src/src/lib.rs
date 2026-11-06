#[cfg(feature = "acs")]
pub mod acs;
pub mod acs_cli;
pub mod bosa;
#[cfg(feature = "mqtt")]
pub mod broker;
pub mod capabilities;
pub mod controller;
pub mod devices;
pub mod fpga;
pub mod fpga_diag;
pub mod fpga_diag_cli;
pub mod hl202_ipc;
pub mod ipc;
pub mod model;
#[cfg(feature = "mqtt")]
pub mod mqtt;
pub mod mqtt_identity;
#[cfg(feature = "npu")]
pub mod npu;
#[cfg(not(feature = "npu"))]
pub mod npu {
    use crate::model::NpuStatus;
    pub fn status() -> NpuStatus {
        NpuStatus::default()
    }
}
#[cfg(any(feature = "mqtt", test))]
mod mqtt_limits;
pub mod optical;
pub mod platform;
pub mod pon_engine;
pub mod provisioning;
pub mod provisioning_cli;
mod runtime;
#[cfg(any(feature = "mqtt", test))]
mod telemetry_sources;

// Unit fixtures which prove the MQTT-off boundary share the production port.
#[cfg(all(test, not(feature = "mqtt")))]
pub(crate) static MQTT_TEST_PORT: std::sync::Mutex<()> = std::sync::Mutex::new(());
