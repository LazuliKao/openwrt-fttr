//! Offline AN4475-G2 / FSL GPON OLT coordination state-machine prototype.
//!
//! Integrates chip reset/init, interrupt poll dispatch, SN discovery (O3),
//! ranging/activation (O4/O5), dynamic GDBA `B_ALL_SET_THD` recalculation,
//! OMCI TX/RX, and ONU deactivation behind `Bus` without wiring into
//! `platform::backend::resolve()` until on-board hardware validation passes.

use super::{
    activation::{
        self, assign_onu_id, ber_interval_ploam, configure_gem_port, configure_port_id_ploam,
        eqd_from_ranging, range_request, ranging_time_ploam, remove_onu, set_onu_active_bit,
        set_onu_upstream_rate_2p5g, ALM_PROC_ONU_ACTIVE_CFG, BMP_CFG_ONU_ACTIVE,
        DEFAULT_BER_INTERVAL, PLOAM_DEFAULT_REPEAT, PLOAM_MSG_ACK, PLOAM_MSG_CONFIGURE_PORT_ID,
    },
    downstream::SendError,
    gdba::{
        configure_alloc, configure_ds_active_pending_slot, read_b_all_set_thd,
        read_eight_times_dba_granularity, recalculate_b_all_bandwidth, set_na_num, GdbaAllocConfig,
        GdbaGapConfig,
    },
    initialization::{
        self,
        defaults::{
            apply_gpon_defaults, configure_nni_vport, configure_rssi_ad_ctrl, DEFAULT_NNI_ETH_TPID,
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
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    num::NonZeroU16,
    time::{Duration, Instant},
};

/// Upstream burst rate mode recovered from `fslstack_gpon_ploamu_serial_number_process`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

/// Local activation progress matching stock states `3` (Ranging),
/// `7` (AwaitingPortIdAck), and `8` (Active after Configure_Port_ID ACK).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnuState {
    Ranging,
    AwaitingPortIdAck,
    Active,
}

/// Pending `Configure_Port_ID` (`0x0e`) ACK tracker matching stock
/// `g_fslstack_ploamd_ack_list_head`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingPortIdAck {
    pub onu_id: u8,
    pub gem_port_id: u16,
    pub sequence: u16,
    pub created_at: Instant,
}

/// Summary returned by periodic timeout sweeps (`check_timeouts`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimeoutReport {
    pub expired_acks: usize,
    pub timed_out_ranging: Vec<u8>,
    pub timed_out_port_id_ack: Vec<u8>,
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
    /// Stock `fslstack_gpon_ploamd_ack_list_timeout_check` expiry (5s).
    pub ack_timeout: Duration,
    /// Stock `fslstack_gpon_range_ack_state_timeout_check` expiry for O3/O4 (5s).
    pub state_timeout: Duration,
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
            ack_timeout: Duration::from_secs(5),
            state_timeout: Duration::from_secs(5),
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

