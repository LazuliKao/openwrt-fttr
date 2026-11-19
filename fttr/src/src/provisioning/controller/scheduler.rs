//! One bounded reconciliation transaction per scheduler tick.
use super::*;
use crate::provisioning::{bootstrap, link_identity, management_network, storage};

pub(super) fn tick(engine: &Arc<Mutex<crate::pon_engine::PonEngine>>, batch: Ready) -> Duration {
    // Release the optical lock before consulting ACS or the controller.
    let online_onus: BTreeMap<_, _> = engine
        .lock()
        .unwrap()
        .active_onus
        .iter()
        .filter(|o| o.state == crate::pon_engine::ploam::OnuState::O5Operation)
        .map(|o| (u16::from(o.onu_id), o.full_sn_str()))
        .collect();
    let online: BTreeSet<_> = online_onus.values().cloned().collect();
    let resources = engine.lock().unwrap().resources;
    let interfaces = resources
        .onu_ids
        .iter()
        .filter_map(|&id| {
            resources
                .interface(id)
                .map(|name| (u16::from(id), name.to_owned()))
        })
        .collect();
    let source_links = link_identity::discover(&online_onus, &interfaces);
    if let Ok(mut registry) = crate::mqtt_identity::shared().lock() {
        registry.update_topology(online_onus.clone(), source_links, now());
    }
    let devices = acs(Command::Devices);
    let acs_status = acs(Command::Status);
    controller().lock().unwrap().activate_pending_config();
    let configured = controller().lock().unwrap().config.clone();
    // Bootstrap-free ACS consumers must also reach the new listener before the
    // old network is retired. They have no bootstrap record to satisfy that gate.
    let cwmp_business_ready =
        bootstrap_free_consumers_ready(&configured, &devices, &acs_status, &online, now());
    management_network::tick(configured.management_network.as_ref(), cwmp_business_ready);
    let active_management = management_network::active();
    let mut c = controller().lock().unwrap();
    c.acs_status = acs_status.into_metadata();
    c.resolve_pending_identities(&devices);
    crate::omci_auth::bind(&c.config.devices);
    let config = c.config.clone();
    bootstrap::retain_devices(
        &config
            .devices
            .iter()
            .filter(|d| d.bootstrap_adapter != "none" && !c.resets.blocks(&d.optical_sn))
            .map(|d| d.id.clone())
            .collect(),
    );
    let time = now();
    c.optical_epochs.retain(|serial, _| {
        online.contains(serial) && config.devices.iter().any(|d| &d.optical_sn == serial)
    });
    for (id, serial) in &online_onus {
        if !config.devices.iter().any(|d| &d.optical_sn == serial) {
            continue;
        }
        if c.optical_epochs
            .get(serial)
            .is_none_or(|(previous, _)| previous != id)
        {
            let epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            c.optical_epochs.insert(serial.clone(), (*id, epoch));
        }
    }
    let mut fresh = BTreeSet::new();
    c.begin_history_tick();
    let mut history_changed = false;
    for d in &config.devices {
        if !crate::omci_auth::allowed(&d.optical_sn) {
            continue;
        }
        if c.resets.blocks(&d.optical_sn) {
            continue;
        }
        // Observe both channels and advance bootstrap independently of business transport.
        let observed = &devices["devices"][&d.cwmp_id];
        let at = current_inform(observed, &c.acs_status);
        let cwmp_reachable = online.contains(&d.optical_sn)
            && at.is_some_and(|at| {
                at <= time
                    && time - at <= d.connection_request.effective_freshness(config.freshness)
            });
        let state = if !d.cwmp_enabled() {
            "disabled"
        } else if cwmp_reachable {
            "online"
        } else if at.is_none() {
            "waiting_inform"
        } else {
            "offline_or_stale"
        };
        c.management.insert(d.id.clone(), json!({"cwmp_id":d.cwmp_id,"product_class":observed["product_class"],"state":state,"optical_online":online.contains(&d.optical_sn),"last_inform":at,"age_seconds":at.map(|at| time.saturating_sub(at))}));
        let mqtt_reachable = online.contains(&d.optical_sn)
            && crate::mqtt_identity::shared()
                .lock()
                .unwrap()
                .snapshots(time)
                .iter()
                .any(|s| s.optical_sn.as_deref() == Some(&d.optical_sn) && s.reason == "ready");
        c.mqtt.insert(d.id.clone(), json!({"state":if mqtt_reachable {"online"} else {"waiting_transport"},"optical_online":online.contains(&d.optical_sn)}));
        let boot = observed["inform"]["events"].as_array().is_some_and(|a| {
            a.iter()
                .any(|e| matches!(e.as_str(), Some("0 BOOTSTRAP" | "1 BOOT")))
        });
        let new_boot = boot && at.is_some() && at != c.last_boot.get(&d.id).copied();
        if boot {
            if let Some(at) = at {
                c.last_boot.insert(d.id.clone(), at);
            }
        }
        if d.bootstrap_adapter != "none" {
            bootstrap::tick(bootstrap::Observation {
                device: d,
                management: active_management.as_ref(),
                last_inform: at,
                online: online.contains(&d.optical_sn),
                new_boot,
                freshness: d.connection_request.effective_freshness(config.freshness),
                time,
                acs_available: active_management.is_some()
                    && c.acs_status["enabled"] == true
                    && c.acs_status["error"].is_null(),
                authorization: management_network::grant(&d.id, &d.optical_sn),
            });
        }
        if d.backend() == "disabled" {
            continue;
        }
        if d.backend() == "command" {
            let reachable = mqtt_reachable;
            if reachable {
                fresh.insert(d.id.clone());
            }
            schedule(&mut c, d, reachable, false, time);
            if let Some(op) = c.operations.get_mut(&d.id) {
                if let Some(job) = &op.command_job {
                    if let Some(outcome) = job.take() {
                        op.command_result = Some(outcome.unwrap_or_else(|reason| json!({"items":op.device.items.iter().map(|i| json!({"id":i.id,"kind":i.kind,"service_type":i.service_type,"state":"blocked","reason":reason})).collect::<Vec<_>>()})));
                        op.command_job = None;
                        op.complete(time);
                        history_changed = true;
                    }
                } else if op.stage != OperationPhase::Completed && reachable {
                    op.work_started = true;
                    op.command_job = Some(crate::provisioning::command::Job::start(
                        op.device.clone(),
                        op.apply,
                        op.full_sync,
                        op.revision.clone(),
                        op.id,
                    ));
                    op.stage = OperationPhase::Reading;
                    history_changed = true;
                }
            }
            continue;
        }
        let host_matches = config
            .management_network
            .as_ref()
            .is_none_or(management_network::matches_desired);
        let reachable = host_matches && cwmp_reachable;
        if reachable {
            fresh.insert(d.id.clone());
        }
        schedule(&mut c, d, reachable, new_boot, time);
        let capability_identity = identity(d, observed);
        use std::hash::{Hash, Hasher};
        let mut epoch_hash = std::collections::hash_map::DefaultHasher::new();
        (
            c.last_boot.get(&d.id).copied().unwrap_or(0),
            c.optical_epochs
                .get(&d.optical_sn)
                .map(|(_, epoch)| *epoch)
                .unwrap_or(0),
        )
            .hash(&mut epoch_hash);
        let capability_epoch = epoch_hash.finish();
        let availability_changed = reachable != c.last_online.contains(&d.id);
        let reload_pending = c.pending_config.is_some();
        if let Some(op) = c.operations.get_mut(&d.id) {
            if op.stage == OperationPhase::Completed
                && (new_boot
                    || op
                        .capabilities
                        .observation_changed(&capability_identity, capability_epoch))
            {
                op.capabilities.invalidate("identity, firmware, optical registration or BOOTSTRAP changed; evidence is stale");
                history_changed = true;
            }
            if op.device.cwmp_id.starts_with("pending:") && !d.cwmp_id.starts_with("pending:") {
                history_changed = true;
                op.device.cwmp_id = d.cwmp_id.clone();
                match (wan::load(&d.cwmp_id), wifi::load(&d.cwmp_id)) {
                    (Ok(owned), Ok(registry)) => {
                        let cleanup = op.wan.full_sync();
                        op.wan =
                            wan::Reconciler::new(&op.device, owned.unwrap_or_default(), cleanup);
                        op.wifi = wifi::Reconciler::new(&op.device, registry, cleanup);
                    }
                    _ => {
                        op.complete(now());
                        op.wan.results.insert(
                            "identity".into(),
                            json!({"state":"blocked","reason":"cannot load device ownership"}),
                        );
                    }
                }
            }
            if op.stage != OperationPhase::Completed
                && (batch.all
                    || batch.devices.contains(&d.cwmp_id)
                    || new_boot
                    || availability_changed)
            {
                if config.management_network.is_some() && !host_matches {
                    continue;
                }
                let before = (
                    op.stage,
                    op.index,
                    op.task,
                    op.compiled,
                    op.capabilities.progress(),
                    op.wan.progress(),
                    op.wifi.progress(),
                );
                op.work_started = true;
                let publish = reconcile(
                    op,
                    capability_identity,
                    capability_epoch,
                    reachable,
                    reload_pending,
                );
                let after = (
                    op.stage,
                    op.index,
                    op.task,
                    op.compiled,
                    op.capabilities.progress(),
                    op.wan.progress(),
                    op.wifi.progress(),
                );
                if before != after && op.stage != OperationPhase::Completed {
                    wake_device(&d.cwmp_id);
                }
                if !publish {
                    continue;
                }
            }
            if let Err(error) = op.wan.save(&op.device.cwmp_id) {
                history_changed = true;
                op.wan.results.insert(
                    "persistence".into(),
                    json!({"state":"blocked","reason":error}),
                );
            }
            if let Err(error) = op.wifi.save(&op.device.cwmp_id) {
                history_changed = true;
                op.wifi.results.insert(
                    "persistence".into(),
                    json!({"state":"blocked","reason":error}),
                );
                op.complete(now());
            }
            let wifi_owned = op.wifi.registry.clone();
            c.wifi_owned.insert(d.cwmp_id.clone(), wifi_owned);
            let owned = c.operations.get(&d.id).unwrap().wan.owned.clone();
            c.owned.insert(d.cwmp_id.clone(), owned);
        }
    }
    c.last_online = fresh;
    c.dirty |= history_changed;
    c.persist_history(|data| {
        storage::write_atomic("/var/lib/fttr/provisioning-state.json", data, false)
    });
    if c.activate_pending_config() {
        // Do not wait for the periodic sweep after the final old operation finishes.
        wake();
        return Duration::ZERO;
    }
    // State remains authoritative; the low-frequency sweep also detects optical
    // changes from backends that do not publish CWMP events.
    let deadline = c
        .operations
        .values()
        .flat_map(|op| [op.verify_after, op.capabilities.deadline()])
        .chain(c.next_retry_at.values().copied())
        .filter(|at| *at > time)
        .min();
    Duration::from_secs(
        deadline
            .map_or(10, |at| at.saturating_sub(time).min(10))
            .max(1),
    )
}

