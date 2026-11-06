//! Internal business models. Serialized names are part of the existing status API.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }

        impl $name {
            pub(super) const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

wire_enum!(TaskOutcome {
    Queued => "queued",
    Sent => "sent",
    Completed => "completed",
    AcceptedPendingApply => "accepted_pending_apply",
    Unknown => "unknown",
    Fault => "fault",
    Expired => "expired",
    Missing => "missing",
});

impl TaskOutcome {
    pub(super) fn from_task(task: &serde_json::Value) -> Self {
        serde_json::from_value(task["state"].clone()).unwrap_or(Self::Missing)
    }
}

wire_enum!(ItemOutcome {
    Verified => "verified",
    Failed => "failed",
    Blocked => "blocked",
    Unknown => "unknown",
    AcceptedPendingApply => "accepted_pending_apply",
    AcceptedUnverified => "accepted_unverified",
    ReconcileRequired => "reconcile_required",
    Drift => "drift",
    Created => "created",
    Deleted => "deleted",
    Disabled => "disabled",
});

impl ItemOutcome {
    pub(super) fn from_result(result: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(result["state"].clone()).ok()
    }
}

wire_enum!(WanOwnershipState {
    Ready => "ready",
    Protected => "protected",
    AddingParent => "adding_parent",
    DiscoveringChild => "discovering_child",
    AddingChild => "adding_child",
    Deleting => "deleting",
});

wire_enum!(WifiPendingAction {
    Adding => "adding",
    Disabling => "disabling",
    Deleting => "deleting",
});

pub use fttr_api::{
    OperationPhase, SyncItemKind as ItemKind, WanStage as WanPhase, WifiStage as WifiPhase,
};

wire_enum!(ParameterKind {
    String => "string",
    Boolean => "boolean",
    UnsignedInt => "unsignedInt",
});

wire_enum!(WifiSecurity {
    None => "none",
    Wpa2Psk => "wpa2-psk",
    Wpa3Sae => "wpa3-sae",
    Wpa2PskWpa3Sae => "wpa2-psk/wpa3-sae",
    WpaPskWpa2Psk => "wpa-psk/wpa2-psk",
});

/// Values retain their CWMP lexical representation (including 0/1 booleans).
#[derive(Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub value: String,
    pub kind: ParameterKind,
    pub secret: bool,
}

#[derive(Clone)]
pub struct Item {
    pub wlan_intent: Option<WlanIntent>,
    pub binding_intent: Option<BindingIntent>,
    pub id: String,
    pub kind: ItemKind,
    pub service_type: String,
    pub parameters: Vec<Parameter>,
    pub guard: Option<String>,
    pub blocked: Option<String>,
    pub wan_ref: Option<String>,
    pub auto_instance: Option<u32>,
    pub wan_intent: Option<WanIntent>,
}

#[derive(Clone, Debug)]
pub struct WlanIntent {
    pub endpoint: EndpointRef,
    pub ssid: String,
    pub enabled: Option<bool>,
    pub broadcast: Option<bool>,
    pub security: Option<WifiSecurity>,
    pub key: Option<String>,
}
#[derive(Clone, Debug)]
pub struct BindingIntent {
    pub endpoints: Vec<EndpointRef>,
}

