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

pub use crate::domain::WifiSecurity;

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

pub use crate::domain::{
    BindingIntent, ConnectionMode, EndpointRef, VlanMode, WanIntent, WlanIntent,
};

#[derive(Clone)]
pub struct Device {
    pub id: String,
    pub optical_sn: String,
    pub cwmp_id: String,
    pub omci_adapter: String,
    pub bootstrap_adapter: String,
    pub provisioning_adapter: String,
    /// Runtime discovery result; never a configuration override.
    pub resolved_contract: Option<String>,
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
        match self.provisioning_adapter.as_str() {
            "none" => "disabled",
            "hl202-mqtt" => "command",
            _ => "acs", // ACS identifiers are validated by the configuration compiler.
        }
    }

    pub fn cwmp_enabled(&self) -> bool {
        self.bootstrap_adapter != "none" || self.backend() == "acs"
    }

    pub fn backend_view(&self) -> serde_json::Value {
        serde_json::json!({"configured":self.provisioning_adapter,
            "effective":self.backend(), "available":[self.backend()], "reason":""})
    }

    pub fn contract(&self) -> &'static crate::cwmp::contract::Contract {
        if let Some(id) = &self.resolved_contract {
            return crate::cwmp::registry::lookup(id).expect("discovered contract registered");
        }
        crate::cwmp::registry::lookup(if self.provisioning_adapter == "hl202-mqtt" {
            "hl202-cu"
        } else {
            &self.provisioning_adapter
        })
        .expect("validated provisioning contract")
    }

    pub fn omci_management(&self) -> bool {
        self.bootstrap_adapter == "omci-acs"
    }
}

#[derive(Clone, Default)]
pub struct Config {
    pub omci_auth: crate::omci_auth::Config,
    pub onu_pins: BTreeMap<String, u8>,
    pub revision: String,
    pub business_revision: Option<String>,
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
impl Config {
    pub fn business_revision(&self) -> &str {
        self.business_revision.as_deref().unwrap_or(&self.revision)
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn optical_adapter_does_not_select_provisioning_transport() {
        let mut device = Device {
            id: "child".into(),
            optical_sn: "H3CT00000001".into(),
            cwmp_id: "pending:H3CT00000001".into(),
            omci_adapter: "hl202".into(),
            bootstrap_adapter: "none".into(),
            provisioning_adapter: "hl202-mqtt".into(),
            resolved_contract: None,
            connection_request: ConnectionRequest::default(),
            items: vec![],
            wifi_cleanup: "preserve".into(),
            wifi_preserve: BTreeSet::new(),
        };
        assert_eq!(device.backend(), "command");
        device.omci_adapter = "generic".into();
        assert_eq!(device.backend(), "command");
        device.provisioning_adapter = "hl202-cu".into();
        assert_eq!(device.backend(), "acs");
        device.omci_adapter = "hwtc".into();
        assert_eq!(device.backend(), "acs");
    }
}