fn bootstrap_free_consumers_ready(
    config: &Config,
    devices: &Value,
    acs: &Value,
    online: &BTreeSet<String>,
    time: u64,
) -> bool {
    config
        .devices
        .iter()
        .filter(|d| d.bootstrap_adapter == "none" && d.backend() == "acs")
        .all(|d| {
            let id =
                discovered_identity(devices, &d.optical_sn).unwrap_or_else(|| d.cwmp_id.clone());
            current_inform(&devices["devices"][&id], acs).is_some_and(|at| {
                online.contains(&d.optical_sn)
                    && at <= time
                    && time - at <= d.connection_request.effective_freshness(config.freshness)
            })
        })
}

fn current_inform(observed: &Value, acs: &Value) -> Option<u64> {
    (observed["endpoint_id"] == acs["endpoint_id"]
        && acs["enabled"] == true
        && acs["error"].is_null()
        && acs["endpoint_id"].as_str().is_some_and(|id| !id.is_empty()))
    .then(|| observed["last_inform"].as_u64())
    .flatten()
}

#[cfg(test)]
#[test]
fn bootstrap_free_acs_consumers_require_current_listener_evidence_before_network_cleanup() {
    let mut device = tests::operation(OperationPhase::Completed, false).device;
    device.bootstrap_adapter = "none".into();
    let mut config = Config::default();
    config.devices.push(device.clone());
    let online = BTreeSet::from([device.optical_sn.clone()]);
    let mut devices = json!({"devices":{}});
    devices["devices"][&device.cwmp_id] = json!({"endpoint_id":"new-listener","last_inform":100});
    let acs = json!({"enabled":true,"endpoint_id":"new-listener","error":null});
    assert!(bootstrap_free_consumers_ready(
        &config, &devices, &acs, &online, 101
    ));
    for (endpoint, inform, time) in [
        ("old-listener", 100, 101),
        ("new-listener", 102, 101),
        ("new-listener", 0, 100000),
    ] {
        devices["devices"][&device.cwmp_id] = json!({"endpoint_id":endpoint,"last_inform":inform});
        assert!(!bootstrap_free_consumers_ready(
            &config, &devices, &acs, &online, time
        ));
    }
    devices["devices"][&device.cwmp_id] = json!({"endpoint_id":"new-listener","last_inform":100});
    assert!(!bootstrap_free_consumers_ready(
        &config,
        &devices,
        &acs,
        &BTreeSet::new(),
        101
    ));
    for adapter in ["none", "hl202-mqtt"] {
        config.devices[0].provisioning_adapter = adapter.into();
        assert!(bootstrap_free_consumers_ready(
            &config,
            &json!({}),
            &json!({}),
            &BTreeSet::new(),
            101
        ));
    }
}

