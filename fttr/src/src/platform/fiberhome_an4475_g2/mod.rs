// Copyright 2026 Lazuli Kao / openwrt-fttr contributors
// SPDX-License-Identifier: Apache-2.0

use super::adapter::{
    IdentificationStatus, PlatformAdapter, PlatformDescriptor, PlatformId, PlatformImplementation,
};

#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod activation;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
mod backend;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod calibration;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub(crate) mod diagnostics;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod downstream;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod gdba;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod initialization;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod interrupts;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod management;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod olt;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod omci;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod optical;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod polling;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod profile;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod protocol;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub(crate) mod runtime;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod serdes_apb;
#[cfg(all(test, feature = "platform-fiberhome-an4475-g2"))]
mod test_support;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
mod transport;
#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub mod upstream;

#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub const ONU_COUNT: usize = 16;

#[cfg(feature = "platform-fiberhome-an4475-g2")]
const ONU_IDS: [u8; ONU_COUNT] = {
    let mut ids = [0; ONU_COUNT];
    let mut index = 0;
    while index < ONU_COUNT {
        ids[index] = index as u8;
        index += 1;
    }
    ids
};

#[cfg(feature = "platform-fiberhome-an4475-g2")]
pub const RESOURCES: super::resources::Resources = super::resources::Resources {
    onu_ids: &ONU_IDS,
    management_interfaces: &["fttr0"],
    capture_interface: Some("fttr0"),
    downstream_interfaces: &[],
    allocations: &allocations(),
};

#[cfg(feature = "platform-fiberhome-an4475-g2")]
const fn allocations() -> [super::resources::OnuResources; ONU_COUNT] {
    let mut entries = [super::resources::OnuResources {
        onu_id: 0,
        alloc_id: 0,
        gem_id: 0,
        interface: None,
    }; ONU_COUNT];
    let mut index = 0;
    while index < entries.len() {
        entries[index] = super::resources::OnuResources {
            onu_id: index as u8,
            alloc_id: 0x100 + index as u16,
            gem_id: 0x100 + index as u16,
            interface: None,
        };
        index += 1;
    }
    entries
}

#[derive(Debug)]
pub struct An4475G2;

impl PlatformAdapter for An4475G2 {
    const DESCRIPTOR: PlatformDescriptor = PlatformDescriptor {
        id: PlatformId::FiberhomeAn4475G2,
        vendor: "FiberHome",
        model: "AN4475-G2",
        // Exact PonWrt board DTS identity, not the original generic AN7581 EVB ID.
        // This confirms source identity only, not live wiring or hardware readiness.
        device_tree_compatibles: &["fiberhome,an4475-g2"],
        board_names: &["fiberhome,an4475-g2"],
        identification: IdentificationStatus::Verified,
        implementation: PlatformImplementation::Runtime,
    };
}
