//! Offline AN4475-G2 / FSL GPON OLT coordination state-machine prototype.
//!
//! Integrates chip reset/init, interrupt poll dispatch, SN discovery (O3),
//! ranging/activation (O4/O5), dynamic GDBA `B_ALL_SET_THD` recalculation,
//! OMCI TX/RX, and ONU deactivation behind `Bus` without wiring into
//! `platform::backend::resolve()` until on-board hardware validation passes.

use super::{
    activation::{
        self, assign_onu_id, configure_gem_port, configure_port_id_ploam, eqd_from_ranging,
        range_request, ranging_time_ploam, remove_onu, set_onu_active_bit,
        set_onu_upstream_rate_2p5g, ALM_PROC_ONU_ACTIVE_CFG, BMP_CFG_ONU_ACTIVE,
        PLOAM_DEFAULT_REPEAT,
    },
    downstream::SendError,
    gdba::{
        configure_alloc, read_b_all_set_thd, read_eight_times_dba_granularity,
        recalculate_b_all_bandwidth, GdbaAllocConfig, GdbaGapConfig,
    },
    initialization::{
        self,
        defaults::{
            apply_gpon_defaults, configure_nni_vport, DEFAULT_NNI_ETH_TPID,
            DEFAULT_NNI_GEMPORT_OFFSET,
        },
        InitConfig,
    },
    interrupts::{
        poll_and_clear_pending, set_all_gpon_int_masks, set_mask, GponInterrupt, SOFT_RDWR_OMCI_STA,
    },
    management::Management,
    polling::PollPolicy,
    protocol::{Bus, Fsl},
    upstream::{OmciFrame, PloamRecord, ReceiveError},
};
use crate::pon_engine::ploam::PloamFrame;
use std::{collections::BTreeMap, io, num::NonZeroU16, time::Duration};

/// Upstream burst rate mode recovered from `fslstack_gpon_ploamu_serial_number_process`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnuUpstreamRate {
    Rate1p25G,
    Rate2p5G,
}

impl OnuUpstreamRate {
    /// Decode the rate field used by the stock serial-number dispatcher.
    pub fn from_metadata(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Rate1p25G),
            1 => Some(Self::Rate2p5G),
            _ => None,
        }
    }

    const fn stock_onu_type(self) -> u8 {
        match self {
            Self::Rate1p25G => 1,
            Self::Rate2p5G => 7,
        }
    }
}

/// Local activation progress; `Active` does not independently confirm ONU O5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnuState {
    Ranging,
    Active,
}

/// Software mirror of an active or ranging ONU managed on the FSL PON MAC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedOnu {
    pub onu_id: u8,
    pub onu_index: u16,
    pub serial_number: [u8; 8],
    pub rate: OnuUpstreamRate,
    pub eqd: u32,
    pub state: OnuState,
}

/// Events collected by a single `FslOltSession::poll_once` cycle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OltPollReport {
    pub interrupts: Vec<GponInterrupt>,
    pub ploams: Vec<PloamRecord>,
    /// Records skipped for the same metadata flag as the stock dispatcher.
    pub flagged_ploams: usize,
    pub omci_frames: Vec<OmciFrame>,
}

/// Timing and capability configuration for `FslOltSession`.
#[derive(Clone, Copy, Debug)]
pub struct OltSessionConfig {
    pub init: InitConfig,
    pub nni_gem_port_offset: u16,
    pub nni_eth_tpid: u16,
    pub tx_poll: PollPolicy,
    pub rx_poll: PollPolicy,
    pub clear_poll: PollPolicy,
    pub ploam_repeat: u8,
    pub o3_range_delay: Duration,
    pub o4_ranging_delay: Duration,
    pub gap_config: GdbaGapConfig,
}

impl Default for OltSessionConfig {
    fn default() -> Self {
        let fast_poll = PollPolicy {
            max_reads: NonZeroU16::new(8).unwrap(),
            interval: Duration::from_millis(1),
        };
        Self {
            init: InitConfig::default(),
            nni_gem_port_offset: DEFAULT_NNI_GEMPORT_OFFSET,
            nni_eth_tpid: DEFAULT_NNI_ETH_TPID,
            tx_poll: fast_poll,
            rx_poll: PollPolicy {
                max_reads: NonZeroU16::new(2).unwrap(),
                interval: Duration::ZERO,
            },
            clear_poll: fast_poll,
            ploam_repeat: PLOAM_DEFAULT_REPEAT,
            o3_range_delay: Duration::from_millis(50),
            o4_ranging_delay: Duration::from_millis(10),
            gap_config: GdbaGapConfig::default(),
        }
    }
}

