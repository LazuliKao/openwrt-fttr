//! Backend-neutral asynchronous command job. Device contracts live in their adapters.
use crate::provisioning::model::Device;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Job(Arc<Mutex<Option<Result<Value, String>>>>);

impl Job {
    pub fn take(&self) -> Option<Result<Value, String>> {
        self.0.lock().unwrap().take()
    }

    pub fn start(
        device: Device,
        apply: bool,
        full: bool,
        revision: String,
        operation_id: u64,
    ) -> Self {
        let job = Self::default();
        let result = job.clone();
        let auth_epoch = crate::omci_auth::serial_epoch(&device.optical_sn);
        crate::runtime::spawn(async move {
            #[cfg(all(feature = "device-hl202", feature = "mqtt"))]
            let outcome = if auth_epoch.is_none() {
                Err("OMCI authentication pending".into())
            } else if device.provisioning_adapter == "hl202-mqtt" {
                crate::devices::hl202::provisioning::run(
                    device,
                    apply,
                    full,
                    revision,
                    operation_id,
                    auth_epoch.unwrap(),
                )
                .await
            } else {
                Err("command provisioning unsupported by this adapter".into())
            };
            #[cfg(not(all(feature = "device-hl202", feature = "mqtt")))]
            let outcome = {
                let _ = (device, apply, full, revision, operation_id, auth_epoch);
                Err("command provisioning not compiled".into())
            };
            *result.0.lock().unwrap() = Some(outcome);
            crate::provisioning::controller::wake();
        });
        job
    }
}