/// Desired business state. Neither vendor fields nor wire paths belong here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WanIntent {
    pub services: Vec<String>,
    pub vlan: VlanMode,
    pub priority: u8,
    pub mode: ConnectionMode,
    pub bindings: Vec<EndpointRef>,
    pub internet: bool,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VlanMode {
    Untagged,
    Tagged(u16),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionMode {
    Bridge,
    Routed,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EndpointRef {
    Lan(u32),
    Ssid { radio: u8, slot: u32 },
    Named(String),
}

#[derive(Clone)]
pub struct Device {
    pub id: String,
    pub optical_sn: String,
    pub cwmp_id: String,
    pub adapter: String,
    pub provisioning_method: String,
    pub cwmp_contract: String,
    pub omci_management: bool,
    pub connection_request: ConnectionRequest,
    pub items: Vec<Item>,
    pub wifi_cleanup: String,
    pub wifi_preserve: BTreeSet<u32>,
}

#[derive(Clone)]
pub struct ConnectionRequest {
    pub enabled: bool,
    pub defaults: bool,
    pub username: String,
    pub password: String,
    pub port: std::num::NonZeroU16,
    pub(crate) authorization: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for ConnectionRequest {
    fn default() -> Self {
        Self {
            enabled: true,
            defaults: true,
            username: String::new(),
            password: String::new(),
            port: const {
                match std::num::NonZeroU16::new(7547) {
                    Some(port) => port,
                    None => panic!("invalid CR port"),
                }
            },
            authorization: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

impl ConnectionRequest {
    pub fn credentials_available(&self) -> bool {
        !self.defaults
            && !self.username.is_empty()
            && !self.password.is_empty()
            && !self.username.contains(':')
            && [&self.username, &self.password]
                .iter()
                .all(|value| value.len() <= 1024 && !value.chars().any(char::is_control))
    }

    pub fn effective_freshness(&self, configured: u64) -> u64 {
        if self.enabled {
            configured.max(2 * 43200)
        } else {
            configured
        }
    }

    #[cfg(feature = "acs")]
    pub(crate) fn same_policy(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.defaults == other.defaults
            && self.username == other.username
            && self.password == other.password
            && self.port == other.port
            && std::sync::Arc::ptr_eq(&self.authorization, &other.authorization)
    }
}

impl Device {
    pub fn backend(&self) -> &'static str {
        if self.provisioning_method != "acs"
            && crate::devices::supports_command_provisioning(&self.adapter)
        {
            "command"
        } else {
            "acs"
        }
    }

    pub fn backend_view(&self) -> serde_json::Value {
        let command = crate::devices::supports_command_provisioning(&self.adapter);
        serde_json::json!({"configured":if self.provisioning_method.is_empty() {"auto"} else {&self.provisioning_method},
            "effective":self.backend(), "available":(if command {vec!["command"]} else {vec![]}).into_iter().chain(if cfg!(feature="acs") {vec!["acs"]} else {vec![]}).collect::<Vec<_>>(),
            "reason":if self.provisioning_method == "command" && !command {"command backend not compiled; using ACS"} else if self.backend() == "acs" && !cfg!(feature="acs") {"ACS backend not compiled"} else {""}})
    }

    pub fn contract(&self) -> &'static crate::devices::cwmp::contract::Contract {
        crate::devices::cwmp::configuration_contract(
            &self.cwmp_contract,
            self.optical_sn.get(..4).unwrap_or(""),
        )
        .expect("configuration must validate the CWMP contract")
    }
}

#[derive(Clone, Default)]
pub struct Config {
    pub onu_pins: BTreeMap<String, u8>,
    pub revision: String,
    pub enabled: bool,
    pub automatic: bool,
    pub triggers: BTreeSet<String>,
    pub interval_seconds: u64,
    pub freshness: u64,
    pub devices: Vec<Device>,
    pub assignments: BTreeMap<String, (String, String)>,
    pub groups: BTreeMap<String, String>,
    pub management: Option<Management>,
    pub management_network: Option<super::management_network::Desired>,
    pub allow_unsupported_as_bridge: bool,
    pub labels: BTreeMap<String, String>,
}

pub use fttr_api::ManagementDesired as Management;

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn per_device_selection_respects_compiled_backends() {
        let mut device = Device {
            id: "child".into(),
            optical_sn: "H3CT00000001".into(),
            cwmp_id: "pending:H3CT00000001".into(),
            adapter: "hl202".into(),
            provisioning_method: String::new(),
            cwmp_contract: "auto".into(),
            omci_management: false,
            connection_request: ConnectionRequest::default(),
            items: vec![],
            wifi_cleanup: "preserve".into(),
            wifi_preserve: BTreeSet::new(),
        };
        let expected = if cfg!(all(feature = "device-hl202", feature = "mqtt")) {
            "command"
        } else {
            "acs"
        };
        assert_eq!(device.backend(), expected);
        device.provisioning_method = "command".into();
        assert_eq!(device.backend(), expected);
        assert_eq!(device.backend_view()["configured"], "command");
        device.provisioning_method = "acs".into();
        assert_eq!(device.backend(), "acs");
        device.adapter = "hwtc".into();
        device.provisioning_method.clear();
        assert_eq!(device.backend(), "acs");
    }
}
