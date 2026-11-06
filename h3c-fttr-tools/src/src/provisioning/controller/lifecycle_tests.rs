use super::tests::operation;
use super::*;

fn configured() -> Controller {
    let op = operation(OperationPhase::Completed, false);
    let device = op.device.clone();
    let mut c = Controller::default();
    c.config.devices.push(device.clone());
    c.recovered.insert(device.id.clone(), op.view());
    c.management
        .insert(device.id.clone(), json!({"state":"online"}));
    c.last_boot.insert(device.id.clone(), 10);
    c.last_online.insert(device.id.clone());
    c.omci_management
        .insert(device.optical_sn.clone(), json!({"state":"verified"}));
    c.optical_epochs.insert(device.optical_sn.clone(), (1, 10));
    c.owned.insert(
        device.cwmp_id.clone(),
        BTreeMap::from([(
            "old-wan".into(),
            wan::Owned {
                parent: 4,
                connection: 1,
                state: super::super::model::WanOwnershipState::Ready,
                service_list: "INTERNET".into(),
                kind: "WANIPConnection".into(),
                baseline: None,
            },
        )]),
    );
    c.wifi_owned
        .insert(device.cwmp_id.clone(), wifi::Registry::default());
    c
}

#[test]
fn removed_devices_release_all_volatile_state() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Completed, false));

    c.replace_config(Config::default()).unwrap();

    assert!(c.operations.is_empty());
    assert!(c.recovered.is_empty());
    assert!(c.management.is_empty());
    assert!(c.last_boot.is_empty());
    assert!(c.last_online.is_empty());
    assert!(c.omci_management.is_empty());
    assert!(c.optical_epochs.is_empty());
    assert!(c.owned.is_empty());
    assert!(c.wifi_owned.is_empty());
    let history: Value = serde_json::from_str(&c.history()).unwrap();
    assert_eq!(history["operations"], json!([]));
    assert!(c.snapshot()["devices"].as_array().unwrap().is_empty());
}

#[test]
fn renamed_device_keeps_identity_ownership_but_drops_old_id_state() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Completed, false));
    let mut config = c.config.clone();
    config.devices[0].id = "renamed".into();

    c.replace_config(config).unwrap();

    assert!(c.operations.is_empty());
    assert!(c.recovered.is_empty());
    assert!(c.management.is_empty());
    assert!(c.last_boot.is_empty());
    assert!(c.last_online.is_empty());
    assert_eq!(c.owned["id"]["old-wan"].parent, 4);
    assert!(c.wifi_owned.contains_key("id"));
    assert!(c.omci_management.contains_key("H3CT12345678"));
    assert_eq!(
        c.snapshot()["devices"][0]["provisioning"]["wan_objects"]["old-wan"]["parent"],
        4
    );
}

#[test]
fn replacement_identity_drops_previous_device_observations() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Completed, false));
    let mut config = c.config.clone();
    config.devices[0].optical_sn = "H3CT87654321".into();
    config.devices[0].cwmp_id = "replacement".into();

    c.replace_config(config).unwrap();

    assert!(c.operations.is_empty());
    assert!(c.recovered.is_empty());
    assert!(c.management.is_empty());
    assert!(c.last_boot.is_empty());
    assert!(c.last_online.is_empty());
    assert!(c.omci_management.is_empty());
    assert!(c.owned.is_empty());
    assert!(c.wifi_owned.is_empty());
}

#[test]
fn active_operation_rejects_replacement_before_pruning() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Writing, false));
    let before = c.history();

    let result = c.replace_config(Config::default());

    assert!(result.is_err());
    assert_eq!(c.history(), before);
    assert_eq!(c.config.devices.len(), 1);
    assert!(c.management.contains_key("child"));
    assert_eq!(c.last_boot["child"], 10);
}

#[test]
fn reload_queues_latest_revision_without_pruning_and_activates_after_drain() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Writing, false));
    let old_revision = c.config.revision.clone();
    c.config_error = Some("old reload error".into());
    let mut config = c.config.clone();
    config.revision = "next".into();
    c.request_config(config.clone());
    assert_eq!(c.config.revision, old_revision);
    assert!(c.config_error.is_none());
    assert_eq!(c.snapshot()["config_load"]["state"], "waiting");
    assert_eq!(
        c.snapshot()["config_load"]["blocking_operations"][0]["operation_id"],
        c.operations["child"].id
    );
    assert!(c
        .start("child", false, false)
        .unwrap_err()
        .contains("reload pending"));
    config.revision = "latest".into();
    c.request_config(config);
    assert!(!c.activate_pending_config());
    c.operations.get_mut("child").unwrap().complete(100);
    c.attempts.insert("child".into(), 4);
    c.next_retry_at.insert("child".into(), u64::MAX);
    assert!(c.activate_pending_config());
    assert_eq!(c.config.revision, "latest");
    assert!(c.pending_config.is_none());
    assert!(c.attempts.is_empty());
    assert!(c.next_retry_at.is_empty());
    assert!(c.owned.contains_key("id"));
    assert_eq!(c.snapshot()["config_load"]["state"], "applied");
    let wire: fttr_api::ConfigLoadStatus = serde_json::from_value(c.config_load_view()).unwrap();
    assert_eq!(wire.requested_revision, "latest");
}