fn schedule(c: &mut Controller, d: &Device, reachable: bool, new_boot: bool, time: u64) {
    if c.pending_config.is_some() {
        return;
    }
    let previous = c
        .operations
        .get(&d.id)
        .map(Operation::view)
        .or_else(|| c.recovered.get(&d.id).cloned());
    let needs_reconcile = previous.as_ref().is_some_and(|p| {
        p["reconcile_required"] == true
            || p["items"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|i| ItemOutcome::from_result(i) == Some(ItemOutcome::ReconcileRequired))
            })
    });
    let needs_manual = previous.as_ref().is_some_and(|p| {
        (p["state"] == "interrupted" && p["reconcile_required"] != true)
            || ["wan_lifecycle", "wifi_lifecycle"].iter().any(|key| {
                p[*key].as_object().is_some_and(|m| {
                    m.values()
                        .any(|i| ItemOutcome::from_result(i) == Some(ItemOutcome::Unknown))
                })
            })
            || p["items"].as_array().is_some_and(|a| {
                a.iter().any(|i| {
                    matches!(
                        ItemOutcome::from_result(i),
                        Some(ItemOutcome::Unknown | ItemOutcome::AcceptedPendingApply)
                    )
                })
            })
    });
    let all_verified = previous.as_ref().is_some_and(|p| {
        p["state"] == "completed"
            && p["items"].as_array().is_some_and(|a| {
                !a.is_empty()
                    && a.iter()
                        .all(|i| ItemOutcome::from_result(i) == Some(ItemOutcome::Verified))
            })
    });
    if all_verified {
        c.attempts.remove(&d.id);
        c.next_retry_at.remove(&d.id);
    }
    let active_op = c.operations.get(&d.id).is_some_and(|o| {
        o.stage != OperationPhase::Completed && o.stage != OperationPhase::Interrupted
    });
    let startup_trigger = c.startup_trigger_pending(&d.id);
    let config_changed = c.config.triggers.contains("config_change")
        && previous
            .as_ref()
            .is_none_or(|p| p["revision"] != c.config.business_revision());
    let periodic = c.periodic_due(&d.id, time);
    if !active_op && reachable && c.config.enabled && c.config.automatic {
        if needs_reconcile {
            let _ = c.start_selected(&d.id, false, false, None);
        } else if !needs_manual
            && (config_changed
                || startup_trigger
                || periodic
                || (c.config.triggers.contains("device_online") && !c.last_online.contains(&d.id))
                || (new_boot && c.config.triggers.contains("cwmp_boot")))
        {
            if c.start(&d.id, true, false).is_ok() && startup_trigger {
                c.startup_pending.remove(&d.id);
            }
        } else if !needs_manual
            && previous.as_ref().is_some_and(|p| {
                p["items"].as_array().is_some_and(|a| {
                    a.iter()
                        .any(|i| ItemOutcome::from_result(i) == Some(ItemOutcome::Drift))
                })
            })
        {
            let attempts = c.attempts.get(&d.id).copied().unwrap_or(0);
            let next = c.next_retry_at.get(&d.id).copied().unwrap_or(0);
            if attempts < 5 && now() >= next {
                let next_attempts = attempts + 1;
                c.attempts.insert(d.id.clone(), next_attempts);
                let delay = (15 * (1 << (next_attempts - 1))).min(300);
                c.next_retry_at.insert(d.id.clone(), now() + delay);
                let _ = c.start(&d.id, true, false);
            }
        }
    }
}

