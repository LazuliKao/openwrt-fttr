//! AN4475-G2 FSL hardware owner, runtime scheduler, and OMCI/PLOAM bridge.
use super::{
    activation::{
        assign_alloc_id_ploam, configure_gem_port, set_gem_wmci_aes, PLOAM_MSG_ACK,
        PLOAM_MSG_ASSIGN_ALLOC_ID, PLOAM_MSG_BER_INTERVAL,
    },
    diagnostics::FslDiagnostics,
    gdba::{alloc_to_tcont, GdbaAllocConfig},
    olt::{FslOltSession, OltSessionConfig, OnuState as FslOnuState, OnuUpstreamRate},
    transport::Device,
    ONU_COUNT,
};
use crate::{
    controller::{
        ipc_server, lifetime,
        optical_owner::{OpticalActor, OpticalBackend, OpticalClient, OpticalCommand},
        service,
    },
    ipc::{self, SubDevice},
    platform::{
        backend::PlatformDiagnostics,
        context::{ChannelGuard, ManagementLease, PlatformContext},
        openwrt::network,
    },
    pon_engine::{
        omci::{session::OmciTransport, wire::BaselineFrame},
        ploam::PloamFrame,
        PonEngine,
    },
};
use fttr_api::OpticalState;
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{self, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

const FSL_DEVICE_PATH: &str = "/dev/fsl-1fbd4000.fsl";
const BOSA_CAL_SOURCE: &str = "/dev/ubi0_3";
const BOSA_CAL_OFFSET: u64 = 0x1a_5000;
const NNI_INTERFACE: &str = "fttr0";
const SERIAL_NUMBER_ONU_MSG_ID: u8 = 1;
const PLOAM_MSG_DYING_GASP: u8 = 3;

fn deactivate_session_onu(session: &mut FslOltSession<Device>, id: u8) -> io::Result<bool> {
    if session.onu(id).is_none() {
        return Ok(false);
    }
    let res = super::RESOURCES.onu(id)?;
    let max_onus = session.config().init.max_onu_id.min(128) as u8;
    let extra_tconts = alloc_to_tcont(res.alloc_id, 0, max_onus, 0)
        .map(|t| vec![t])
        .unwrap_or_default();
    session.deactivate_onu(id, &extra_tconts, &[res.gem_id], &mut |delay| {
        thread::sleep(delay);
        Ok(())
    })?;
    Ok(true)
}

/// Bounded owner-only readback, including the counter banks read by stock
/// `fsldrv_gpon_getponstat`. Do not acknowledge status, clear counters, or read
/// the PLOAM/OMCI FIFO here. Counter values are raw snapshots, not interval sums.
fn log_receive_path_snapshot(session: &mut FslOltSession<Device>) -> io::Result<()> {
    use crate::platform::diagnostics::RegisterSpace;
    for (name, address, words) in [
        ("pon_reset", 0x12, 1),
        ("ram_status", 0x31, 1),
        ("serdes_status", 0x110f, 1),
        ("opt_config", 0x102, 1),
        ("sd_config", 0x208, 1),
        ("gpon_mask", 0x210, 1),
        ("gpon_int", 0x2f0, 1),
        ("ploam_mask", 0x44a, 1),
        ("ploam_int", 0x49e, 1),
        ("ploam_valid", 0x41b, 1),
        ("sn_request", 0x606, 2),
        ("dba_start", 0x501, 1),
        ("dba_work", 0x500, 1),
        ("nni_spi", 0x913, 1),
        ("opt_int", 0x150, 1),
        ("us_gtc_counters", 0x732, 14),
        ("ds_gem_counters", 0x74c, 14),
    ] {
        let values = session
            .fsl_mut()
            .read(RegisterSpace::Register, address, 0, words)?;
        eprintln!("[*] FSL RX snapshot {name}@{address:#x}={values:x?}");
    }
    Ok(())
}

struct FslRuntimeState {
    session: Mutex<Option<FslOltSession<Device>>>,
    omci_rx: Mutex<BTreeMap<u8, VecDeque<BaselineFrame>>>,
}

static RUNTIME: OnceLock<FslRuntimeState> = OnceLock::new();

fn runtime_state() -> &'static FslRuntimeState {
    RUNTIME.get_or_init(|| FslRuntimeState {
        session: Mutex::new(None),
        omci_rx: Mutex::new(BTreeMap::new()),
    })
}

fn with_session<T>(f: impl FnOnce(&mut FslOltSession<Device>) -> io::Result<T>) -> io::Result<T> {
    let mut guard = runtime_state()
        .session
        .lock()
        .map_err(|_| io::Error::other("FSL runtime session lock poisoned"))?;
    let session = guard.as_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotConnected,
            "FSL runtime session is not initialized",
        )
    })?;
    f(session)
}

pub(crate) struct FslOmciTransport {
    onu_id: u8,
}

impl FslOmciTransport {
    pub(crate) fn new(onu_id: u8) -> Self {
        Self { onu_id }
    }
}

impl OmciTransport for FslOmciTransport {
    fn send(&mut self, frame: &BaselineFrame) -> io::Result<()> {
        let bytes = frame.to_bytes();
        with_session(|session| session.send_omci(self.onu_id, &bytes, &mut |_| Ok(())))
    }