/// Converts a 13-byte `pon_engine` PLOAM frame into the 12-byte FSL downstream word input.
pub fn to_downstream_ploam(frame: &PloamFrame) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0] = frame.onu_id;
    out[1] = frame.message_id;
    out[2..12].copy_from_slice(&frame.payload);
    out
}

/// Parses an upstream `PloamRecord` into a `pon_engine::ploam::PloamFrame`.
pub fn parse_upstream_ploam(record: &PloamRecord) -> Option<PloamFrame> {
    PloamFrame::from_bytes(&record.frame)
}

fn map_send_err(err: SendError) -> io::Error {
    err.source
}

fn map_recv_err(err: ReceiveError) -> io::Error {
    err.source
}

/// Stateful offline GPON OLT controller driving the FSL SPI primitives over a generic `Bus`.
pub struct FslOltSession<B: Bus> {
    mgmt: Management<B>,
    config: OltSessionConfig,
    initialized: bool,
    onu_id_offset: u8,
    base_b_all_set_thd: u16,
    dba_granularity_8x: u32,
    onus: BTreeMap<u8, ManagedOnu>,
    /// Per-session PON sequence; this alone does not implement the stock ACK queue.
    next_port_id_sequence: u16,
}

impl<B: Bus> FslOltSession<B> {
    pub fn new(bus: B, config: OltSessionConfig) -> Self {
        Self {
            mgmt: Management::with_omci_cursor(Fsl::new(bus), 0),
            config,
            initialized: false,
            onu_id_offset: 0,
            base_b_all_set_thd: 0,
            dba_granularity_8x: 64,
            onus: BTreeMap::new(),
            next_port_id_sequence: 0,
        }
    }

    pub fn bus(&self) -> &B {
        &self.mgmt.fsl.bus
    }

    pub fn bus_mut(&mut self) -> &mut B {
        &mut self.mgmt.fsl.bus
    }

    pub(crate) fn fsl_mut(&mut self) -> &mut Fsl<B> {
        &mut self.mgmt.fsl
    }

    pub(crate) fn send_ploam(
        &mut self,
        frame: &[u8; 12],
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        self.require_initialized()?;
        self.mgmt
            .send_ploam(frame, self.config.tx_poll, checkpoint)
            .map_err(map_send_err)
    }

