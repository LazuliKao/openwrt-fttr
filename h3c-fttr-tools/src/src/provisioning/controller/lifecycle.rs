use super::*;
use serde::ser::{Serialize, SerializeSeq, Serializer};
use serde::Deserialize;

#[derive(Deserialize)]
struct Recovered {
    operations: Vec<Value>,
    #[serde(default)]
    retired: Vec<Value>,
    #[serde(default)]
    owned: BTreeMap<String, BTreeMap<String, wan::Owned>>,
}

struct Operations<'a>(&'a Controller);

impl Serialize for Operations<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for (id, recovered) in &self.0.recovered {
            if !self.0.operations.contains_key(id) {
                sequence.serialize_element(recovered)?;
            }
        }
        for operation in self.0.operations.values() {
            sequence.serialize_element(&operation.view())?;
        }
        sequence.end()
    }
}

impl Controller {
    pub(super) fn begin_history_tick(&mut self) {
        self.dirty |= self.operations.values().any(|op| {
            op.stage != OperationPhase::Completed && op.stage != OperationPhase::Interrupted
        });
    }

    pub(super) fn persist_history(&mut self, write: impl FnOnce(&[u8]) -> std::io::Result<()>) {
        if !self.dirty && !self.saved.is_empty() {
            return;
        }
        let saved = self.history();
        if saved == self.saved {
            self.dirty = false;
            return;
        }
        match write(saved.as_bytes()) {
            Ok(()) => {
                self.saved = saved;
                self.dirty = false;
                self.failure = None;
            }
            Err(error) => {
                self.failure = Some(format!("provisioning state persistence: {error}"));
                self.dirty = true;
            }
        }
    }

    pub(super) fn resolve_pending_identities(&mut self, devices: &Value) {
        let mut resolved = false;
        for d in &mut self.config.devices {
            if d.cwmp_id.starts_with("pending:") && !self.resets.blocks(&d.optical_sn) {
                if let Some(id) = discovered_identity(devices, &d.optical_sn, &d.adapter) {
                    d.cwmp_id = id;
                    resolved = true;
                }
            }
        }
        if resolved {
            self.retain_configured_state();
        }
    }