    fn receive(&mut self) -> io::Result<Option<BaselineFrame>> {
        let mut queues = runtime_state()
            .omci_rx
            .lock()
            .map_err(|_| io::Error::other("FSL OMCI RX queue lock poisoned"))?;
        Ok(queues.get_mut(&self.onu_id).and_then(VecDeque::pop_front))
    }
}

pub(crate) struct FslManagementLease {
    onu_id: u8,
    disarmed: bool,
    business_modified: bool,
}

impl FslManagementLease {
    pub(crate) fn open(onu_id: u8) -> io::Result<Self> {
        if usize::from(onu_id) >= ONU_COUNT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ONU-ID out of AN4475-G2 range",
            ));
        }
        Ok(Self {
            onu_id,
            disarmed: false,
            business_modified: false,
        })
    }
}

fn configure_business_datapath(onu_id: u8, floor_bytes: Option<u16>) -> io::Result<()> {
    let res = super::RESOURCES.onu(onu_id)?;
    with_session(|session| {
        let onu = session
            .onu(onu_id)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ONU not active in FSL"))?;
        configure_gem_port(session.fsl_mut(), onu.onu_index, 0, res.gem_id, true)?;
        let max_onus = session.config().init.max_onu_id.min(128) as u8;
        let tcont = alloc_to_tcont(res.alloc_id, 0, max_onus, 0)?;
        let alloc = GdbaAllocConfig::business_data(
            tcont,
            onu.onu_index,
            session.dba_granularity_8x(),
            onu.rate == OnuUpstreamRate::Rate2p5G,
            floor_bytes.map(u32::from),
        )?;
        session.configure_tcont_alloc(&alloc)?;
        let frame = assign_alloc_id_ploam(onu_id, res.alloc_id, true, 0)?;
        for _ in 0..session.config().ploam_repeat {
            session.send_ploam(&frame, &mut |delay| {
                thread::sleep(delay);
                Ok(())
            })?;
        }
        println!(
            "[+] FSL business datapath enabled for ONU {onu_id}: gem={:#x} alloc={:#x} tcont={tcont} fixed_bw={} max_bw={} alloc_type={}",
            res.gem_id, res.alloc_id, alloc.fixed_bw, alloc.max_bw, alloc.alloc_type
        );
        Ok(())
    })
}

impl ManagementLease for FslManagementLease {
    fn set_business_grant_floor(&mut self, bytes: u16) -> io::Result<()> {
        // Mark before touching hardware so partial configuration is cleaned up.
        // A read-only management lease must not revoke persistent business.
        self.business_modified = true;
        if bytes == 0 {
            revoke_business(self.onu_id)
        } else {
            configure_business_datapath(self.onu_id, Some(bytes))
        }
    }

    fn set_management_grant_floor(&mut self, _bytes: u16) -> io::Result<()> {
        Ok(())
    }

    fn disarm(&mut self) {
        if !self.business_modified {
            if let Err(err) = configure_business_datapath(self.onu_id, None) {
                eprintln!(
                    "[-] FSL business datapath activation failed for ONU {}: {err}",
                    self.onu_id
                );
            } else {
                self.business_modified = true;
            }
        }
        self.disarmed = true;
    }
}

impl Drop for FslManagementLease {
    fn drop(&mut self) {
        if self.business_modified && !self.disarmed {
            let _ = revoke_business(self.onu_id);
        }
    }
}

pub(crate) struct FslChannelGuard;
impl ChannelGuard for FslChannelGuard {}

pub(crate) fn send_ploam(frame: &PloamFrame) -> io::Result<()> {
    let bytes = super::olt::to_downstream_ploam(frame);
    with_session(|session| {
        session.send_ploam(&bytes, &mut |delay| {
            thread::sleep(delay);
            Ok(())
        })
    })
}

/// Revokes only the ONU's business data GEM port and data T-CONT without
/// deactivating the ONU's O5 registration or OMCC management channel.
pub(crate) fn revoke_business(id: u8) -> io::Result<()> {
    let res = super::RESOURCES.onu(id)?;
    with_session(|session| {
        let Some(onu) = session.onu(id).copied() else {
            return Ok(());
        };
        configure_gem_port(session.fsl_mut(), onu.onu_index, 0, res.gem_id, false)?;
        let max_onus = session.config().init.max_onu_id.min(128) as u8;
        if let Ok(tcont) = alloc_to_tcont(res.alloc_id, 0, max_onus, 0) {
            let disable_alloc = GdbaAllocConfig {
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
            let _ = session.configure_tcont_alloc(&disable_alloc);
        }
        Ok(())
    })
}

struct RuntimeOptics<'a> {
    ready: bool,
    services: &'a mut service::Services,
    engine: &'a Mutex<PonEngine>,
}