#[cfg(test)]
mod config_change_tests {
    use super::*;

    fn fixture() -> (Controller, Device) {
        let mut op = super::super::tests::operation(OperationPhase::Completed, false);
        op.results
            .insert("wifi".into(), json!({"id":"wifi","state":"verified"}));
        let device = op.device.clone();
        crate::omci_auth::test_authorize(&device.optical_sn);
        let mut c = Controller::default();
        c.config.enabled = true;
        c.config.automatic = true;
        c.config.revision = "new-revision".into();
        c.config.triggers.insert("config_change".into());
        c.config.devices.push(device.clone());
        c.operations.insert(device.id.clone(), op);
        (c, device)
    }

    #[test]
    fn changed_revision_is_scheduled_per_device_when_it_becomes_reachable() {
        let (mut c, ready) = fixture();
        let mut offline = ready.clone();
        offline.id = "offline".into();
        offline.optical_sn = "H3CT00000002".into();
        offline.cwmp_id = "offline-id".into();
        c.config.devices.push(offline.clone());
        let mut previous = c.operations[&ready.id].view();
        previous["device"] = json!(offline.id);
        previous["optical_sn"] = json!(offline.optical_sn);
        c.recovered.insert(offline.id.clone(), previous);

        schedule(&mut c, &offline, false, false, 100);
        schedule(&mut c, &ready, true, false, 100);
        assert_eq!(c.operations[&ready.id].revision, "new-revision");
        assert!(c.operations[&ready.id].apply);
        assert!(!c.operations.contains_key(&offline.id));
        assert_eq!(c.recovered[&offline.id]["revision"], "rev");
        let id = c.operations[&ready.id].id;
        schedule(&mut c, &ready, true, false, 101);
        assert_eq!(c.operations[&ready.id].id, id);

        crate::omci_auth::test_authorize(&offline.optical_sn);
        schedule(&mut c, &offline, true, false, 102);
        assert_eq!(c.operations[&offline.id].revision, "new-revision");
    }