#[test]
fn unstarted_offline_operations_do_not_delay_reload_but_started_work_does() {
    let mut c = configured();
    let mut unstarted = operation(OperationPhase::Waiting, false);
    unstarted.task = None;
    c.operations.insert("child".into(), unstarted);
    let mut next = c.config.clone();
    next.revision = "next".into();
    c.request_config(next);
    assert_eq!(c.config.revision, "next");
    assert_eq!(c.operations["child"].stage, OperationPhase::Completed);

    let mut op = operation(OperationPhase::Waiting, false);
    op.work_started = true;
    c.operations.insert("child".into(), op);
    let mut next = c.config.clone();
    next.revision = "latest".into();
    c.request_config(next);
    assert_eq!(c.config.revision, "next");
    assert!(c.pending_config.is_some());
}

#[test]
fn reverting_to_active_revision_cancels_queued_reload_and_interrupted_is_terminal() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Writing, false));
    let active = c.config.clone();
    let mut next = active.clone();
    next.revision = "next".into();
    c.request_config(next.clone());
    c.request_config(active);
    assert!(c.pending_config.is_none());
    c.operations.get_mut("child").unwrap().stage = OperationPhase::Interrupted;
    c.request_config(next);
    assert_eq!(c.config.revision, "next");
    assert!(c.pending_config.is_none());
}

#[test]
fn unstarted_command_job_is_superseded_without_duplicate_item_results() {
    let mut c = configured();
    let mut op = operation(OperationPhase::Waiting, false);
    op.task = None;
    op.device.provisioning_method = "command".into();
    c.config.devices[0] = op.device.clone();
    c.operations.insert("child".into(), op);
    let mut next = c.config.clone();
    next.revision = "next".into();
    c.request_config(next);
    assert_eq!(c.config.revision, "next");
    let view = c.operations["child"].view();
    assert_eq!(view["items"].as_array().unwrap().len(), 1);
    assert_eq!(view["items"][0]["state"], "blocked");
    assert!(c.operations["child"].command_job.is_none());
}

#[test]
fn unchanged_device_preserves_cleanup_ownership_on_reload() {
    let mut c = configured();
    let mut config = c.config.clone();
    config.revision = "changed".into();
    config.devices[0].items.clear();

    c.replace_config(config).unwrap();

    assert_eq!(c.owned["id"]["old-wan"].parent, 4);
    assert!(c.wifi_owned.contains_key("id"));
    assert!(c.recovered.contains_key("child"));
    assert!(c.management.contains_key("child"));
    assert_eq!(c.last_boot["child"], 10);
}

#[test]
fn startup_filters_history_and_ownership_to_configured_identities() {
    let mut c = configured();
    let mut stale = operation(OperationPhase::Completed, false).view();
    stale["device"] = json!("removed");
    stale["operation_id"] = json!(90);
    let mut interrupted = c.recovered["child"].clone();
    interrupted["state"] = json!("writing");
    let data = json!({"operations":[stale,interrupted],"owned":{
        "id":c.owned["id"],"removed":c.owned["id"]
    }})
    .to_string();

    c.recover(data.as_bytes());
    c.retain_configured_state();

    assert_eq!(c.next_id, 90);
    assert_eq!(c.recovered.len(), 1);
    assert_eq!(c.owned.len(), 1);
    assert_eq!(
        c.snapshot()["devices"][0]["provisioning"]["state"],
        "interrupted"
    );
    assert_eq!(c.owned["id"]["old-wan"].parent, 4);
}

#[test]
fn live_operation_replaces_shadow_in_history_without_duplicate_entries() {
    let mut c = configured();
    c.operations
        .insert("child".into(), operation(OperationPhase::Completed, false));

    let history: Value = serde_json::from_str(&c.history()).unwrap();

    assert_eq!(history["operations"].as_array().unwrap().len(), 1);
    assert_eq!(history["operations"][0]["state"], "completed");
    assert_eq!(history["owned"]["id"]["old-wan"]["parent"], 4);
}