impl OpticalBackend for RuntimeOptics<'_> {
    fn validate_calibration(&self) -> io::Result<()> {
        let mut file = fs::File::open(BOSA_CAL_SOURCE)?;
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(BOSA_CAL_OFFSET))?;
        let mut raw = [0u8; super::calibration::TABLE_BYTES];
        file.read_exact(&mut raw)?;
        let table = super::calibration::CalibrationTable::parse(&raw)?;
        table.require_chip(super::profile::UX3326S_CHIP_ID)
    }

    fn save_settings(&mut self, settings: fttr_api::OpticalSettings) -> io::Result<()> {
        crate::optical::save(settings)
    }

    fn mac_tx_allowed(&self) -> bool {
        true
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn cleanup_pending(&self) -> bool {
        false
    }

    fn quiesce(&mut self) -> io::Result<()> {
        let active: Vec<u8> =
            with_session(|session| Ok(session.active_onus().map(|onu| onu.onu_id).collect()))
                .unwrap_or_default();
        for id in active {
            self.services.stop(id);
            if let Ok(mut engine) = self.engine.lock() {
                engine.mark_offline(id);
            }
            let _ = with_session(|session| deactivate_session_onu(session, id).map(|_| ()));
            if let Ok(mut queues) = runtime_state().omci_rx.lock() {
                queues.remove(&id);
            }
        }
        super::vport::bring_all_ports_down();
        Ok(())
    }

    fn set_tx(&mut self, enabled: bool) -> io::Result<()> {
        with_session(|session| session.fsl_mut().set_tx_enabled(enabled))
    }

    fn read_tx(&mut self) -> io::Result<bool> {
        with_session(|session| session.fsl_mut().is_tx_enabled())
    }

    fn desired(&self) -> io::Result<bool> {
        crate::optical::load_enabled()
    }

    fn save(&mut self, enabled: bool) -> io::Result<()> {
        crate::optical::save_enabled(enabled)
    }
}

fn initialize_hardware(stopping: &AtomicBool) -> io::Result<OpticalState> {
    let config = OltSessionConfig {
        nni_eth_tpid: super::initialization::defaults::IEEE_8021AD_TPID,
        ..OltSessionConfig::default()
    };
    super::vport::prepare_nni_and_ports(NNI_INTERFACE, config.nni_gem_port_offset, stopping)?;
    let dev = Device::open(Path::new(FSL_DEVICE_PATH))?;
    let mut session = FslOltSession::new(dev, config);
    session.initialize(&mut |delay| {
        if stopping.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopping"));
        }
        thread::sleep(delay);
        Ok(())
    })?;
    // Stock `apply_gemport_defaults` pre-maps GEM 256..271 to ONU 0 and enables
    // WMCI on GEM 270. Clear our managed business GEM slots so each ONU's GEM
    // is bound strictly to its own `onu_index` only after provisioning succeeds.
    for entry in super::RESOURCES.allocations {
        configure_gem_port(session.fsl_mut(), 0, 0, entry.gem_id, false)?;
        set_gem_wmci_aes(session.fsl_mut(), entry.gem_id, false, false)?;
    }
    FslDiagnostics.calibrate_bosa(Path::new(BOSA_CAL_SOURCE), BOSA_CAL_OFFSET)?;
    let desired = crate::optical::load_enabled().unwrap_or(false);
    session.fsl_mut().set_tx_enabled(desired)?;
    let enabled = session.fsl_mut().is_tx_enabled()?;
    *runtime_state()
        .session
        .lock()
        .map_err(|_| io::Error::other("FSL runtime session lock poisoned"))? = Some(session);
    Ok(if enabled {
        OpticalState::Enabled
    } else {
        OpticalState::Disabled
    })
}

pub(crate) fn status_snapshot(
    optics: &OpticalClient,
    pon_engine: &Mutex<PonEngine>,
    devices: &Mutex<Vec<SubDevice>>,
    services: &service::States,
    provisioning: fttr_api::SyncStatusReply,
) -> Result<fttr_api::StatusReply, i32> {
    let ready = optics.hardware_ready.load(Ordering::Acquire);
    let tx_enabled = with_session(|session| session.fsl_mut().is_tx_enabled()).ok();
    let devices = devices.lock().map_err(|_| 9)?;
    let engine = pon_engine.lock().map_err(|_| 9)?;
    Ok(crate::platform::status::snapshot(
        &engine,
        services,
        (&devices, provisioning),
        crate::platform::status::Hardware {
            switch_port_count: super::ONU_COUNT as u8,
            supports_pinning: false,
            ranging_source: |_| "FSL upstream ranging time / calculated EqD".into(),
            has_pon_mac_fec: false,
            data_path,
            fpga_status: if ready { "ready" } else { "not_ready" }.into(),
            pon_mac: None,
            optical_transceiver: fttr_api::OpticalTransceiverStatus {
                calibration_sha256: None,
                calibration_applied: ready,
                model: "UX3326S1".into(),
                wavelength_tx_nm: 1490,
                wavelength_rx_nm: 1310,
                phy_rate_downlink_gbps: 2.5,
                phy_rate_uplink_gbps: 1.25,
                tx_power_dbm: None,
                laser_bias_current_ma: None,
                temperature_celsius: None,
                vcc_voltage: None,
                cdr_locked: None,
                tx_enabled,
                measurements: Default::default(),
            },
            npu: None,
        },
    ))
}

struct PortCounters {
    at: Instant,
    tx: Option<u64>,
    rx: Option<u64>,
}

fn read_sysfs_counter(path: &Path) -> (Option<u64>, fttr_api::MeasurementEvidence) {
    let source = path.display().to_string();
    let unit = if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("packets"))
    {
        "packets"
    } else {
        "bytes"
    };
    match fs::read_to_string(path) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(value) => (
                Some(value),
                fttr_api::MeasurementEvidence::observed(source, unit, raw.trim()),
            ),
            Err(error) => (
                None,
                fttr_api::MeasurementEvidence::unavailable(
                    source,
                    unit,
                    format!("invalid_counter: {error}"),
                ),
            ),
        },
        Err(error) => (
            None,
            fttr_api::MeasurementEvidence::unavailable(
                source,
                unit,
                format!("read_failed: {error}"),
            ),
        ),
    }
}

