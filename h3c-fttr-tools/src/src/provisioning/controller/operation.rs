//! Per-device operation transitions and redacted status representation.
use super::*;

impl Operation {
    pub(super) fn new(
        id: u64,
        mut device: Device,
        revision: String,
        apply: bool,
        management: bool,
    ) -> Self {
        let mut activations = Vec::new();
        for service in &mut device.items {
            if service.kind != ItemKind::DeviceService {
                continue;
            }
            if let Some(enable) = service
                .parameters
                .iter_mut()
                .find(|p| p.name.ends_with(".Enable"))
            {
                if matches!(enable.value.as_str(), "1" | "true") {
                    let desired = enable.clone();
                    enable.value = "0".into();
                    let mut activation = service.clone();
                    activation.id = format!("{}__activation", service.id);
                    activation.kind = ItemKind::WanActivation;
                    activation.parameters = vec![desired];
                    activations.push(activation);
                }
            }
        }
        for binding in device
            .items
            .iter_mut()
            .filter(|i| i.kind == ItemKind::PortBinding)
        {
            if binding.parameters.is_empty() {
                continue;
            }
            if let Some(activation) = activations.iter().find(|i| i.wan_ref == binding.wan_ref) {
                let mut enable = activation.parameters[0].clone();
                enable.value = "0".into();
                binding.parameters.push(enable);
            }
        }
        device.items.extend(activations);
        let wan = wan::Reconciler::new(&device, BTreeMap::new(), false);
        let wifi = wifi::Reconciler::new(&device, wifi::Registry::default(), false);
        Self {
            work_started: false,
            command_job: None,
            command_result: None,
            full_sync: true,
            verify_after: 0,
            compiled: false,
            capabilities: crate::devices::cwmp::runtime::Discovery::new(&device.cwmp_contract),
            id,
            device,
            revision,
            apply,
            management,
            index: 0,
            stage: OperationPhase::Waiting,
            task: None,
            submission: None,
            wan,
            wifi,
            roots: Vec::new(),
            writable: BTreeSet::new(),
            changes: Vec::new(),
            uncertain: false,
            pending: false,
            written: false,
            passphrase_fallback: false,
            results: BTreeMap::new(),
            started: now(),
            completed_at: None,
        }
    }

    pub(super) fn item(&self) -> &Item {
        &self.device.items[self.index]
    }

    pub(super) fn finish_item(&mut self, state: ItemOutcome, reason: &str) {
        let i = self.item().clone();
        if i.kind == ItemKind::Ssid && state == ItemOutcome::Verified && self.apply {
            for p in &i.parameters {
                if let Some(n) = wifi::instance(&p.name) {
                    self.wifi.registry.owned.insert(n);
                }
            }
        }
        self.results.insert(i.id.clone(), json!({"id":i.id,"kind":i.kind,"service_type":i.service_type,"state":state,"reason":reason,"revision":self.revision,"operation_id":self.id,"task_id":self.task,"updated_at":now(),"changes":self.changes.iter().map(|p| json!({"name":p.name,"desired":if p.secret {"<redacted>"} else {&p.value}})).collect::<Vec<_>>()}));
        if i.kind == ItemKind::WanActivation && state != ItemOutcome::Verified {
            if let Some(service) = i.wan_ref.clone() {
                if let Some(result) = self.results.get_mut(&service) {
                    // A dependent activation must not hide the service's
                    // original configuration failure.
                    if ItemOutcome::from_result(result) == Some(ItemOutcome::Verified) {
                        result["state"] = json!(state);
                        result["reason"] = json!(format!("WAN activation: {reason}"));
                    }
                }
            }
        }
        self.index += 1;
        self.task = None;
        self.submission = None;
        self.stage = OperationPhase::Waiting;
        self.roots.clear();
        self.writable.clear();
        self.changes.clear();
        self.uncertain = false;
        self.pending = false;
        self.written = false;
        self.passphrase_fallback = false;
        if self.index >= self.device.items.len() {
            self.stage = OperationPhase::WifiReconciling;
        }
    }

    pub(super) fn complete(&mut self, time: u64) {
        self.submission = None;
        self.stage = OperationPhase::Completed;
        self.completed_at.get_or_insert(time);
    }

    pub(super) fn supersede_remaining(&mut self) {
        if self.device.backend() == "command" && self.command_result.is_none() {
            self.command_result = Some(json!({"items":[]}));
        }
        for item in self.device.items.iter().skip(self.index) {
            self.results.insert(item.id.clone(), json!({"id":item.id,"kind":item.kind,"service_type":item.service_type,"state":"blocked","reason":"superseded at a safe boundary by a newer configuration","revision":self.revision}));
        }
        self.index = self.device.items.len();
        self.complete(now());
    }

    pub(super) fn view(&self) -> Value {
        if self.device.backend() == "command" {
            let mut result = self.command_result.clone().unwrap_or_else(|| json!({"items":self.device.items.iter().map(|i| json!({"id":i.id,"kind":i.kind,"service_type":i.service_type,"state":self.stage,"revision":self.revision})).collect::<Vec<_>>()}));
            result["operation_id"] = json!(self.id);
            result["device"] = json!(self.device.id);
            result["optical_sn"] = json!(self.device.optical_sn);
            result["cwmp_id"] = json!(self.device.cwmp_id);
            result["revision"] = json!(self.revision);
            result["backend"] = self.device.backend_view();
            result["state"] = json!(self.stage);
            result["mode"] = json!(if self.apply { "apply" } else { "plan" });
            result["started_at"] = json!(self.started);
            result["completed_at"] = json!(self.completed_at);
            result["wifi_stage"] = json!(if self.completed_at.is_some() {
                "done"
            } else {
                "inventory"
            });
            if let Some(items) = result["items"].as_array_mut() {
                items.extend(self.results.values().cloned());
                for item in items {
                    item["revision"] = json!(self.revision);
                    item["operation_id"] = json!(self.id);
                }
            }
            return result;
        }
        let mut items: Vec<Value> = self.results.values().cloned().collect();
        for i in self.device.items.iter().skip(self.index) {
            items.push(json!({"id":i.id,"kind":i.kind,"service_type":i.service_type,"state":if i.id == self.item().id { self.stage.as_str() } else {"pending"},"task_id":if i.id == self.item().id {self.task} else {None},"revision":self.revision}));
        }
        json!({"backend":self.device.backend_view(),"operation_id":self.id,"device":self.device.id,"optical_sn":self.device.optical_sn,"cwmp_id":self.device.cwmp_id,"revision":self.revision,"mode":if self.apply {"apply"} else {"plan"},"state":self.stage,"started_at":self.started,"completed_at":self.completed_at,"items":items,"cwmp_contract":self.capabilities.view(),"wan_objects":self.wan.owned,"wan_lifecycle":self.wan.results,"wan_stage":self.wan.stage,"wifi_objects":self.wifi.registry,"wifi_lifecycle":self.wifi.results,"wifi_stage":self.wifi.stage,"wifi_cleanup":self.device.wifi_cleanup})
    }
}