    #[test]
    fn revision_trigger_survives_restart_and_does_not_repeat_after_completion() {
        let (c, device) = fixture();
        let mut restarted = Controller::default();
        restarted.config = c.config.clone();
        restarted.recover(c.history().as_bytes());
        schedule(&mut restarted, &device, true, false, 100);
        assert_eq!(restarted.operations[&device.id].revision, "new-revision");
        let op = restarted.operations.get_mut(&device.id).unwrap();
        op.results
            .insert("wifi".into(), json!({"id":"wifi","state":"verified"}));
        op.index = op.device.items.len();
        op.complete(101);
        let id = op.id;
        schedule(&mut restarted, &device, true, false, 102);
        assert_eq!(restarted.operations[&device.id].id, id);
    }

    #[test]
    fn config_change_preserves_unknown_write_and_disabled_trigger_guards() {
        let (mut c, device) = fixture();
        c.operations
            .get_mut(&device.id)
            .unwrap()
            .results
            .insert("wifi".into(), json!({"id":"wifi","state":"unknown"}));
        schedule(&mut c, &device, true, false, 100);
        assert_eq!(c.operations[&device.id].revision, "rev");
        c.operations.get_mut(&device.id).unwrap().results.clear();
        c.config.triggers.clear();
        schedule(&mut c, &device, true, false, 101);
        assert_eq!(c.operations[&device.id].revision, "rev");
    }