fn counter_rate_kbps(
    current: Option<u64>,
    previous: Option<u64>,
    elapsed: Duration,
) -> Option<f64> {
    if elapsed < Duration::from_secs(1) {
        return None;
    }
    let delta = current?.checked_sub(previous?)?;
    Some((delta as f64) * 8.0 / elapsed.as_secs_f64() / 1000.0)
}

fn data_path(
    id: u8,
    registration: &str,
    inventory: Option<fttr_api::OnuOmciInventory>,
) -> fttr_api::DataPathStatus {
    static SAMPLES: OnceLock<Mutex<BTreeMap<String, PortCounters>>> = OnceLock::new();
    let interface = super::RESOURCES
        .interface(id)
        .unwrap_or("unmapped")
        .to_owned();
    let base = Path::new("/sys/class/net")
        .join(&interface)
        .join("statistics");
    let mut measurements = fttr_api::Measurements::new();
    measurements.insert(
        "interface_scope".into(),
        fttr_api::MeasurementEvidence::observed("FSL 802.1ad VPORT", "scope", "per_onu_vport"),
    );
    let (tx_bytes, tx_ev) = read_sysfs_counter(&base.join("tx_bytes"));
    let (rx_bytes, rx_ev) = read_sysfs_counter(&base.join("rx_bytes"));
    let (_tx_packets, tx_pkts_ev) = read_sysfs_counter(&base.join("tx_packets"));
    let (_rx_packets, rx_pkts_ev) = read_sysfs_counter(&base.join("rx_packets"));
    measurements.insert("tx_bytes".into(), tx_ev);
    measurements.insert("rx_bytes".into(), rx_ev);
    measurements.insert("tx_packets".into(), tx_pkts_ev);
    measurements.insert("rx_packets".into(), rx_pkts_ev);

    let mut current_tx_kbps = None;
    let mut current_rx_kbps = None;
    if let Ok(mut samples) = SAMPLES.get_or_init(|| Mutex::new(BTreeMap::new())).lock() {
        let key = format!("{interface}:{registration}");
        let previous = samples.get(&key);
        if let Some(previous) = previous {
            current_tx_kbps = counter_rate_kbps(tx_bytes, previous.tx, previous.at.elapsed());
            current_rx_kbps = counter_rate_kbps(rx_bytes, previous.rx, previous.at.elapsed());
        }
        if previous.is_none_or(|sample| sample.at.elapsed() >= Duration::from_secs(1)) {
            samples.insert(
                key,
                PortCounters {
                    at: Instant::now(),
                    tx: tx_bytes,
                    rx: rx_bytes,
                },
            );
        }
    }
    for (name, value) in [
        ("current_tx_kbps", current_tx_kbps),
        ("current_rx_kbps", current_rx_kbps),
    ] {
        let evidence = match value {
            Some(value) => fttr_api::MeasurementEvidence::observed(
                format!("{}/counter_delta", base.display()),
                "kbit/s",
                value.to_string(),
            ),
            None => fttr_api::MeasurementEvidence::unavailable(
                format!("{}/counter_delta", base.display()),
                "kbit/s",
                "requires_two_valid_monotonic_samples_at_least_one_second_apart",
            ),
        };
        measurements.insert(name.into(), evidence);
    }

    measurements.insert(
        "nni_expected_tpid".into(),
        fttr_api::MeasurementEvidence::observed(
            "FSL runtime 802.1ad VPORT constant; not readback",
            "hex",
            format!(
                "0x{:04x}",
                super::initialization::defaults::IEEE_8021AD_TPID
            ),
        ),
    );
    measurements.insert(
        "nni_expected_gem_offset".into(),
        fttr_api::MeasurementEvidence::observed(
            "FSL initialization constant; not readback",
            "gem_port_id",
            super::initialization::defaults::DEFAULT_NNI_GEMPORT_OFFSET.to_string(),
        ),
    );
    // The session is also used by OMCI and the activation worker. Never wait
    // for it while status holds the engine/device locks: an unavailable sample
    // is preferable to blocking or inverting the hardware lock order.
    use super::initialization::defaults::{
        NNI_ETH_TPID_CFG, NNI_GEMPORT_OFFSET_CFG, VLAN_TYPE_CFG,
    };
    let register_values = runtime_state()
        .session
        .try_lock()
        .ok()
        .and_then(|mut guard| {
            guard.as_mut().map(|session| {
                [NNI_ETH_TPID_CFG, VLAN_TYPE_CFG, NNI_GEMPORT_OFFSET_CFG]
                    .map(|address| session.fsl_mut().read_register(address).ok())
            })
        });
    for (index, name, address, mask, unit) in [
        (0, "nni_tpid_readback", NNI_ETH_TPID_CFG, 0xffff, "hex"),
        (1, "nni_vlan_type_readback", VLAN_TYPE_CFG, 0xffff, "hex"),
        (
            2,
            "nni_gem_offset_readback",
            NNI_GEMPORT_OFFSET_CFG,
            0x0fff,
            "gem_port_id",
        ),
    ] {
        let source = format!("shared FSL register {address:#x}; readback");
        let evidence = match register_values.and_then(|values| values[index]) {
            Some(value) if unit == "hex" => fttr_api::MeasurementEvidence::observed(
                source,
                unit,
                format!("0x{:04x}", value & mask),
            ),
            Some(value) => {
                fttr_api::MeasurementEvidence::observed(source, unit, (value & mask).to_string())
            }
            None => fttr_api::MeasurementEvidence::unavailable(
                source,
                unit,
                "session_busy_or_register_unavailable",
            ),
        };
        measurements.insert(name.into(), evidence);
    }

    let olt_channel_gem = super::RESOURCES.onu(id).ok().map(|r| r.gem_id);
    let gem_ports: Option<Vec<u16>> = inventory
        .as_ref()
        .and_then(|v| v.gem_ports.iter().map(|p| p.port_id).collect());
    let forwarding_verified = if let Some(ref inv) = inventory {
        let gem_matched = olt_channel_gem.is_some_and(|expected| {
            gem_ports
                .as_ref()
                .is_some_and(|ports| ports.contains(&expected))
        });
        let rx_traffic = rx_bytes.unwrap_or(0) > 0;
        let (verified, fwd_ev) = if !gem_matched {
            (
                false,
                fttr_api::MeasurementEvidence::unavailable(
                    format!("FSL VPORT {interface}"),
                    "forwarding_proof",
                    "gem_port_mismatch_with_olt_vport",
                ),
            )
        } else if rx_traffic {
            (
                true,
                fttr_api::MeasurementEvidence::observed(
                    format!("{interface}/statistics"),
                    "forwarding_proof",
                    format!(
                        "vport_gem_rx_observed: rx_bytes={}, tx_bytes={}, gem_port=0x{:04x}",
                        rx_bytes.unwrap_or(0),
                        tx_bytes.unwrap_or(0),
                        olt_channel_gem.unwrap_or(0)
                    ),
                ),
            )
        } else {
            (
                false,
                fttr_api::MeasurementEvidence::unavailable(
                    format!("{interface}/statistics"),
                    "forwarding_proof",
                    "waiting_for_first_rx_frame_from_vport",
                ),
            )
        };
        for field in ["gem_ports", "onu_inventory"] {
            let mut ev = fttr_api::MeasurementEvidence::observed(
                "fresh ONU OMCI MIB upload; not OLT forwarding proof",
                "ONU configuration",
                format!(
                    "{} GEM CTPs; {} VLAN filters; {} VLAN operation tables",
                    inv.gem_ports.len(),
                    inv.vlan_filters.len(),
                    inv.vlan_operations.len()
                ),
            );
            ev.age_seconds = Some(inv.age_seconds);
            measurements.insert(field.into(), ev);
        }
        measurements.insert("forwarding_proof".into(), fwd_ev);
        Some(verified)
    } else {
        measurements.insert(
            "forwarding_proof".into(),
            fttr_api::MeasurementEvidence::unavailable(
                "verified data-path inventory",
                "forwarding_proof",
                "verified_inventory_unavailable",
            ),
        );
        Some(false)
    };
    fttr_api::DataPathStatus {
        interface,
        gem_ports,
        vlan_id: None,
        onu_inventory: inventory,
        tx_bytes,
        rx_bytes,
        current_tx_kbps,
        current_rx_kbps,
        olt_channel_gem,
        forwarding_verified,
        offload: Default::default(),
        measurements,
    }
}