/// Stateful GPON OLT controller driving the FSL SPI primitives over a generic `Bus`.
pub struct FslOltSession<B: Bus> {
    mgmt: Management<B>,
    config: OltSessionConfig,
    initialized: bool,
    onu_id_offset: u8,
    base_b_all_set_thd: u16,
    dba_granularity_8x: u32,
    gdba_pending_initialized: bool,
    active_na_tconts: BTreeSet<u8>,
    onus: BTreeMap<u8, ManagedOnu>,
    state_entered_at: BTreeMap<u8, Instant>,
    pending_port_id_acks: Vec<PendingPortIdAck>,
    blacklist: BTreeSet<([u8; 8], OnuUpstreamRate)>,
    whitelist: BTreeSet<([u8; 8], OnuUpstreamRate)>,
    auto_whitelist: bool,
    /// Per-session PON sequence matching stock `g_fslstack_ploam_seq`.
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
            gdba_pending_initialized: false,
            active_na_tconts: BTreeSet::new(),
            onus: BTreeMap::new(),
            state_entered_at: BTreeMap::new(),
            pending_port_id_acks: Vec::new(),
            blacklist: BTreeSet::new(),
            whitelist: BTreeSet::new(),
            auto_whitelist: true,
            next_port_id_sequence: 0,
        }
    }

    pub fn pending_port_id_acks(&self) -> &[PendingPortIdAck] {
        &self.pending_port_id_acks
    }

    pub fn add_to_blacklist(&mut self, serial_number: [u8; 8], rate: OnuUpstreamRate) {
        self.blacklist.insert((serial_number, rate));
    }

    pub fn remove_from_blacklist(&mut self, serial_number: [u8; 8], rate: OnuUpstreamRate) -> bool {
        self.blacklist.remove(&(serial_number, rate))
    }

    pub fn is_blacklisted(&self, serial_number: [u8; 8], rate: OnuUpstreamRate) -> bool {
        self.blacklist.contains(&(serial_number, rate))
    }

    pub fn add_to_whitelist(&mut self, serial_number: [u8; 8], rate: OnuUpstreamRate) {
        self.whitelist.insert((serial_number, rate));
    }

    pub fn remove_from_whitelist(&mut self, serial_number: [u8; 8], rate: OnuUpstreamRate) -> bool {
        self.whitelist.remove(&(serial_number, rate))
    }

    pub fn is_whitelisted(&self, serial_number: [u8; 8], rate: OnuUpstreamRate) -> bool {
        self.whitelist.contains(&(serial_number, rate))
    }

    pub fn set_auto_whitelist(&mut self, enabled: bool) {
        self.auto_whitelist = enabled;
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

    /// Programs a T-CONT in `GDBA_DS_PARA_TAB`, `GDBA_PARA_H/L_TAB`, the
    /// `GDBA_DS_ACTIVE_TAB`/`GDBA_DS_PENDING_TAB` scheduling ring, and
    /// `GDBA_NA_NUM_CFG` (`fsldrv_gpon_gdba_alloc_info_conf`).
    pub fn configure_tcont_alloc(&mut self, cfg: &GdbaAllocConfig) -> io::Result<()> {
        self.require_initialized()?;
        configure_alloc(&mut self.mgmt.fsl, cfg)?;
        match configure_ds_active_pending_slot(
            &mut self.mgmt.fsl,
            self.config.init.gdba_scene,
            cfg.onu_index,
            cfg.tcont,
            cfg.enable,
            &mut self.gdba_pending_initialized,
        ) {
            Ok(()) => {}
            Err(err) if !cfg.enable && err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        if cfg.enable && cfg.alloc_type == 1 {
            self.active_na_tconts.insert(cfg.tcont);
        } else {
            self.active_na_tconts.remove(&cfg.tcont);
        }
        set_na_num(&mut self.mgmt.fsl, self.active_na_tconts.len() as u16)
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
    /// board RSSI/GPIO optical enable (`fhdrv_olt_pon_rssi_ad_ctrl(0xff, 0xff, 1)`),
    /// baseline GDBA threshold capture, and default interrupt mask setup.
    pub fn initialize(
        &mut self,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        initialization::release_reset(&mut self.mgmt.fsl, &self.config.init, checkpoint)?;
        self.onu_id_offset = apply_gpon_defaults(&mut self.mgmt.fsl, self.config.init.max_onu_id)?;
        configure_rssi_ad_ctrl(&mut self.mgmt.fsl, None, None, Some(true))?;
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

    /// Handles `Serial_Number_ONU` (O3): enforces blacklist/whitelist policy,
    /// configures upstream rate, sends `Assign_ONU-ID`, waits `o3_range_delay`,
    /// and triggers `range_request` (`fslstack_gpon_add_onu_o3_process`).
    pub fn process_o3_sn_discovery(
        &mut self,
        serial_number: [u8; 8],
        rate: OnuUpstreamRate,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<ManagedOnu> {
        self.process_o3_sn_discovery_at(serial_number, rate, Instant::now(), checkpoint)
    }

    pub fn process_o3_sn_discovery_at(
        &mut self,
        serial_number: [u8; 8],
        rate: OnuUpstreamRate,
        now: Instant,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<ManagedOnu> {
        self.require_initialized()?;
        if self.blacklist.contains(&(serial_number, rate)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ONU serial number and rate are blacklisted",
            ));
        }
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
        if !self.whitelist.contains(&(serial_number, rate)) {
            if self.auto_whitelist {
                self.whitelist.insert((serial_number, rate));
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "ONU serial number and rate are not whitelisted",
                ));
            }
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
        self.state_entered_at.insert(onu_id, now);
        Ok(entry)
    }

    /// Handles ranged `Serial_Number_ONU` (O4): sends `Ranging_Time`, activates
    /// alarm monitor and OMCC T-CONT, enables `BMP_CFG_ONU_ACTIVE`, sends
    /// `Configure_Port_ID`, enqueues the pending ACK entry, and recalculates
    /// dynamic GDBA `B_ALL_SET_THD` (`fslstack_gpon_add_onu_o4_o5_process` +
    /// `fslstack_gpon_configure_omci_bw_and_port`).
    ///
    /// Matching stock behavior, this transitions the ONU to
    /// `OnuState::AwaitingPortIdAck` (state 7) and defers `PORTID_CFG_TAB` OMCC
    /// GEM port programming and `OnuState::Active` (state 8) until a matching
    /// upstream `Configure_Port_ID` ACK is processed by `process_ploam_ack`.
    pub fn process_o4_o5_activation(
        &mut self,
        onu_id: u8,
        serial_number: &[u8; 8],
        raw_ranging_time: u32,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<ManagedOnu> {
        self.process_o4_o5_activation_at(
            onu_id,
            serial_number,
            raw_ranging_time,
            Instant::now(),
            checkpoint,
        )
    }

    pub fn process_o4_o5_activation_at(
        &mut self,
        onu_id: u8,
        serial_number: &[u8; 8],
        raw_ranging_time: u32,
        now: Instant,
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
        if self
            .blacklist
            .contains(&(existing.serial_number, existing.rate))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ONU serial number and rate are blacklisted",
            ));
        }
        if !self
            .whitelist
            .contains(&(existing.serial_number, existing.rate))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ONU serial number and rate are not whitelisted",
            ));
        }
        if matches!(
            existing.state,
            OnuState::AwaitingPortIdAck | OnuState::Active
        ) {
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
        self.configure_tcont_alloc(&omcc_cfg)?;
        set_onu_active_bit(
            &mut self.mgmt.fsl,
            BMP_CFG_ONU_ACTIVE,
            existing.onu_index,
            true,
        )?;
        // Stock reserves its 16-bit per-PON sequence before attempting the send,
        // so even an ambiguous send failure must not reuse it.
        let sequence = self.next_port_id_sequence;
        self.next_port_id_sequence = sequence.wrapping_add(1);
        let gem_port_id = u16::from(onu_id);
        let port_frame = configure_port_id_ploam(onu_id, gem_port_id, true, sequence)?;
        // All three stock Configure_Port_ID copies carry the same sequence.
        for _ in 0..3 {
            self.mgmt
                .send_ploam(&port_frame, self.config.tx_poll, checkpoint)
                .map_err(map_send_err)?;
        }
        self.pending_port_id_acks.push(PendingPortIdAck {
            onu_id,
            gem_port_id,
            sequence,
            created_at: now,
        });
        recalculate_b_all_bandwidth(
            &mut self.mgmt.fsl,
            self.base_b_all_set_thd,
            &self.config.gap_config,
        )?;

        let updated = ManagedOnu {
            eqd,
            state: OnuState::AwaitingPortIdAck,
            ..existing
        };
        self.onus.insert(onu_id, updated);
        self.state_entered_at.insert(onu_id, now);
        Ok(updated)
    }

    /// Processes an upstream PLOAM `Acknowledge` (`0x09`) frame
    /// (`fslstack_gpon_ploam_ack_process` ->
    /// `fslstack_gpon_configure_port_id_ack_process`).
    ///
    /// When the acknowledged downstream message is `Configure_Port_ID` (`0x0e`)
    /// with `activate != 0`, matches a pending `(onu_id, sequence)` entry, and
    /// the ONU is in `OnuState::AwaitingPortIdAck`, this programs the OMCC GEM
    /// port in `PORTID_CFG_TAB`, transitions the ONU to `OnuState::Active`, and
    /// transmits `BER_Interval` (`28_800_000`, 3x).
    pub fn process_ploam_ack(
        &mut self,
        record: &PloamRecord,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<Option<ManagedOnu>> {
        self.process_ploam_ack_at(record, Instant::now(), checkpoint)
    }

    pub fn process_ploam_ack_at(
        &mut self,
        record: &PloamRecord,
        now: Instant,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<Option<ManagedOnu>> {
        self.require_initialized()?;
        if record.message_id() != PLOAM_MSG_ACK {
            return Ok(None);
        }
        let Some(onu_id) = record.onu_id() else {
            return Ok(None);
        };
        let acked_msg_id = record.frame[2];
        if acked_msg_id != PLOAM_MSG_CONFIGURE_PORT_ID {
            return Ok(None);
        }
        let enable = record.frame[5] != 0;
        let sequence = u16::from_be_bytes([record.frame[8], record.frame[9]]);
        let Some(existing) = self.onus.get(&onu_id).copied() else {
            return Ok(None);
        };
        if !enable || existing.state != OnuState::AwaitingPortIdAck {
            return Ok(None);
        }
        let Some(ack_pos) = self
            .pending_port_id_acks
            .iter()
            .position(|entry| entry.onu_id == onu_id && entry.sequence == sequence)
        else {
            return Ok(None);
        };

        let ack_entry = self.pending_port_id_acks.remove(ack_pos);
        configure_gem_port(
            &mut self.mgmt.fsl,
            existing.onu_index,
            0,
            ack_entry.gem_port_id,
            true,
        )?;
        let ber_frame = ber_interval_ploam(onu_id, DEFAULT_BER_INTERVAL);
        for _ in 0..self.config.ploam_repeat {
            self.mgmt
                .send_ploam(&ber_frame, self.config.tx_poll, checkpoint)
                .map_err(map_send_err)?;
        }

        let updated = ManagedOnu {
            state: OnuState::Active,
            ..existing
        };
        self.onus.insert(onu_id, updated);
        self.state_entered_at.insert(onu_id, now);
        Ok(Some(updated))
    }

    /// Runs the stock periodic ACK-list expiry
    /// (`fslstack_gpon_ploamd_ack_list_timeout_check`) and independent O3/O4
    /// state timeout sweep (`fslstack_gpon_range_ack_state_timeout_check`),
    /// reregistering timed-out ONUs so they can cleanly re-enter discovery.
    pub fn check_timeouts(
        &mut self,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<TimeoutReport> {
        self.check_timeouts_at(Instant::now(), checkpoint)
    }

    pub fn check_timeouts_at(
        &mut self,
        now: Instant,
        checkpoint: &mut impl FnMut(Duration) -> io::Result<()>,
    ) -> io::Result<TimeoutReport> {
        self.require_initialized()?;
        let mut report = TimeoutReport::default();

        let ack_timeout = self.config.ack_timeout;
        let before = self.pending_port_id_acks.len();
        self.pending_port_id_acks
            .retain(|entry| now.saturating_duration_since(entry.created_at) < ack_timeout);
        report.expired_acks = before - self.pending_port_id_acks.len();

        let state_timeout = self.config.state_timeout;
        let expired_onus: Vec<(u8, OnuState)> = self
            .onus
            .iter()
            .filter_map(|(&onu_id, onu)| {
                if !matches!(onu.state, OnuState::Ranging | OnuState::AwaitingPortIdAck) {
                    return None;
                }
                let entered_at = self.state_entered_at.get(&onu_id).copied()?;
                (now.saturating_duration_since(entered_at) >= state_timeout)
                    .then_some((onu_id, onu.state))
            })
            .collect();

        for (onu_id, state) in expired_onus {
            self.deactivate_onu(onu_id, &[], &[], checkpoint)?;
            match state {
                OnuState::Ranging => report.timed_out_ranging.push(onu_id),
                OnuState::AwaitingPortIdAck => report.timed_out_port_id_ack.push(onu_id),
                OnuState::Active => {}
            }
        }
        Ok(report)
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

        for &tcont in &tconts {
            let disable_cfg = GdbaAllocConfig {
                tcont,
                onu_index: onu.onu_index,
                enable: false,
                fixed_bw: 0,
                assured_bw: 0,
                max_bw: 0,
                alloc_type: 0,
                priority: 0,
                weight: 0,
                ds_period: 0,
                ds_flag: false,
                ds_bw_flag: false,
                ds_tail_flag: false,
            };
            self.configure_tcont_alloc(&disable_cfg)?;
        }

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
        self.state_entered_at.remove(&onu_id);
        self.pending_port_id_acks
            .retain(|entry| entry.onu_id != onu_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