    #[test]
    fn pending_reload_prevents_old_revision_rescheduling_then_starts_latest_immediately() {
        let (mut c, device) = fixture();
        let mut blocker = super::super::tests::operation(OperationPhase::Writing, false);
        blocker.device.id = "busy".into();
        let mut next = c.config.clone();
        next.revision = "latest".into();
        c.operations.insert("busy".into(), blocker);
        c.request_config(next);
        schedule(&mut c, &device, true, false, 100);
        assert_eq!(c.operations[&device.id].revision, "rev");
        c.operations.get_mut("busy").unwrap().complete(101);
        assert!(c.activate_pending_config());
        schedule(&mut c, &device, true, false, 101);
        assert_eq!(c.operations[&device.id].revision, "latest");
        assert!(c.operations[&device.id].apply);
    }
}

fn identity(d: &Device, observed: &Value) -> crate::cwmp::identity::Identity {
    let version = |suffix: &str| {
        observed["inform"]["parameters"]
            .as_array()
            .and_then(|p| {
                p.iter()
                    .find(|p| p["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
            })
            .and_then(|p| p["value"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    crate::cwmp::identity::Identity {
        optical_vendor: d.optical_sn[..4].into(),
        optical_serial: d.optical_sn.clone(),
        cwmp_serial: observed["serial"].as_str().unwrap_or("").into(),
        product_class: observed["product_class"].as_str().unwrap_or("").into(),
        software_version: version(".SoftwareVersion"),
        hardware_version: version(".HardwareVersion"),
    }
}

/// False retains the original tick's early-continue boundary (no ownership publication).
fn reconcile(
    op: &mut Operation,
    identity: crate::cwmp::identity::Identity,
    epoch: u64,
    reachable: bool,
    reload_pending: bool,
) -> bool {
    op.capabilities
        .tick(&op.device.cwmp_id, identity, epoch, reachable, acs);
    if op.capabilities.blocked() {
        let reason = op.capabilities.view()["reason"]
            .as_str()
            .unwrap_or("capability contract unresolved")
            .to_owned();
        for item in &op.device.items {
            op.results.entry(item.id.clone()).or_insert_with(|| json!({"id":item.id,"kind":item.kind,"service_type":item.service_type,"state":"blocked","reason":reason,"revision":op.revision}));
        }
        op.complete(now());
        return false;
    }
    if !op.capabilities.ready() {
        return false;
    }
    op.device.resolved_contract = Some(op.capabilities.selected().unwrap().to_owned());
    if !op.compiled {
        match op.capabilities.endpoints(&op.device) {
            Ok(endpoints) => {
                crate::cwmp::compiler::compile_device(&mut op.device, &endpoints);
                let mut prepared = Operation::new(
                    op.id,
                    op.device.clone(),
                    op.revision.clone(),
                    op.apply,
                    op.management,
                );
                prepared.wan = wan::Reconciler::new(
                    &prepared.device,
                    op.wan.owned.clone(),
                    op.wan.full_sync(),
                );
                prepared.wifi = wifi::Reconciler::new(
                    &prepared.device,
                    op.wifi.registry.clone(),
                    op.wan.full_sync(),
                );
                op.device = prepared.device;
                op.wan = prepared.wan;
                op.wifi = prepared.wifi;
                op.compiled = true;
            }
            Err(reason) => {
                op.capabilities.invalidate(&reason);
                return false;
            }
        }
    }
    op.wan.protected_roots = op.capabilities.protected_wan();
    op.wan.evidence_paths = op.capabilities.wan_fields();
    let ready = op
        .wan
        .tick(&mut op.device, op.apply, op.management, reachable);
    if let Err(error) = op.wan.save(&op.device.cwmp_id) {
        op.wan.results.insert(
            "persistence".into(),
            json!({"state":"blocked","reason":error}),
        );
        op.complete(now());
    } else if ready {
        op.wan.block_unresolved(&mut op.device);
        let finish = op.index >= op.device.items.len();
        if finish
            && op
                .wan
                .results
                .get("final_inventory")
                .is_some_and(|r| ItemOutcome::from_result(r) == Some(ItemOutcome::Blocked))
        {
            for item in op.device.items.iter().filter(|i| i.wan_ref.is_some()) {
                if let Some(result) = op.results.get_mut(&item.id) {
                    result["state"] = json!("blocked");
                    result["reason"] =
                        json!("final full WAN inventory does not match ordered configuration");
                }
            }
        }
        let verified = op
            .device
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Ssid)
            .all(|i| {
                op.results
                    .get(&i.id)
                    .is_some_and(|r| ItemOutcome::from_result(r) == Some(ItemOutcome::Verified))
            });
        if finish && op.wan.begin_final_verification(op.apply) {
            op.stage = OperationPhase::WanVerifying;
        } else if op
            .wifi
            .tick(&op.device, op.apply, reachable, finish, verified)
        {
            if finish {
                op.complete(now());
            } else {
                op.wifi.block_unresolved(&mut op.device);
                if matches!(
                    op.stage,
                    OperationPhase::WanReconciling | OperationPhase::WifiReconciling
                ) {
                    op.stage = OperationPhase::Waiting;
                }
                advance_or_supersede(op, reachable, reload_pending);
            }
        } else {
            op.stage = OperationPhase::WifiReconciling;
        }
        for item in &op.device.items {
            if op.results.get(&item.id).is_some_and(|r| {
                ItemOutcome::from_result(r) == Some(ItemOutcome::Verified)
                    && r["revision"] == op.revision
            }) {
                if let Some(owned) = op.wan.owned.get_mut(&item.id) {
                    if let Some(p) = item.parameters.iter().find(|p| {
                        p.name.ends_with("ServiceList") || p.name.ends_with("SERVICELIST")
                    }) {
                        owned.service_list = p.value.clone();
                    }
                }
            }
        }
    } else {
        op.stage = OperationPhase::WanReconciling;
    }
    true
}

// Called only after WAN and Wi-Fi lifecycle transactions are drained.
fn advance_or_supersede(op: &mut Operation, reachable: bool, reload_pending: bool) {
    if reload_pending
        && op.stage == OperationPhase::Waiting
        && op.task.is_none()
        && op.submission.is_none()
    {
        op.supersede_remaining();
    } else {
        advance(op, reachable);
    }
}

#[cfg(test)]
mod reload_boundary_tests {
    use super::*;

    #[test]
    fn safe_boundary_skips_old_work_even_offline_without_losing_unknown_results() {
        let mut op = super::super::tests::operation(OperationPhase::Waiting, false);
        op.task = None;
        op.results.insert(
            "previous".into(),
            json!({"id":"previous","state":"unknown"}),
        );
        advance_or_supersede(&mut op, false, true);
        assert_eq!(op.stage, OperationPhase::Completed);
        assert_eq!(op.results["previous"]["state"], "unknown");
        assert_eq!(op.results["wifi"]["state"], "blocked");
        assert!(op.task.is_none());
    }

    #[test]
    fn reload_never_discards_an_inflight_write() {
        let mut op = super::super::tests::operation(OperationPhase::Writing, false);
        RPC.with(|r| {
            *r.borrow_mut() = (
                std::collections::VecDeque::from([json!({"state":"sent"})]),
                Vec::new(),
            )
        });
        advance_or_supersede(&mut op, true, true);
        assert_eq!(op.stage, OperationPhase::Writing);
        assert_eq!(op.task, Some(1));
        assert!(op.completed_at.is_none());
    }

    #[test]
    fn disabled_cwmp_reports_disabled_management_state() {
        let mut d = super::super::tests::operation(OperationPhase::Completed, false).device;
        d.bootstrap_adapter = "none".into();
        d.provisioning_adapter = "hl202-mqtt".into();
        assert!(!d.cwmp_enabled());

        d.provisioning_adapter = "none".into();
        assert!(!d.cwmp_enabled());

        d.bootstrap_adapter = "manual-acs".into();
        assert!(d.cwmp_enabled());

        d.bootstrap_adapter = "none".into();
        d.provisioning_adapter = "hl202-cu".into();
        assert!(d.cwmp_enabled());
    }
}