fn ipc_handler(
    devices: Arc<Mutex<Vec<SubDevice>>>,
    pon_engine: Arc<Mutex<PonEngine>>,
    services: service::States,
    optics: OpticalClient,
) -> impl Fn(UnixStream, &str) + Send + Sync + 'static {
    move |mut stream, line| {
        if line == "BRIDGE SETUP" {
            let response = match network::setup_bridge(&super::RESOURCES, "br-lan") {
                Ok(()) => serde_json::json!({"status": "success"}),
                Err(error) => serde_json::json!({"status": "error", "error": error.to_string()}),
            };
            let _ = serde_json::to_writer(&mut stream, &response);
            let _ = stream.write_all(b"\n");
        } else if line == "STATUS" {
            let reply = serde_json::from_value(crate::provisioning::snapshot())
                .map_err(|_| 9)
                .and_then(|provisioning| {
                    status_snapshot(&optics, &pon_engine, &devices, &services, provisioning)
                });
            match reply {
                Ok(status) => {
                    let _ = serde_json::to_writer(&mut stream, &status);
                }
                Err(_) => {
                    let _ = serde_json::to_writer(
                        &mut stream,
                        &serde_json::json!({"status":"error","error":"status unavailable"}),
                    );
                }
            }
            let _ = stream.write_all(b"\n");
        } else if line == "DEVICES" {
            let list = devices.lock().map(|d| d.clone()).unwrap_or_default();
            let _ = serde_json::to_writer(
                &mut stream,
                &serde_json::json!({"status": "success", "devices": list}),
            );
            let _ = stream.write_all(b"\n");
        } else if let Some(cmd) = line.strip_prefix("OPTICAL ") {
            let req = match cmd.trim() {
                "ON" => Ok(OpticalCommand::On),
                "OFF" => Ok(OpticalCommand::Off),
                "STATUS" => Ok(OpticalCommand::Status),
                "RELOAD" => Ok(OpticalCommand::Reload),
                "REINITIALIZE" => Ok(OpticalCommand::Reinitialize),
                "SHUTDOWN" => Ok(OpticalCommand::Shutdown),
                other => Err(format!("unknown optical command: {other}")),
            };
            match req {
                Ok(cmd) => match optics.request(cmd) {
                    Ok(reply) => {
                        let _ = serde_json::to_writer(&mut stream, &reply);
                    }
                    Err(e) => {
                        let _ = serde_json::to_writer(
                            &mut stream,
                            &serde_json::json!({"status": "error", "error": e.to_string()}),
                        );
                    }
                },
                Err(msg) => {
                    let _ = serde_json::to_writer(
                        &mut stream,
                        &serde_json::json!({"status": "error", "error": msg}),
                    );
                }
            }
            let _ = stream.write_all(b"\n");
        } else {
            let _ = &services;
            let _ = serde_json::to_writer(
                &mut stream,
                &serde_json::json!({"status": "error", "error": "unsupported IPC command"}),
            );
            let _ = stream.write_all(b"\n");
        }
    }
}