#[test]
fn partial_start_preserves_other_results_before_releasing_recovered_shadow() {
    let mut c = configured();
    c.recovered.get_mut("child").unwrap()["items"] = json!([
        {"id":"wifi","state":"failed"}, {"id":"untouched","state":"verified"}
    ]);

    c.start_selected("child", false, false, Some("wifi"))
        .unwrap();

    assert!(c.recovered.is_empty());
    assert_eq!(
        c.operations["child"].results["untouched"]["state"],
        "verified"
    );
    assert!(!c.operations["child"].results.contains_key("wifi"));
    assert_eq!(c.operations["child"].wan.owned["old-wan"].parent, 4);
    assert_eq!(
        c.snapshot()["devices"][0]["provisioning"]["state"],
        "waiting"
    );
}

#[test]
fn full_start_releases_recovered_shadow() {
    let mut c = configured();

    c.start("child", false, false).unwrap();

    assert!(c.recovered.is_empty());
    let history: Value = serde_json::from_str(&c.history()).unwrap();
    assert_eq!(history["operations"].as_array().unwrap().len(), 1);
    assert_eq!(history["operations"][0]["mode"], "plan");
}

#[test]
fn program_start_trigger_is_pending_only_for_configured_devices() {
    let mut c = configured();
    c.config.triggers.insert("program_start".into());
    c.startup_pending.insert("child".into());

    assert!(c.startup_trigger_pending("child"));
    assert!(!c.startup_trigger_pending("other"));

    c.config.triggers.remove("program_start");
    assert!(!c.startup_trigger_pending("child"));
}

fn template_config() -> Config {
    crate::provisioning::templates::compile(
        "config template 't'\nconfig group 'g'\n option template 't'\nconfig device 'child'\n option optical_sn 'H3CT12345678'\n option adapter 'hl202'\n option provisioning_method 'acs'\n option group 'g'\n",
    ).unwrap()
}

fn discovered(id: &str) -> Value {
    json!({"devices":{id:{"product_class":"HL202-DU-C","serial":"H3CT12345678"}}})
}

#[test]
#[cfg_attr(not(feature = "device-hl202"), ignore = "requires HL202 adapter")]
fn startup_preserves_template_safety_and_ownership_until_binding_is_discovered() {
    for state in ["writing", "completed"] {
        let previous = configured();
        let mut history: Value = serde_json::from_str(&previous.history()).unwrap();
        history["operations"][0]["state"] = json!(state);
        history["operations"][0]["items"] = json!([{"id":"wifi","state":"unknown"}]);
        let mut c = Controller::default();
        c.config = template_config();
        assert_eq!(c.config.devices[0].cwmp_id, "pending:H3CT12345678");
        c.recover(history.to_string().as_bytes());

        c.retain_configured_state();

        assert_eq!(
            c.recovered["child"]["state"],
            if state == "writing" {
                "interrupted"
            } else {
                "completed"
            }
        );
        assert_eq!(c.recovered["child"]["items"][0]["state"], "unknown");
        assert_eq!(c.owned["id"]["old-wan"].parent, 4);
        c.resolve_pending_identities(&discovered("id"));
        assert_eq!(c.config.devices[0].cwmp_id, "id");
        assert!(c.recovered.contains_key("child"));
        assert!(c.owned.contains_key("id"));
    }
}

#[test]
#[cfg_attr(not(feature = "device-hl202"), ignore = "requires HL202 adapter")]
fn unchanged_template_reload_preserves_resolved_binding_and_unknown_outcomes() {
    let mut c = configured();
    c.config = template_config();
    c.config.devices[0].cwmp_id = "id".into();
    let mut op = operation(OperationPhase::Completed, false);
    op.results
        .insert("wifi".into(), json!({"id":"wifi","state":"unknown"}));
    c.operations.insert("child".into(), op);

    c.replace_config(template_config()).unwrap();

    assert_eq!(c.config.devices[0].cwmp_id, "id");
    assert_eq!(c.operations["child"].results["wifi"]["state"], "unknown");
    assert_eq!(c.last_boot["child"], 10);
    assert!(c.last_online.contains("child"));
    assert!(c.management.contains_key("child"));
    assert!(c.owned.contains_key("id"));
    assert!(c.wifi_owned.contains_key("id"));
}

#[test]
#[cfg_attr(not(feature = "device-hl202"), ignore = "requires HL202 adapter")]
fn discovered_template_replacement_prunes_previous_resolved_history_and_ownership() {
    let previous = configured();
    let mut c = Controller::default();
    c.config = template_config();
    c.recover(previous.history().as_bytes());
    c.retain_configured_state();
    assert!(c.recovered.contains_key("child"));
    assert!(c.owned.contains_key("id"));

    c.resolve_pending_identities(&discovered("replacement"));

    assert_eq!(c.config.devices[0].cwmp_id, "replacement");
    assert!(c.recovered.is_empty());
    assert!(c.owned.is_empty());
    assert_eq!(
        c.snapshot()["devices"][0]["provisioning"]["state"],
        "not_started"
    );
}