    pub(super) fn history(&self) -> String {
        #[derive(serde::Serialize)]
        struct History<'a> {
            operations: Operations<'a>,
            retired: &'a Vec<Value>,
            owned: &'a BTreeMap<String, BTreeMap<String, wan::Owned>>,
        }
        #[expect(
            clippy::expect_used,
            reason = "JSON values and string-keyed ownership maps serialize infallibly"
        )]
        serde_json::to_string(&History {
            operations: Operations(self),
            retired: &self.retired,
            owned: &self.owned,
        })
        .expect("controller history is serializable")
    }

    pub(super) fn recover(&mut self, data: &[u8]) {
        if let Ok(recovered) = serde_json::from_slice::<Recovered>(data) {
            self.retired = recovered.retired;
            self.owned = recovered.owned;
            for mut operation in recovered.operations {
                self.next_id = self
                    .next_id
                    .max(operation["operation_id"].as_u64().unwrap_or(0));
                if operation["optical_sn"]
                    .as_str()
                    .is_some_and(|serial| self.resets.blocks(serial))
                {
                    continue;
                }
                if operation["state"] == "completed" {
                    // Legacy history has no completion timestamp. Start a full wait
                    // at recovery rather than treating the operation start as its end.
                    if operation["completed_at"].as_u64().is_none() {
                        operation["completed_at"] = json!(now());
                    }
                } else {
                    operation["state"] = json!("interrupted");
                    operation["reconcile_required"] = json!(true);
                    operation["reconcile_reason"] = json!("service_restarted");
                    if let Some(items) = operation["items"].as_array_mut() {
                        for item in items {
                            if item["state"] != "verified" && item["state"] != "unknown" {
                                item["state"] = json!("reconcile_required");
                                item["reason"] = json!("reconcile after service restart");
                            }
                        }
                    }
                }
                if let Some(id) = operation["device"].as_str().map(str::to_owned) {
                    self.recovered.insert(id, operation);
                }
            }
            self.owned.retain(|id, _| {
                !self.resets.owns(id)
                    && !self
                        .config
                        .devices
                        .iter()
                        .any(|d| &d.cwmp_id == id && self.resets.blocks(&d.optical_sn))
            });
            self.dirty = true;
        }
    }

    fn config_blocked(&self) -> bool {
        self.operations.values().any(|op| {
            !matches!(
                op.stage,
                OperationPhase::Completed | OperationPhase::Interrupted
            )
        }) || self
            .config
            .devices
            .iter()
            .any(|d| self.resets.blocks(&d.optical_sn))
    }

    pub(super) fn request_config(&mut self, config: Config) {
        if config.revision == self.config.revision {
            self.pending_config = None;
            self.config_error = None;
            return;
        }
        // Never wait for an offline device's old, entirely unstarted plan.
        // Once discovery or a transport job starts, its owner must drain it.
        for op in self.operations.values_mut().filter(|op| {
            !op.work_started
                && op.index == 0
                && op.task.is_none()
                && op.submission.is_none()
                && op.command_job.is_none()
                && matches!(
                    op.stage,
                    OperationPhase::Waiting | OperationPhase::WanReconciling
                )
        }) {
            op.supersede_remaining();
            self.dirty = true;
        }
        // Repeated reloads of the same revision retain the original wait time.
        let requested_at = self
            .pending_config
            .as_ref()
            .filter(|(previous, _)| previous.revision == config.revision)
            .map_or_else(now, |(_, at)| *at);
        self.pending_config = Some((config, requested_at));
        self.config_error = None;
        self.activate_pending_config();
    }

    pub(super) fn activate_pending_config(&mut self) -> bool {
        if self.pending_config.is_none() || self.config_blocked() {
            return false;
        }
        let (config, _) = self.pending_config.take().unwrap();
        // The controller mutex covers both the eligibility check and replacement.
        self.replace_config(config)
            .expect("configuration eligibility checked under lock");
        self.config_error = None;
        self.failure = None;
        self.attempts.clear();
        self.next_retry_at.clear();
        true
    }

    pub(super) fn config_load_view(&self) -> Value {
        let blockers: Vec<_> = self.operations.values()
            .filter(|op| !matches!(op.stage, OperationPhase::Completed | OperationPhase::Interrupted))
            .map(|op| json!({"operation_id":op.id,"device":op.device.id,"state":op.stage,"started_at":op.started}))
            .collect();
        json!({"state":if self.pending_config.is_some() {"waiting"} else {"applied"},
            "requested_revision":self.pending_config.as_ref().map_or(&self.config.revision, |(cfg, _)| &cfg.revision),
            "requested_at":self.pending_config.as_ref().map(|(_, at)| at),
            "blocking_operations":blockers,
            "reset_pending":self.config.devices.iter().any(|d| self.resets.blocks(&d.optical_sn))})
    }

    pub(super) fn replace_config(&mut self, mut config: Config) -> Result<(), String> {
        if self.pending_config.is_some() {
            return Err("configuration reload pending; wait for automatic activation".into());
        }
        if self
            .config
            .devices
            .iter()
            .any(|d| self.resets.blocks(&d.optical_sn))
        {
            return Err("device reset pending; retry reset before reload".into());
        }
        if self.operations.values().any(|op| {
            !matches!(
                op.stage,
                OperationPhase::Completed | OperationPhase::Interrupted
            )
        }) {
            return Err("active synchronization: finish operation before reload".into());
        }
        for device in &mut config.devices {
            if device.cwmp_id.starts_with("pending:") {
                if let Some(previous) = self.config.devices.iter().find(|previous| {
                    previous.id == device.id
                        && previous.optical_sn == device.optical_sn
                        && previous.adapter == device.adapter
                }) {
                    device.cwmp_id.clone_from(&previous.cwmp_id);
                }
            }
        }
        for previous in &self.config.devices {
            if !config.devices.iter().any(|d| {
                d.id == previous.id
                    && d.optical_sn == previous.optical_sn
                    && d.cwmp_id == previous.cwmp_id
            }) {
                self.management.remove(&previous.id);
                self.last_boot.remove(&previous.id);
                self.last_online.remove(&previous.id);
            }
        }
        for previous in &self.config.devices {
            previous
                .connection_request
                .authorization
                .store(false, std::sync::atomic::Ordering::Release);
        }
        self.config = config;
        super::super::health_watch::config(
            self.config
                .devices
                .iter()
                .filter(|d| d.omci_management)
                .map(|d| d.optical_sn.clone())
                .collect(),
        );
        self.load_reset_intents();
        self.retain_configured_state();
        Ok(())
    }

    pub(super) fn retain_configured_state(&mut self) {
        let previous = (
            self.operations.len(),
            self.recovered.len(),
            self.owned.len(),
        );
        let devices = &self.config.devices;
        for (id, view) in self
            .operations
            .iter()
            .map(|(id, op)| (id, op.view()))
            .chain(self.recovered.iter().map(|(id, op)| (id, op.clone())))
        {
            if devices.iter().any(|d| {
                &d.id == id
                    && view["optical_sn"] == d.optical_sn
                    && view["backend"]["effective"].as_str().unwrap_or("acs") != d.backend()
            }) {
                let mut retired = view;
                retired["retired_reason"] =
                    json!("provisioning backend changed; rediscovery required");
                self.retired.push(retired);
            }
        }
        if self.retired.len() > 32 {
            self.retired.drain(..self.retired.len() - 32);
        }
        self.operations.retain(|id, op| {
            devices.iter().any(|d| {
                &d.id == id
                    && d.backend() == op.device.backend()
                    && d.provisioning_method == op.device.provisioning_method
                    && d.optical_sn == op.device.optical_sn
                    && (d.cwmp_id.starts_with("pending:")
                        || op.device.cwmp_id.starts_with("pending:")
                        || d.cwmp_id == op.device.cwmp_id)
            })
        });
        self.recovered.retain(|id, op| {
            !self.operations.contains_key(id)
                && devices.iter().any(|d| {
                    &d.id == id
                        && op["optical_sn"] == d.optical_sn
                        && op["backend"]["effective"].as_str().unwrap_or("acs") == d.backend()
                        && (d.cwmp_id.starts_with("pending:") || op["cwmp_id"] == d.cwmp_id)
                })
        });
        self.management
            .retain(|id, _| devices.iter().any(|d| &d.id == id));
        self.last_boot
            .retain(|id, _| devices.iter().any(|d| &d.id == id));
        self.last_online
            .retain(|id| devices.iter().any(|d| &d.id == id));
        self.omci_management
            .retain(|serial, _| devices.iter().any(|d| &d.optical_sn == serial));
        self.optical_epochs
            .retain(|serial, _| devices.iter().any(|d| &d.optical_sn == serial));
        self.attempts
            .retain(|id, _| devices.iter().any(|d| &d.id == id));
        self.next_retry_at
            .retain(|id, _| devices.iter().any(|d| &d.id == id));
        let retained_binding = |id: &String| {
            devices.iter().any(|d| &d.cwmp_id == id)
                || self
                    .recovered
                    .values()
                    .any(|op| op["cwmp_id"].as_str() == Some(id.as_str()))
                || self.operations.values().any(|op| &op.device.cwmp_id == id)
        };
        self.owned.retain(|id, _| retained_binding(id));
        self.wifi_owned.retain(|id, _| retained_binding(id));
        self.dirty |= previous
            != (
                self.operations.len(),
                self.recovered.len(),
                self.owned.len(),
            );
    }
}
