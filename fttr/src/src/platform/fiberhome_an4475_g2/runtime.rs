//! AN4475-G2 FSL hardware owner, runtime scheduler, and OMCI/PLOAM bridge.
use super::{
    activation::{
        assign_alloc_id_ploam, configure_gem_port, PLOAM_MSG_ACK, PLOAM_MSG_BER_INTERVAL,
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
        })
    }
}

impl ManagementLease for FslManagementLease {
    fn set_business_grant_floor(&mut self, bytes: u16) -> io::Result<()> {
        let onu_id = self.onu_id;
        let res = super::RESOURCES.onu(onu_id)?;
        let enable = bytes > 0;
        with_session(|session| {
            let onu = session
                .onu(onu_id)
                .copied()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ONU not active in FSL"))?;
            configure_gem_port(session.fsl_mut(), onu.onu_index, 0, res.gem_id, enable)?;
            let max_onus = session.config().init.max_onu_id.min(128) as u8;
            let tcont = alloc_to_tcont(res.alloc_id, 0, max_onus, 0)?;
            let bw = if enable { u32::from(bytes) } else { 0 };
            let alloc = GdbaAllocConfig {
                tcont,
                onu_index: onu.onu_index,
                enable,
                fixed_bw: bw,
                assured_bw: 0,
                max_bw: bw,
                alloc_type: 0,
                priority: 0,
                weight: 0x80,
                ds_period: 7,
                ds_flag: enable,
                ds_bw_flag: false,
                ds_tail_flag: false,
            };
            session.configure_tcont_alloc(&alloc)?;
            if enable {
                let frame = assign_alloc_id_ploam(onu_id, res.alloc_id, true, 0)?;
                for _ in 0..session.config().ploam_repeat {
                    session.send_ploam(&frame, &mut |delay| {
                        thread::sleep(delay);
                        Ok(())
                    })?;
                }
            }
            Ok(())
        })
    }

    fn set_management_grant_floor(&mut self, _bytes: u16) -> io::Result<()> {
        Ok(())
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for FslManagementLease {
    fn drop(&mut self) {
        if !self.disarmed {
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
            let _ = with_session(|session| {
                if session.onu(id).is_some() {
                    let res = super::RESOURCES.onu(id)?;
                    let max_onus = session.config().init.max_onu_id.min(128) as u8;
                    let extra_tconts = alloc_to_tcont(res.alloc_id, 0, max_onus, 0)
                        .map(|t| vec![t])
                        .unwrap_or_default();
                    session.deactivate_onu(id, &extra_tconts, &[res.gem_id], &mut |delay| {
                        thread::sleep(delay);
                        Ok(())
                    })?;
                }
                Ok(())
            });
        }
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

fn ensure_nni_up(stopping: &AtomicBool) -> io::Result<()> {
    let status = std::process::Command::new("ip")
        .args(["link", "set", NNI_INTERFACE, "up"])
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "failed to bring {NNI_INTERFACE} up: {status}"
        )));
    }
    let carrier_path = format!("/sys/class/net/{NNI_INTERFACE}/carrier");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !stopping.load(Ordering::Acquire) {
        if let Ok(val) = fs::read_to_string(&carrier_path) {
            if val.trim() == "1" {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{NNI_INTERFACE} carrier did not come up within 5s"),
    ))
}

fn initialize_hardware(stopping: &AtomicBool) -> io::Result<OpticalState> {
    ensure_nni_up(stopping)?;
    let dev = Device::open(Path::new(FSL_DEVICE_PATH))?;
    let mut session = FslOltSession::new(dev, OltSessionConfig::default());
    session.initialize(&mut |delay| {
        if stopping.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopping"));
        }
        thread::sleep(delay);
        Ok(())
    })?;
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

fn format_serial(sn: &[u8; 8]) -> String {
    let vendor = String::from_utf8_lossy(&sn[..4]);
    format!(
        "{vendor}{:02X}{:02X}{:02X}{:02X}",
        sn[4], sn[5], sn[6], sn[7]
    )
}

pub(crate) fn status_snapshot(
    optics: &OpticalClient,
    pon_engine: &Mutex<PonEngine>,
    provisioning: Option<fttr_api::SyncStatusReply>,
) -> fttr_api::StatusReply {
    let ready = optics.hardware_ready.load(Ordering::Acquire);
    let tx_enabled = with_session(|session| session.fsl_mut().is_tx_enabled()).ok();
    let _ = pon_engine;
    fttr_api::StatusReply {
        ubus: None,
        jobs: None,
        onu_pinning: None,
        fpga_status: if ready { "ready" } else { "initializing" }.into(),
        pon_mac: None,
        provisioning,
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
        switch_ports: Vec::new(),
        sub_gateways: Vec::new(),
        telemetry_gateways: Vec::new(),
        npu: None,
        ipc: None,
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
            let reply = status_snapshot(&optics, &pon_engine, None);
            let _ = serde_json::to_writer(&mut stream, &reply);
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
    let _lifetime = lifetime::Lifetime::start().await.unwrap_or_else(|error| {
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
    let devices_loop = devices.clone();
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
                                Ok(None) if record.frame[2] != PLOAM_MSG_BER_INTERVAL => {
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
                    Ok(newly_active)
                });

                match poll_result {
                    Ok(newly_active) => {
                        for onu in newly_active {
                            let sn = format_serial(&onu.serial_number);
                            let mut vendor = [0u8; 4];
                            vendor.copy_from_slice(&onu.serial_number[..4]);
                            let mut sn_tail = [0u8; 4];
                            sn_tail.copy_from_slice(&onu.serial_number[4..8]);
                            if let Ok(mut engine) = pon_engine_loop.lock() {
                                engine.register_onu(vendor, sn_tail);
                                let _ = engine.activate_onu(onu.onu_id);
                                if let Some(entry) = engine
                                    .active_onus
                                    .iter_mut()
                                    .find(|entry| entry.onu_id == onu.onu_id)
                                {
                                    entry.eq_delay = Some(onu.eqd);
                                    entry.state = crate::pon_engine::ploam::OnuState::O5Operation;
                                }
                            }
                            if let Ok(mut list) = devices_loop.lock() {
                                if !list.iter().any(|d| d.id == sn) {
                                    list.push(SubDevice {
                                        id: sn.clone(),
                                        last_seen: 0,
                                        payload_bytes: 0,
                                    });
                                }
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
            if let Ok(mut guard) = runtime_state().session.lock() {
                *guard = None;
            }
        })
        .ok();

    let _ = crate::runtime::termination().await;
    stopping.store(true, Ordering::Release);
    if let Some(handle) = fsl_thread {
        let _ = handle.join();
    }
}
