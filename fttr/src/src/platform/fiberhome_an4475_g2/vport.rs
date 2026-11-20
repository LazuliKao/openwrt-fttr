//! Per-ONU IEEE 802.1ad (TPID 0x88a8) VPORT Linux VLAN subinterfaces over shared `fttr0`.
//!
//! FSL maps upstream/downstream NNI outer VLAN IDs using `VID = gem_port_id - NNI_GEMPORT_OFFSET`.
//! Programming the FSL NNI TPID to `0x88a8` lets the standard Linux `8021q` stack demultiplex
//! each ONU onto its own `fttr1..fttr16` netdev while preserving inner `0x8100` business/TR-069 VLANs.
use super::RESOURCES;
use std::{
    fs, io,
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Enough room for 1500B payload + 4B inner 802.1Q tag + 4B outer 802.1ad VPORT tag.
pub const NNI_MTU: u16 = 1508;
const DEFAULT_BRIDGE: &str = "br-lan";

pub fn gem_to_vid(gem_id: u16, offset: u16) -> io::Result<u16> {
    let vid = gem_id.checked_sub(offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("GEM {gem_id} is below NNI offset {offset}"),
        )
    })?;
    if !(1..=4094).contains(&vid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mapped NNI VLAN ID {vid} is outside 1..=4094"),
        ));
    }
    Ok(vid)
}

fn valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn configured_bridge() -> String {
    let Ok(output) = Command::new("uci")
        .args(["-q", "get", "fttr.offload.bridge"])
        .output()
    else {
        return DEFAULT_BRIDGE.into();
    };
    if !output.status.success() {
        return DEFAULT_BRIDGE.into();
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if valid_interface_name(&name) {
        name
    } else {
        DEFAULT_BRIDGE.into()
    }
}

fn run_ip_link(args: &[&str]) -> io::Result<()> {
    let output = Command::new("ip").arg("link").args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!(
            "ip link {} failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        )))
    }
}

pub fn prepare_nni_and_ports(nni: &str, gem_offset: u16, stopping: &AtomicBool) -> io::Result<()> {
    let mtu = NNI_MTU.to_string();
    run_ip_link(&["set", nni, "mtu", &mtu, "up"])?;
    let carrier_path = format!("/sys/class/net/{nni}/carrier");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut carrier_up = false;
    while Instant::now() < deadline && !stopping.load(Ordering::Acquire) {
        if fs::read_to_string(&carrier_path).is_ok_and(|val| val.trim() == "1") {
            carrier_up = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !carrier_up {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{nni} carrier did not come up within 5s"),
        ));
    }

    let bridge = configured_bridge();
    let bridge_exists = Path::new(&format!("/sys/class/net/{bridge}")).exists();
    for entry in RESOURCES.allocations {
        let Some(iface) = entry.interface else {
            continue;
        };
        let vid = gem_to_vid(entry.gem_id, gem_offset)?;
        let vid_str = vid.to_string();
        if Path::new(&format!("/sys/class/net/{iface}")).exists() {
            let _ = run_ip_link(&["set", iface, "down"]);
            let _ = run_ip_link(&["del", iface]);
        }
        run_ip_link(&[
            "add", "link", nni, "name", iface, "type", "vlan", "protocol", "802.1ad", "id",
            &vid_str,
        ])?;
        run_ip_link(&["set", iface, "mtu", "1500", "down"])?;
        if bridge_exists {
            let _ = run_ip_link(&["set", iface, "master", &bridge]);
        }
    }
    Ok(())
}

pub fn set_port_up(onu_id: u8, up: bool) -> io::Result<()> {
    let iface = RESOURCES.interface(onu_id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ONU {onu_id} has no assigned VPORT interface"),
        )
    })?;
    if !Path::new(&format!("/sys/class/net/{iface}")).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("VPORT interface {iface} does not exist"),
        ));
    }
    if up {
        let bridge = configured_bridge();
        if Path::new(&format!("/sys/class/net/{bridge}")).exists() {
            let _ = run_ip_link(&["set", iface, "master", &bridge]);
        }
        run_ip_link(&["set", iface, "up"])
    } else {
        run_ip_link(&["set", iface, "down"])
    }
}

pub fn bring_all_ports_down() {
    for &iface in RESOURCES.downstream_interfaces {
        if Path::new(&format!("/sys/class/net/{iface}")).exists() {
            let _ = run_ip_link(&["set", iface, "down"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gem_to_vid_maps_fsl_slots_and_rejects_out_of_range() {
        assert_eq!(gem_to_vid(0x100, 255).unwrap(), 1);
        assert_eq!(gem_to_vid(0x101, 255).unwrap(), 2);
        assert_eq!(gem_to_vid(0x10f, 255).unwrap(), 16);
        assert!(gem_to_vid(255, 255).is_err());
        assert!(gem_to_vid(254, 255).is_err());
        assert!(gem_to_vid(4095, 0).is_err());
    }

    #[test]
    fn interface_name_validation_rejects_traversal_and_spaces() {
        assert!(valid_interface_name("br-lan"));
        assert!(valid_interface_name("br_fttr.1"));
        assert!(!valid_interface_name(""));
        assert!(!valid_interface_name("../br-lan"));
        assert!(!valid_interface_name("br lan"));
        assert!(!valid_interface_name("0123456789abcdef"));
    }
}