    pub fn config(&self) -> &OltSessionConfig {
        &self.config
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn base_b_all_set_thd(&self) -> u16 {
        self.base_b_all_set_thd
    }

    pub fn dba_granularity_8x(&self) -> u32 {
        self.dba_granularity_8x
    }

    pub fn onu(&self, onu_id: u8) -> Option<&ManagedOnu> {
        self.onus.get(&onu_id)
    }

    pub fn active_onus(&self) -> impl Iterator<Item = &ManagedOnu> {
        self.onus
            .values()
            .filter(|onu| onu.state == OnuState::Active)
    }

    fn require_initialized(&self) -> io::Result<()> {
        if !self.initialized {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "FSL OLT session is not initialized",
            ));
        }
        Ok(())
    }

    fn allocate_onu_slot(&self) -> io::Result<(u16, u8)> {
        let max_onus = self.config.init.max_onu_id.min(128);
        for onu_idx in 0..max_onus {
            if self.onus.values().any(|onu| onu.onu_index == onu_idx) {
                continue;
            }
            let raw_id = u16::from(self.onu_id_offset) + onu_idx;
            let onu_id = u8::try_from(raw_id).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "allocated ONU-ID exceeds u8")
            })?;
            return Ok((onu_idx, onu_id));
        }
        Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "no free ONU-ID slots remain",
        ))
    }

    /// Executes full hardware reset release, static GPON/GDBA/BMP/NNI/GEM defaults,
    /// baseline GDBA threshold capture, and default interrupt mask setup.
    pub fn initialize(
        &mut self,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        initialization::release_reset(&mut self.mgmt.fsl, &self.config.init, checkpoint)?;
        self.onu_id_offset = apply_gpon_defaults(&mut self.mgmt.fsl, self.config.init.max_onu_id)?;
        configure_nni_vport(
            &mut self.mgmt.fsl,
            self.config.nni_gem_port_offset,
            self.config.nni_eth_tpid,
        )?;
        self.base_b_all_set_thd = read_b_all_set_thd(&mut self.mgmt.fsl)?;
        self.dba_granularity_8x = read_eight_times_dba_granularity(&mut self.mgmt.fsl)?;
        set_mask(&mut self.mgmt.fsl, GponInterrupt::PloamRx, true)?;
        set_mask(&mut self.mgmt.fsl, GponInterrupt::OmciRx, true)?;
        set_all_gpon_int_masks(&mut self.mgmt.fsl, true)?;
        self.mgmt.omci_cursor = Some(0);
        self.mgmt.indeterminate = false;
        self.initialized = true;
        Ok(())
    }

    /// Polls and clears pending masked GPON interrupts (`fslstack_gpon_interrupt_dispatch`)
    /// and drains queued PLOAMu/OMCIu frames when signaled.
    pub fn poll_once(
        &mut self,
        max_ploam_reads: usize,
        max_omci_reads: usize,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<OltPollReport> {
        self.require_initialized()?;
        let interrupts = poll_and_clear_pending(&mut self.mgmt.fsl)?;
        let mut report = OltPollReport {
            interrupts,
            ploams: Vec::new(),
            flagged_ploams: 0,
            omci_frames: Vec::new(),
        };

        if report.interrupts.contains(&GponInterrupt::PloamRx) {
            for _ in 0..max_ploam_reads {
                let Some(record) = self
                    .mgmt
                    .receive_ploam(self.config.rx_poll, checkpoint)
                    .map_err(map_recv_err)?
                else {
                    break;
                };
                let is_last = record.last;
                if !record.stock_error_flag() {
                    report.ploams.push(record);
                } else {
                    report.flagged_ploams += 1;
                }
                if is_last {
                    break;
                }
            }
        }

        if report.interrupts.contains(&GponInterrupt::OmciRx) {
            for i in 0..max_omci_reads {
                if i > 0 && (self.mgmt.fsl.read_register(SOFT_RDWR_OMCI_STA)? & 1) == 0 {
                    break;
                }
                let Some(frame) = self
                    .mgmt
                    .receive_omci_frame(self.config.rx_poll, checkpoint)
                    .map_err(map_recv_err)?
                else {
                    break;
                };
                report.omci_frames.push(frame);
            }
        }

        Ok(report)
    }

    /// Handles `Serial_Number_ONU` (O3): configures upstream rate, sends
    /// `Assign_ONU-ID`, waits `o3_range_delay`, and triggers `range_request`
    /// (`fslstack_gpon_add_onu_o3_process`).
    pub fn process_o3_sn_discovery(
        &mut self,
        serial_number: [u8; 8],
        rate: OnuUpstreamRate,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<ManagedOnu> {
        self.require_initialized()?;
        if self
            .onus
            .values()
            .any(|onu| onu.serial_number == serial_number)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "duplicate ONU serial number",
            ));
        }

        let (onu_index, onu_id) = self.allocate_onu_slot()?;
        set_onu_upstream_rate_2p5g(
            &mut self.mgmt.fsl,
            onu_index,
            rate == OnuUpstreamRate::Rate2p5G,
        )?;
        assign_onu_id(
            &mut self.mgmt,
            onu_id,
            &serial_number,
            self.config.ploam_repeat,
            self.config.tx_poll,
            checkpoint,
        )
        .map_err(map_send_err)?;
        checkpoint(self.config.o3_range_delay)?;
        range_request(
            &mut self.mgmt.fsl,
            onu_index,
            self.config.tx_poll,
            checkpoint,
        )?;

        let entry = ManagedOnu {
            onu_id,
            onu_index,
            serial_number,
            rate,
            eqd: 0,
            state: OnuState::Ranging,
        };
        self.onus.insert(onu_id, entry);
        Ok(entry)
    }

    /// Handles ranged `Serial_Number_ONU` (O4->O5): sends `Ranging_Time`, activates
    /// alarm monitor and OMCC T-CONT/GEM, enables `BMP_CFG_ONU_ACTIVE`, sends
    /// `Configure_Port_ID`, and recalculates dynamic GDBA `B_ALL_SET_THD`
    /// (`fslstack_gpon_add_onu_o4_o5_process` + `fslstack_gpon_configure_omci_bw_and_port`).
    pub fn process_o4_o5_activation(
        &mut self,
        onu_id: u8,
        serial_number: &[u8; 8],
        raw_ranging_time: u32,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<ManagedOnu> {
        self.require_initialized()?;
        let existing = *self.onus.get(&onu_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "unknown ONU-ID in OLT session")
        })?;
        if &existing.serial_number != serial_number {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ranged serial number does not match assigned ONU-ID",
            ));
        }
        if existing.state == OnuState::Active {
            return Ok(existing);
        }

        let eqd = eqd_from_ranging(raw_ranging_time, existing.rate == OnuUpstreamRate::Rate2p5G);
        let rt_frame = ranging_time_ploam(onu_id, eqd);
        // The stock ranging-time message requests three transmissions.
        for _ in 0..3 {
            self.mgmt
                .send_ploam(&rt_frame, self.config.tx_poll, checkpoint)
                .map_err(map_send_err)?;
        }
        checkpoint(self.config.o4_ranging_delay)?;

        set_onu_active_bit(
            &mut self.mgmt.fsl,
            ALM_PROC_ONU_ACTIVE_CFG,
            existing.onu_index,
            true,
        )?;
        let omcc_cfg = GdbaAllocConfig::omcc(
            existing.onu_index as u8,
            existing.onu_index,
            self.dba_granularity_8x,
            existing.rate.stock_onu_type(),
        )?;
        configure_alloc(&mut self.mgmt.fsl, &omcc_cfg)?;
        set_onu_active_bit(
            &mut self.mgmt.fsl,
            BMP_CFG_ONU_ACTIVE,
            existing.onu_index,
            true,
        )?;
        configure_gem_port(
            &mut self.mgmt.fsl,
            existing.onu_index,
            0,
            u16::from(onu_id),
            true,
        )?;
        // Stock reserves its 16-bit per-PON sequence before attempting the send,
        // so even an ambiguous send failure must not reuse it.
        let sequence = self.next_port_id_sequence;
        self.next_port_id_sequence = sequence.wrapping_add(1);
        let port_frame = configure_port_id_ploam(onu_id, u16::from(onu_id), true, sequence)?;
        // All three stock Configure_Port_ID copies carry the same sequence.
        for _ in 0..3 {
            self.mgmt
                .send_ploam(&port_frame, self.config.tx_poll, checkpoint)
                .map_err(map_send_err)?;
        }
        recalculate_b_all_bandwidth(
            &mut self.mgmt.fsl,
            self.base_b_all_set_thd,
            &self.config.gap_config,
        )?;

        let updated = ManagedOnu {
            eqd,
            state: OnuState::Active,
            ..existing
        };
        self.onus.insert(onu_id, updated);
        Ok(updated)
    }

    /// Toggles an ONU's upstream FEC bitmap bit (`BMP_CFG_ONU_FEC_1G`) and
    /// recalculates `GDBA_B_ALL_CFG.B_ALL_SET_THD`.
    pub fn set_onu_us_fec_and_recalc(
        &mut self,
        onu_id: u8,
        enable: bool,
    ) -> io::Result<Option<u16>> {
        self.require_initialized()?;
        let onu = *self.onus.get(&onu_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "unknown ONU-ID in OLT session")
        })?;
        activation::set_onu_us_fec(&mut self.mgmt.fsl, onu.onu_index, enable)?;
        recalculate_b_all_bandwidth(
            &mut self.mgmt.fsl,
            self.base_b_all_set_thd,
            &self.config.gap_config,
        )
    }

    /// Sends one downstream OMCI payload to an active ONU via its OMCC GEM Port-ID.
    pub fn send_omci(
        &mut self,
        onu_id: u8,
        payload: &[u8],
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        self.require_initialized()?;
        let onu = self.onus.get(&onu_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "unknown ONU-ID in OLT session")
        })?;
        if onu.state != OnuState::Active {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "cannot send OMCI before ONU reaches Active state",
            ));
        }
        self.mgmt
            .send_omci_frame(
                u16::from(onu_id),
                true,
                payload,
                self.config.tx_poll,
                checkpoint,
            )
            .map_err(map_send_err)
    }

    /// Deactivates and cleans up an ONU (`fslstack_gpon_remove_onu`) and
    /// updates dynamic GDBA `B_ALL_SET_THD`.
    pub fn deactivate_onu(
        &mut self,
        onu_id: u8,
        extra_tconts: &[u8],
        extra_gems: &[u16],
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        self.require_initialized()?;
        let onu = *self.onus.get(&onu_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "unknown ONU-ID in OLT session")
        })?;

        let mut tconts = Vec::with_capacity(1 + extra_tconts.len());
        tconts.push(onu.onu_index as u8);
        tconts.extend_from_slice(extra_tconts);

        let mut gems = Vec::with_capacity(1 + extra_gems.len());
        gems.push(u16::from(onu_id));
        gems.extend_from_slice(extra_gems);

        remove_onu(
            &mut self.mgmt,
            onu_id,
            onu.onu_index,
            &tconts,
            &gems,
            self.config.ploam_repeat,
            self.config.tx_poll,
            self.config.clear_poll,
            checkpoint,
        )
        .map_err(map_send_err)?;
        recalculate_b_all_bandwidth(
            &mut self.mgmt.fsl,
            self.base_b_all_set_thd,
            &self.config.gap_config,
        )?;
        self.onus.remove(&onu_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
