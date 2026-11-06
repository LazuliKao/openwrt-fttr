use crate::{bosa, pon_engine::ploam::PloamFrame};
use std::{
    io,
    os::fd::AsRawFd,
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

// Daemon-owned business allocations outlive a temporary OMCC diagnostic lease.
// Access is serialized by lock_onu; offline cleanup clears ownership before reuse.
static BUSINESS_ALLOCATIONS: AtomicU16 = AtomicU16::new(0);

pub fn forget_business_allocation(id: u8) {
    if id < 16 {
        BUSINESS_ALLOCATIONS.fetch_and(!(1u16 << id), Ordering::SeqCst);
    }
}

fn allocation_enabled_after_lease(owned: u16, id: u8) -> bool {
    owned & (1u16 << id) != 0
}
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// Project hardware serialization; a lease restores only its own bitmap bit.
pub fn update_onu_enable(id: u8, enabled: bool) -> io::Result<()> {
    if id > 15 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "ONU slot"));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open("/tmp/fttr-hardware.lock")?;
    if unsafe { flock(lock.as_raw_fd(), 2) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let value = bosa::read_fpga_reg(0x01000008)?;
    bosa::write_fpga_reg(0x01000008, enable_bit(value, id, enabled))
}
fn enable_bit(value: u32, id: u8, enabled: bool) -> u32 {
    if enabled {
        value | (1u32 << id)
    } else {
        value & !(1u32 << id)
    }
}
/// Serialize workers and diagnostic processes. Never unlink flock files.
pub fn lock_onu(id: u8) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(format!("/tmp/fttr-onu-{id}.lock"))?;
    if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
pub struct ChannelLease {
    id: u8,
    previous: u16,
}
impl ChannelLease {
    pub fn open(id: u8, channel: u16) -> io::Result<Self> {
        Ok(Self {
            id,
            previous: bosa::exchange_omci_channel(id, channel)?,
        })
    }
}
impl Drop for ChannelLease {
    fn drop(&mut self) {
        if let Err(e) = bosa::exchange_omci_channel(self.id, self.previous) {
            eprintln!("OMCI channel restore failed: {e}");
        }
    }
}
/// Reversible, per-ONU stock OMCC resource ownership (project platform object).
pub struct StockOmccLease {
    id: u8,
    saved: Vec<(u32, u32)>,
    disarmed: bool,
}
impl StockOmccLease {
    pub fn open(id: u8) -> io::Result<Self> {
        if id > 15 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ONU has no verified hardware slot",
            ));
        }
        let mut saved = Vec::new();
        for addr in [
            0x01000008,
            0x0500009c + 16 * id as u32,
            0x050000a4 + 16 * id as u32,
        ] {
            saved.push((addr, bosa::read_fpga_reg(addr)?));
        }
        let lease = Self {
            id,
            saved,
            disarmed: false,
        };
        for (i, &(addr, old)) in lease.saved.iter().enumerate() {
            let value = if i == 0 { old | (1 << id) } else { 0x11000000 };
            println!("STOCK_OMCC {addr:08x}: {old:08x}->{value:08x}");
            if i == 0 {
                update_onu_enable(id, true)?;
            } else {
                bosa::write_fpga_reg(addr, value)?;
            }
        }
        lease.configure_port(true)?;
        std::thread::sleep(Duration::from_millis(500));
        Ok(lease)
    }

    /// Disarm the lease teardown when transitioning to a persistent business service.
    /// This prevents dropping the Alloc-ID and FPGA hardware mappings when configuration succeeds.
    pub fn disarm(&mut self) {
        BUSINESS_ALLOCATIONS.fetch_or(1u16 << self.id, Ordering::SeqCst);
        self.disarmed = true;
    }

    fn configure_port(&self, enable: bool) -> io::Result<()> {
        let port = 0x100u16 + self.id as u16;
        let mut p = [0; 10];
        p[0] = (port >> 4) as u8;
        p[1] = (port << 4) as u8;
        p[2] = if enable { 1 } else { 0xff };
        for _ in 0..3 {
            bosa::send_ploam_msg(&PloamFrame::new(self.id, 0x0a, p).to_bytes())?;
        }
        Ok(())
    }
}
impl Drop for StockOmccLease {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        let enabled =
            allocation_enabled_after_lease(BUSINESS_ALLOCATIONS.load(Ordering::SeqCst), self.id);
        if let Err(e) = self.configure_port(enabled) {
            eprintln!("OMCC allocation restore failed: {e}");
        }
        for &(addr, value) in self.saved.iter().rev() {
            let result = if addr == 0x01000008 {
                update_onu_enable(self.id, value & (1u32 << self.id) != 0)
            } else {
                bosa::write_fpga_reg(addr, value)
            };
            if let Err(e) = result {
                eprintln!("OMCC restore {addr:08x} failed: {e}");
            }
        }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn temporary_diagnostics_preserve_only_owned_business_allocations() {
        use super::*;
        let mut service = StockOmccLease {
            id: 15,
            saved: Vec::new(),
            disarmed: false,
        };
        forget_business_allocation(15);
        assert!(!allocation_enabled_after_lease(
            BUSINESS_ALLOCATIONS.load(Ordering::SeqCst),
            15
        ));
        service.disarm();
        drop(service); // Persistent service must not run temporary teardown.
        assert!(allocation_enabled_after_lease(
            BUSINESS_ALLOCATIONS.load(Ordering::SeqCst),
            15
        ));
        forget_business_allocation(15);
        assert!(!allocation_enabled_after_lease(
            BUSINESS_ALLOCATIONS.load(Ordering::SeqCst),
            15
        ));
    }
    #[test]
    fn disabling_one_slot_preserves_other_live_slots() {
        let both = super::enable_bit(super::enable_bit(0x80000000, 1, true), 2, true);
        assert_eq!(super::enable_bit(both, 1, false), 0x80000004);
        assert_eq!(super::enable_bit(both, 2, false), 0x80000002);
    }
}