pub fn run(platform: PlatformContext) {
    crate::runtime::block_on(run_async(platform));
}

async fn run_async(platform: PlatformContext) {
    println!("[*] Starting FiberHome AN4475-G2 FTTR Master Controller Daemon (Rust fttrd)...");
    let _owner = crate::optical::claim_owner().unwrap_or_else(|error| {
        eprintln!("[-] Optical ownership failed: {error}");
        std::process::exit(1);
    });
    let lifetime = lifetime::Lifetime::start().await.unwrap_or_else(|error| {
        eprintln!("[-] MQTT startup failed: {error}");
        std::process::exit(1);
    });
    #[cfg(feature = "acs")]
    crate::acs::start_from_env();

    let _ = fs::create_dir_all(ipc::SUBDEV_DIR);
    let devices: Arc<Mutex<Vec<SubDevice>>> = Arc::new(Mutex::new(Vec::new()));
    let pon_engine = Arc::new(Mutex::new(PonEngine::new(platform.resources())));
    service::init_pon_engine(pon_engine.clone());
    crate::provisioning::start(pon_engine.clone());

    let hardware_ready = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let (optical_requests, optical_receiver) = mpsc::sync_channel(2);
    let shutdown_client = OpticalClient {
        platform,
        requests: optical_requests.clone(),
        hardware_ready: hardware_ready.clone(),
        stopping: stopping.clone(),
    };
    let service_states: service::States = Arc::new(Mutex::new(BTreeMap::new()));

    if let Ok(listener) = ipc::create_socket_listener() {
        ipc_server::start(
            listener,
            ipc_handler(
                devices.clone(),
                pon_engine.clone(),
                service_states.clone(),
                OpticalClient {
                    platform,
                    requests: optical_requests.clone(),
                    hardware_ready: hardware_ready.clone(),
                    stopping: stopping.clone(),
                },
            ),
        );
        println!("[+] IPC socket server listening on {}", ipc::SOCKET_PATH);
    }

    let _ubus_thread = super::ubus::start(
        OpticalClient {
            platform,
            requests: optical_requests.clone(),
            hardware_ready: hardware_ready.clone(),
            stopping: stopping.clone(),
        },
        devices.clone(),
        pon_engine.clone(),
        service_states.clone(),
        stopping.clone(),
    );

    let pon_engine_loop = pon_engine.clone();
    let states_loop = service_states.clone();
    let stopping_loop = stopping.clone();
    let ready_loop = hardware_ready.clone();
    let fsl_thread = thread::Builder::new()
        .name("fsl-loop".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let initial = initialize_hardware(&stopping_loop);
            let (ready, state) = match initial {
                Ok(state) => (true, state),
                Err(error) => {
                    eprintln!("[-] FSL hardware initialization failed: {error}");
                    (false, OpticalState::Unknown)
                }
            };
            ready_loop.store(ready, Ordering::Release);
            let mut services = service::Services::new(platform, states_loop);
            let mut optics = OpticalActor { state };
            let mut observation_start = Instant::now();
            let mut last_timeout_check = Instant::now();
            let mut observation_first = true;
            let mut observation_counts = (0usize, 0usize, 0usize, 0usize);
            let mut receive_snapshots_remaining = 3u8;

            while !stopping_loop.load(Ordering::Acquire) {
                {
                    let mut backend = RuntimeOptics {
                        ready: ready_loop.load(Ordering::Acquire),
                        services: &mut services,
                        engine: &pon_engine_loop,
                    };
                    for request in optical_receiver.try_iter().take(2) {
                        let response = optics.dispatch(&request, &mut backend, &stopping_loop);
                        let _ = request.reply.send(response);
                    }
                }
                if stopping_loop.load(Ordering::Acquire) {
                    break;
                }
                if !ready_loop.load(Ordering::Acquire) || optics.state != OpticalState::Enabled {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }

                let run_timeouts = last_timeout_check.elapsed() >= Duration::from_secs(1);
                let poll_result = with_session(|session| {
                    let report = session.poll_once(16, 16, &mut |delay| {
                        thread::sleep(delay);
                        Ok(())
                    })?;
                    observation_counts.0 += report.interrupts.len();
                    observation_counts.1 += report.ploams.len();
                    observation_counts.2 += report.flagged_ploams;
                    observation_counts.3 += report.omci_frames.len();
                    if observation_start.elapsed() >= Duration::from_secs(30) {
                        if receive_snapshots_remaining > 0 {
                            receive_snapshots_remaining -= 1;
                            if let Err(error) = log_receive_path_snapshot(session) {
                                eprintln!("[-] FSL RX snapshot failed: {error}");
                            }
                        }
                        if observation_first || observation_counts != (0, 0, 0, 0) {
                            eprintln!(
                                "[*] FSL poll 30s: interrupts={} ploam={} flagged={} omci={}",
                                observation_counts.0,
                                observation_counts.1,
                                observation_counts.2,
                                observation_counts.3
                            );
                        }
                        observation_first = false;
                        observation_start = Instant::now();
                        observation_counts = (0, 0, 0, 0);
                    }

                    for frame in &report.omci_frames {
                        if let Ok(parsed) = BaselineFrame::from_bytes(&frame.payload) {
                            if let Ok(mut queues) = runtime_state().omci_rx.lock() {
                                queues.entry(frame.source).or_default().push_back(parsed);
                            }
                        }
                    }

                    let mut deactivated = Vec::new();
                    match session.evaluate_alarm_interrupts(&report.interrupts) {
                        Ok(offline) => {
                            for (onu_id, reason) in offline {
                                eprintln!(
                                    "[*] FSL offline alarm ({reason}) for ONU {onu_id} -> deactivating"
                                );
                                match deactivate_session_onu(session, onu_id) {
                                    Ok(true) => deactivated.push(onu_id),
                                    Ok(false) => {}
                                    Err(err) => eprintln!(
                                        "[-] FSL deactivate failed for ONU {onu_id} ({reason}): {err}"
                                    ),
                                }
                            }
                        }
                        Err(err) => eprintln!("[-] FSL alarm evaluation failed: {err}"),
                    }

                    let mut newly_active = Vec::new();
                    for record in &report.ploams {
                        if record.message_id() == SERIAL_NUMBER_ONU_MSG_ID {
                            let Some(rate) = OnuUpstreamRate::from_metadata(record.rate_metadata())
                            else {
                                eprintln!(
                                    "[-] Ignoring SN with unsupported upstream rate metadata {}",
                                    record.rate_metadata()
                                );
                                continue;
                            };
                            match record.onu_id() {
                                None => {
                                    if let Some(parsed) = super::olt::parse_upstream_ploam(record) {
                                        let mut sn = [0u8; 8];
                                        sn.copy_from_slice(&parsed.payload[..8]);
                                        if let Some(existing) = session.find_onu_by_sn(&sn) {
                                            if existing.state == FslOnuState::Active {
                                                eprintln!(
                                                    "[*] O3 SN rediscovery for active ONU {} -> deactivating stale registration",
                                                    existing.onu_id
                                                );
                                                match deactivate_session_onu(session, existing.onu_id)
                                                {
                                                    Ok(true) => deactivated.push(existing.onu_id),
                                                    Ok(false) => {}
                                                    Err(err) => eprintln!(
                                                        "[-] Stale ONU {} deactivation failed: {err}",
                                                        existing.onu_id
                                                    ),
                                                }
                                            }
                                        }
                                        match session.process_o3_sn_discovery(
                                            sn,
                                            rate,
                                            &mut |delay| {
                                                thread::sleep(delay);
                                                Ok(())
                                            },
                                        ) {
                                            Ok(onu) => eprintln!(
                                                "[+] O3 SN accepted -> ONU {} ranging (rate={rate:?})",
                                                onu.onu_id
                                            ),
                                            Err(err) => {
                                                eprintln!("[-] O3 SN discovery failed: {err}")
                                            }
                                        }
                                    }
                                }
                                Some(onu_id)
                                    if session
                                        .onu(onu_id)
                                        .is_some_and(|onu| onu.state == FslOnuState::Ranging) =>
                                {
                                    let mut serial_number = [0u8; 8];
                                    serial_number.copy_from_slice(&record.frame[2..10]);
                                    match session.process_o4_o5_activation(
                                        onu_id,
                                        &serial_number,
                                        record.ranging_time_raw(),
                                        &mut |delay| {
                                            thread::sleep(delay);
                                            Ok(())
                                        },
                                    ) {
                                        Ok(pending) => eprintln!(
                                            "[+] O4 ranging + Configure_Port_ID sent for ONU {} (eqd={}, state={:?})",
                                            pending.onu_id, pending.eqd, pending.state
                                        ),
                                        Err(err) => eprintln!(
                                            "[-] O4 activation failed for ONU {onu_id}: {err}"
                                        ),
                                    }
                                }
                                Some(_) => {}
                            }
                        } else if record.message_id() == PLOAM_MSG_DYING_GASP {
                            if let Some(onu_id) = record.onu_id() {
                                if session
                                    .onu(onu_id)
                                    .is_some_and(|onu| onu.state == FslOnuState::Active)
                                {
                                    eprintln!(
                                        "[*] ONU {onu_id} sent Dying_Gasp PLOAM -> deactivating"
                                    );
                                    match deactivate_session_onu(session, onu_id) {
                                        Ok(true) => deactivated.push(onu_id),
                                        Ok(false) => {}
                                        Err(err) => eprintln!(
                                            "[-] Dying_Gasp deactivate failed for ONU {onu_id}: {err}"
                                        ),
                                    }
                                }
                            }
                        } else if record.message_id() == PLOAM_MSG_ACK {
                            match session.process_ploam_ack(record, &mut |delay| {
                                thread::sleep(delay);
                                Ok(())
                            }) {
                                Ok(Some(active)) => {
                                    eprintln!(
                                        "[+] O5 Configure_Port_ID ACK confirmed for ONU {} (rate={:?}, eqd={})",
                                        active.onu_id, active.rate, active.eqd
                                    );
                                    newly_active.push(active);
                                }
                                Ok(None)
                                    if record.frame[2] != PLOAM_MSG_BER_INTERVAL
                                        && record.frame[2] != PLOAM_MSG_ASSIGN_ALLOC_ID =>
                                {
                                    eprintln!(
                                        "[*] Ignoring unmatched PLOAM ACK onu_id={:?} acked_msg={:#x} enable={} seq={}",
                                        record.onu_id(),
                                        record.frame[2],
                                        record.frame[5],
                                        u16::from_be_bytes([record.frame[8], record.frame[9]])
                                    );
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    eprintln!("[-] PLOAM ACK processing failed: {err}");
                                }
                            }
                        } else {
                            eprintln!(
                                "[*] FSL upstream PLOAM msg_id={:#x} onu_id={:?}",
                                record.message_id(),
                                record.onu_id()
                            );
                        }
                    }

                    if run_timeouts {
                        last_timeout_check = Instant::now();
                        match session.check_timeouts(&mut |delay| {
                            thread::sleep(delay);
                            Ok(())
                        }) {
                            Ok(timeouts)
                                if timeouts.expired_acks > 0
                                    || !timeouts.timed_out_ranging.is_empty()
                                    || !timeouts.timed_out_port_id_ack.is_empty() =>
                            {
                                eprintln!(
                                    "[*] FSL timeout sweep: expired_acks={} ranging_timeout={:?} port_id_ack_timeout={:?}",
                                    timeouts.expired_acks,
                                    timeouts.timed_out_ranging,
                                    timeouts.timed_out_port_id_ack
                                );
                            }
                            Ok(_) => {}
                            Err(err) => eprintln!("[-] FSL timeout sweep failed: {err}"),
                        }
                    }
                    Ok((deactivated, newly_active))
                });

                match poll_result {
                    Ok((deactivated, newly_active)) => {
                        for id in deactivated {
                            services.stop(id);
                            if let Ok(mut engine) = pon_engine_loop.lock() {
                                engine.mark_offline(id);
                            }
                            if let Ok(mut queues) = runtime_state().omci_rx.lock() {
                                queues.remove(&id);
                            }
                            let _ = super::vport::set_port_up(id, false);
                        }
                        for onu in newly_active {
                            let mut vendor = [0u8; 4];
                            vendor.copy_from_slice(&onu.serial_number[..4]);
                            let mut sn_tail = [0u8; 4];
                            sn_tail.copy_from_slice(&onu.serial_number[4..8]);
                            let displaced = match pon_engine_loop.lock() {
                                Ok(mut engine) => match engine.confirm_platform_onu(
                                    onu.onu_id, vendor, sn_tail, onu.eqd,
                                ) {
                                    Ok(displaced) => displaced,
                                    Err(err) => {
                                        eprintln!("[-] FSL ONU confirmation failed: {err}");
                                        continue;
                                    }
                                },
                                Err(_) => {
                                    eprintln!("[-] PON engine lock poisoned");
                                    continue;
                                }
                            };
                            for id in displaced {
                                services.stop(id);
                            }
                            let vendor_str = String::from_utf8_lossy(&vendor).into_owned();
                            services.start(onu.onu_id, vendor_str);
                        }
                    }
                    Err(err) => {
                        eprintln!("[-] FSL poll error: {err}");
                        thread::sleep(Duration::from_millis(50));
                    }
                }
                services.refresh_auth();
                thread::sleep(Duration::from_millis(5));
            }

            // Drain any final Shutdown/Status request queued right as `stopping` flipped.
            {
                let mut backend = RuntimeOptics {
                    ready: ready_loop.load(Ordering::Acquire),
                    services: &mut services,
                    engine: &pon_engine_loop,
                };
                let mut handled_shutdown = false;
                for request in optical_receiver.try_iter() {
                    handled_shutdown |= matches!(request.command, OpticalCommand::Shutdown);
                    let response = optics.dispatch(&request, &mut backend, &stopping_loop);
                    let _ = request.reply.send(response);
                }
                if !handled_shutdown {
                    if let Ok(request) = optical_receiver.recv_timeout(Duration::from_millis(50)) {
                        let response = optics.dispatch(&request, &mut backend, &stopping_loop);
                        let _ = request.reply.send(response);
                    }
                }
            }
            super::vport::bring_all_ports_down();
            if let Ok(mut guard) = runtime_state().session.lock() {
                *guard = None;
            }
        })
        .ok();

    tokio::select! {
        result = lifetime.wait(devices) => {
            eprintln!("[-] Controller supervision stopped: {result:?}");
            stopping.store(true, Ordering::Release);
            if let Some(handle) = fsl_thread {
                let _ = handle.join();
            }
            std::process::exit(1);
        }
        result = crate::runtime::termination() => {
            if let Err(error) = result {
                eprintln!("[-] Signal handler failed: {error}");
            }
            #[cfg(feature = "acs")]
            crate::acs::shutdown();
            crate::runtime::request_stop();
            let optical = crate::runtime::spawn_blocking(move || {
                shutdown_client.request(OpticalCommand::Shutdown)
            });
            let (optical, ()) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(4), optical),
                crate::runtime::shutdown(),
            );
            match optical {
                Ok(Ok(Ok(_))) => (),
                result => eprintln!("[-] Optical shutdown not confirmed: {result:?}"),
            }
            stopping.store(true, Ordering::Release);
            if let Some(handle) = fsl_thread {
                let _ = handle.join();
            }
        }
    }
}
