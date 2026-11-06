use super::*;
use serde_json::json;
use std::sync::{atomic::AtomicUsize, Arc};

#[test]
fn invalid_native_write_requests_never_reach_the_handler() {
    let calls = Arc::new(AtomicUsize::new(0));
    let invoked = calls.clone();
    let (name, handler) = bind(&NPU_SET, move |request| {
        invoked.fetch_add(1, Ordering::SeqCst);
        Ok(NpuSetReply::Success {
            enabled: request.enabled,
        })
    });
    for invalid in [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!({"enabled": "false", "bridge": "br-test"})),
        Some(json!({"enabled": false})),
    ] {
        assert_eq!(handler(&name, invalid.as_ref()), Err(2));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        handler(
            &name,
            Some(
                &json!({"enabled": false, "bridge": "br-test", "wan_upstream": false, "wan_downstream": false, "onu_bridge": true})
            )
        ),
        Ok(json!({"status": "success", "enabled": false}))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn native_dispatch_preserves_ubus_and_business_errors_without_retrying() {
    let calls = AtomicUsize::new(0);
    let (_, handler) = bind(&NPU_SET, move |_| {
        // Each invocation is handled once even if it fails.
        assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
        Err(4)
    });
    let request = json!({"enabled": true, "bridge": "br-test", "wan_upstream": false, "wan_downstream": false, "onu_bridge": true});
    assert_eq!(handler("npu_set", Some(&request)), Err(4));

    let (_, handler) = bind(&NPU_SET, |_| {
        Ok(NpuSetReply::Error {
            error: "adapter unavailable".into(),
        })
    });
    assert_eq!(
        handler("npu_set", Some(&request)),
        Ok(json!({"status": "error", "error": "adapter unavailable"}))
    );
}

#[cfg(feature = "device-hl202")]
#[test]
fn async_hl_results_follow_the_request_and_preserve_failure_evidence() {
    let target = HlOpticalSelection {
        optical_sn: "REDACTED".into(),
        onu_id: 0,
        binding_generation: "test-epoch".into(),
    };
    let query = |action| HlQuery::Run {
        request: HlCommandRequest {
            schema_version: 1,
            target: target.clone(),
            action,
        },
    };
    let exec = query(HlAction::Exec {
        script: "true".into(),
        timeout_seconds: 1,
    });
    let operation = json!({
        "target": {
            "mqtt_mac": "REDACTED", "optical_sn": null, "onu_id": null,
            "binding_generation": "test-epoch", "last_seen": 0, "reason": "test"
        },
        "result": {
            "target": "REDACTED", "status": "timeout", "output_truncated": true,
            "output": "partial output", "error": "timed out"
        },
        "binding_current": false
    });
    let result = json!({"schema_version": 1, "operation": operation});
    assert_eq!(
        normalize_hl_result(&exec, result.clone()),
        Ok(result.clone())
    );
    for action in [
        HlAction::AccountsRead,
        HlAction::AccountSetPassword {
            entry: 0,
            password: String::new(),
        },
        HlAction::AccountSetActive {
            entry: 1,
            active: false,
        },
    ] {
        let accounts = query(action);
        assert_eq!(normalize_hl_result(&accounts, result.clone()), Err(9));
        let mut payload = result.clone();
        payload["accounts"] = json!(null);
        payload["write_status"] = json!("unconfirmed");
        assert_eq!(normalize_hl_result(&accounts, payload.clone()), Ok(payload));
    }
    let report = HlQuery::Report { target };
    assert_eq!(normalize_hl_result(&report, result), Err(9));
    let error =
        json!({"schema_version": 1, "error": {"code": "unavailable", "message": "offline"}});
    for request in [&exec, &report, &HlQuery::List] {
        assert_eq!(
            normalize_hl_result(request, error.clone()),
            Ok(error.clone())
        );
    }
}
