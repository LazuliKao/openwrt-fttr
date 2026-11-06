use std::fs;
use std::io::{self, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::activation::{clear_offline_hardware, RangingSession};
use crate::bosa;
use crate::fpga;
use crate::ipc::{self, SubDevice};
use crate::model::IpcResponse;
use crate::pon_engine::ploam::OnuState;
use crate::pon_engine::ploam::PloamFrame;
use crate::pon_engine::PonEngine;
use std::collections::{BTreeMap, BTreeSet};

mod calibration;
mod ipc_server;
mod ubus;

use fttr_api::{OpticalAction, OpticalError, OpticalReply, OpticalState, ReplyStatus};

#[derive(Clone, Copy)]
enum OpticalCommand {
    Set(fttr_api::OpticalSettings),
    On,
    Off,
    Status,
    Reload,
    Reinitialize,
    Shutdown,
}

impl OpticalCommand {
    fn action(self) -> OpticalAction {
        match self {
            Self::Set(_) => OpticalAction::OpticalReload,
            Self::On => OpticalAction::OpticalOn,
            Self::Off => OpticalAction::OpticalOff,
            Self::Status => OpticalAction::OpticalStatus,
            Self::Reload => OpticalAction::OpticalReload,
            Self::Reinitialize => OpticalAction::OpticalReinitialize,
            Self::Shutdown => OpticalAction::OpticalShutdown,
        }
    }
}

trait OpticalReplyExt {
    fn value(self) -> Value;
    fn unavailable(command: OpticalCommand, ready: bool, error: io::Error) -> Value;
}

impl OpticalReplyExt for OpticalReply {
    fn value(self) -> serde_json::Value {
        serde_json::to_value(self).expect("optical reply is serializable")
    }
    fn unavailable(command: OpticalCommand, ready: bool, error: io::Error) -> serde_json::Value {
        Self {
            settings: None,
            pon_mac: None,
            schema_version: 1,
            status: ReplyStatus::Error,
            action: command.action(),
            desired_enabled: None,
            tx_enabled: None,
            actor_state: OpticalState::Unknown,
            hardware_ready: ready,
            discovery_active: false,
            cleanup_pending: false,
            error: Some(OpticalError {
                code: format!("{:?}", error.kind()),
                message: error.to_string(),
            }),
        }
        .value()
    }
}

struct OpticalRequest {
    command: OpticalCommand,
    reply: SyncSender<serde_json::Value>,
    deadline: Instant,
}

#[derive(Clone)]
struct OpticalClient {
    requests: SyncSender<OpticalRequest>,
    hardware_ready: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
}

impl OpticalClient {
    fn request(&self, command: OpticalCommand) -> io::Result<serde_json::Value> {
        if matches!(command, OpticalCommand::Shutdown) {
            self.stopping.store(true, Ordering::Release);
        } else if self.stopping.load(Ordering::Acquire)
            && !matches!(command, OpticalCommand::Status)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "controller is stopping",
            ));
        }
        if !self.hardware_ready.load(Ordering::Acquire)
            && !matches!(command, OpticalCommand::Shutdown)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hardware initialization incomplete",
            ));
        }
        let (reply, response) = mpsc::sync_channel(1);
        self.requests
            .try_send(OpticalRequest {
                command,
                reply,
                deadline: Instant::now() + Duration::from_secs(5),
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "optical request queue full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::NotConnected, "PLOAM owner unavailable")
                }
            })?;
        response
            .recv_timeout(Duration::from_secs(6))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{error}; optical operation outcome unknown; query status"),
                )
            })
    }
}

trait OpticalBackend {
    fn save_settings(&mut self, _settings: fttr_api::OpticalSettings) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "optical settings are not writable",
        ))
    }
    fn mac_tx_allowed(&self) -> bool {
        true
    }
    fn ready(&self) -> bool;
    fn cleanup_pending(&self) -> bool;
    fn quiesce(&mut self) -> io::Result<()>;
    fn set_tx(&mut self, enabled: bool) -> io::Result<()>;
    fn read_tx(&mut self) -> io::Result<bool>;
    fn desired(&self) -> io::Result<bool>;
    fn save(&mut self, enabled: bool) -> io::Result<()>;
}

struct OpticalActor {
    state: OpticalState,
}

impl OpticalActor {
    fn dispatch(
        &mut self,
        request: &OpticalRequest,
        backend: &mut impl OpticalBackend,
        stopping: &AtomicBool,
    ) -> serde_json::Value {
        if Instant::now() > request.deadline {
            OpticalReply::unavailable(
                request.command,
                backend.ready(),
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "optical request expired before execution",
                ),
            )
        } else if stopping.load(Ordering::Acquire)
            && !matches!(
                request.command,
                OpticalCommand::Shutdown | OpticalCommand::Status
            )
        {
            OpticalReply::unavailable(
                request.command,
                backend.ready(),
                io::Error::new(io::ErrorKind::NotConnected, "controller is stopping"),
            )
        } else {
            self.handle(request.command, backend)
        }
    }

    fn handle(
        &mut self,
        command: OpticalCommand,
        backend: &mut impl OpticalBackend,
    ) -> serde_json::Value {
        let mut tx_enabled = None;
        let mut quiesce_attempted = false;
        let desired = backend.desired();
        let mut desired_enabled = desired.as_ref().ok().copied();
        let result = if !backend.ready() && !matches!(command, OpticalCommand::Shutdown) {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "hardware initialization incomplete",
            ))
        } else {
            match command {
                OpticalCommand::Set(settings) => match backend.save_settings(settings) {
                    Ok(()) => return self.handle(OpticalCommand::Reload, backend),
                    Err(error) => {
                        let mut reply = self.handle(OpticalCommand::Status, backend);
                        reply["status"] = "error".into();
                        reply["error"] = serde_json::json!({"code":"Configuration", "message":error.to_string()});
                        return reply;
                    }
                },
                OpticalCommand::On
                | OpticalCommand::Off
                | OpticalCommand::Reload
                | OpticalCommand::Reinitialize
                | OpticalCommand::Shutdown => {
                    let target = match command {
                        OpticalCommand::On | OpticalCommand::Off => {
                            let enabled = matches!(command, OpticalCommand::On);
                            backend.save(enabled).map(|()| {
                                desired_enabled = Some(enabled);
                                enabled
                            })
                        }
                        OpticalCommand::Shutdown => Ok(false),
                        _ => desired,
                    };
                    // Persist first; failures must not falsely report a saved setting.
                    if let Err(error) = target {
                        let mut reply = if matches!(
                            command,
                            OpticalCommand::Reload | OpticalCommand::Reinitialize
                        ) {
                            // Invalid desired state cannot authorize continued discovery.
                            self.handle(OpticalCommand::Shutdown, backend)
                        } else {
                            self.handle(OpticalCommand::Status, backend)
                        };
                        reply["status"] = "error".into();
                        reply["action"] = serde_json::to_value(command.action()).unwrap();
                        reply["error"] =
                            serde_json::json!({"code":"Configuration","message":error.to_string()});
                        return reply;
                    }
                    let enabled = target.unwrap();
                    if matches!(command, OpticalCommand::Reload)
                        && self.state
                            == if enabled {
                                OpticalState::Enabled
                            } else {
                                OpticalState::Disabled
                            }
                        && backend.read_tx().ok() == Some(enabled)
                    {
                        let mut reply = self.handle(OpticalCommand::Status, backend);
                        reply["action"] = serde_json::to_value(command.action()).unwrap();
                        return reply;
                    }
                    let was_enabled = self.state != OpticalState::Disabled;
                    self.state = OpticalState::Unknown;
                    let quiesced = if enabled
                        && !was_enabled
                        && !matches!(command, OpticalCommand::Reinitialize)
                    {
                        Ok(())
                    } else {
                        quiesce_attempted = true;
                        backend.quiesce()
                    };
                    // A failed quiesce must still permit reset, never a fresh enable.
                    let target_tx = enabled && quiesced.is_ok();
                    let transition = backend.set_tx(target_tx).and_then(|()| {
                        let actual = backend.read_tx()?;
                        tx_enabled = Some(actual);
                        if actual == target_tx {
                            Ok(())
                        } else {
                            Err(io::Error::other("optical TX readback mismatch"))
                        }
                    });
                    let transition = transition.map_err(|error| {
                        if enabled {
                            let reset = backend.set_tx(false);
                            tx_enabled = backend.read_tx().ok();
                            if let Err(reset) = reset {
                                return io::Error::other(format!(
                                    "{error}; recovery reset failed: {reset}"
                                ));
                            }
                        }
                        if tx_enabled.is_none() {
                            tx_enabled = backend.read_tx().ok();
                        }
                        error
                    });
                    let result = match (quiesced, transition) {
                        (Ok(()), result) => result,
                        (Err(error), Ok(())) => Err(error),
                        (Err(quiesce), Err(transition)) => Err(io::Error::other(format!(
                            "quiesce failed: {quiesce}; optical transition failed: {transition}"
                        ))),
                    };
                    result.map(|()| {
                        self.state = if enabled {
                            OpticalState::Enabled
                        } else {
                            OpticalState::Disabled
                        };
                    })
                }
                OpticalCommand::Status => backend.read_tx().and_then(|actual| {
                    tx_enabled = Some(actual);
                    if self.state == OpticalState::Enabled && !actual {
                        self.state = OpticalState::Disabled;
                        quiesce_attempted = true;
                        backend.quiesce()?;
                    }
                    desired.map(|_| ())
                }),
            }
        };
        let result = match result {
            Err(error) => {
                self.state = OpticalState::Unknown;
                if backend.ready() && !quiesce_attempted {
                    match backend.quiesce() {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(io::Error::other(format!(
                            "{error}; quiesce failed: {cleanup}"
                        ))),
                    }
                } else {
                    Err(error)
                }
            }
            result => result,
        };
        OpticalReply {
            settings: Some(crate::optical::settings()),
            pon_mac: Some(super::pon_mac::snapshot()),
            schema_version: 1,
            status: if result.is_ok() {
                ReplyStatus::Success
            } else {
                ReplyStatus::Error
            },
            action: command.action(),
            desired_enabled,
            tx_enabled,
            actor_state: self.state,
            hardware_ready: backend.ready(),
            discovery_active: backend.ready()
                && backend.mac_tx_allowed()
                && self.state == OpticalState::Enabled
                && !backend.cleanup_pending(),
            cleanup_pending: backend.cleanup_pending(),
            error: result.err().map(|error| OpticalError {
                code: format!("{:?}", error.kind()),
                message: error.to_string(),
            }),
        }
        .value()
    }
}

struct RuntimeOptics<'a> {
    ready: bool,
    ranging: &'a mut Option<RangingSession>,
    online: &'a mut BTreeMap<u8, RangingSession>,
    pending_cleanup: &'a mut BTreeSet<u8>,
    services: &'a mut super::service::Services,
    engine: &'a Mutex<PonEngine>,
}

impl RuntimeOptics<'_> {
    fn quiesce_with(
        &mut self,
        mut lower_carrier: impl FnMut(u8) -> io::Result<()>,
    ) -> io::Result<()> {
        self.pending_cleanup.extend(self.online.keys().copied());
        if let Some(session) = self.ranging.take() {
            self.pending_cleanup.insert(session.id);
        }
        self.online.clear();
        let mut errors = Vec::new();
        for &id in self.pending_cleanup.iter() {
            self.services.stop(id);
            match self.engine.lock() {
                Ok(mut engine) => engine.mark_offline(id),
                Err(error) => errors.push(error.to_string()),
            }
            if let Err(error) = lower_carrier(id) {
                errors.push(format!("ONU {id} carrier: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(errors.join("; ")))
        }
    }
}

impl OpticalBackend for RuntimeOptics<'_> {
    fn save_settings(&mut self, settings: fttr_api::OpticalSettings) -> io::Result<()> {
        crate::optical::save(settings)
    }
    fn mac_tx_allowed(&self) -> bool {
        super::pon_mac::snapshot().tx_enabled == Some(true)
    }
    fn ready(&self) -> bool {
        self.ready
    }
    fn cleanup_pending(&self) -> bool {
        !self.pending_cleanup.is_empty()
    }
    fn quiesce(&mut self) -> io::Result<()> {
        self.quiesce_with(|id| bosa::set_fttr_carrier(u32::from(id), false))
    }
    fn set_tx(&mut self, enabled: bool) -> io::Result<()> {
        bosa::set_optical_tx_enable(enabled)
    }
    fn read_tx(&mut self) -> io::Result<bool> {
        bosa::get_optical_tx_enable()
    }
    fn desired(&self) -> io::Result<bool> {
        crate::optical::load_enabled()
    }
    fn save(&mut self, enabled: bool) -> io::Result<()> {
        crate::optical::save_enabled(enabled)
    }
}

fn setup_bridge() {
    println!("[*] Initializing downstream FTTR DSA switch ports (fttr1 ~ fttr16)...");
    let _ = Command::new("ip")
        .args(["link", "set", "molt_ploam", "up"])
        .status();
    let _ = Command::new("ip")
        .args(["link", "set", "molt_omci", "up"])
        .status();
    let _ = Command::new("ip")
        .args(["link", "set", "fttr0", "up"])
        .status();
    for id in 1..=16 {
        let ifname = format!("fttr{id}");
        let _ = Command::new("ip")
            .args(["link", "set", &ifname, "up"])
            .status();
        let _ = Command::new("ip")
            .args(["link", "set", &ifname, "master", "br-lan"])
            .status();
    }
    println!("[+] Downstream FTTR switch ports fttr1 ~ fttr16 bridged to br-lan.");
}

fn start_ipc_server(
    listener: UnixListener,
    devices: Arc<Mutex<Vec<SubDevice>>>,
    pon_engine: Arc<Mutex<PonEngine>>,
    services: super::service::States,
    optics: OpticalClient,
) {
    ipc_server::start(listener, ipc_handler(devices, pon_engine, services, optics));
}

fn start_ubus_server(
    devices: Arc<Mutex<Vec<SubDevice>>>,
    pon_engine: Arc<Mutex<PonEngine>>,
    services: super::service::States,
    optics: OpticalClient,
    stopping: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    let create_methods = |devices: Arc<Mutex<Vec<SubDevice>>>,
                          pon_engine: Arc<Mutex<PonEngine>>,
                          services: super::service::States,
                          optics: OpticalClient,
                          stopping: Arc<AtomicBool>|
     -> Vec<(String, openwrt_ubus::MethodHandler)> {
        ubus::methods(optics, devices, pon_engine, services, stopping)
    };

    std::thread::Builder::new()
        .name("ubus-server".into())
        .spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let mut server = match openwrt_ubus::UbusServer::new(None) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[-] Native ubus server connect failed: {e}; retrying in 3s");
                        std::thread::sleep(Duration::from_secs(3));
                        continue;
                    }
                };

                let methods = create_methods(
                    devices.clone(),
                    pon_engine.clone(),
                    services.clone(),
                    optics.clone(),
                    stopping.clone(),
                );
                // Exclusively register the native system 'fttr' object.
                if let Err(e) = server.add_object(fttr_api::OBJECT, methods) {
                    eprintln!("[-] Failed to register 'fttr' ubus object: {e}; retrying in 3s");
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
                println!("[+] Registered native ubus object 'fttr'. Starting ubus event loop.");

                if let Err(e) = server.run_loop() {
                    eprintln!("[-] ubus server run_loop exited: {e}");
                }
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                println!("[!] ubus disconnected; reconnecting in 2s...");
                std::thread::sleep(Duration::from_secs(2));
            }
            println!("[*] ubus server thread stopped cleanly.");
        })
        .ok()
}

fn fpga_diagnostic_snapshot(
    request: &crate::fpga_diag::Request,
    pon_engine: &Arc<Mutex<PonEngine>>,
    services: &super::service::States,
) -> std::io::Result<crate::fpga_diag::Snapshot> {
    let slots = request.slots()?;
    // Identity stays inside the daemon. Compare it and the worker generation
    // around the non-atomic reads rather than attributing counters to a new ONU.
    let registrations = || -> std::io::Result<_> {
        let engine = pon_engine
            .lock()
            .map_err(|_| std::io::Error::other("PON state unavailable"))?;
        let states = services
            .lock()
            .map_err(|_| std::io::Error::other("ONU workers unavailable"))?;
        Ok(slots
            .iter()
            .map(|id| {
                let registered = engine.active_onus.iter().find(|onu| {
                    onu.onu_id == *id
                        && onu.state == crate::pon_engine::ploam::OnuState::O5Operation
                });
                (
                    *id,
                    registered.map(|onu| {
                        (
                            onu.full_sn_str(),
                            states.get(id).map(|state| state.generation),
                        )
                    }),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>())
    };
    let before = registrations()?;
    let mut snapshot = crate::fpga_diag::snapshot(request, crate::bosa::read_fpga_reg)?;
    if before != registrations()? {
        return Err(std::io::Error::other(
            "ONU registration changed during FPGA diagnostic snapshot",
        ));
    }
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    snapshot.daemon_instance = Some(
        INSTANCE
            .get_or_init(|| {
                format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                )
            })
            .clone(),
    );
    for onu in &mut snapshot.onus {
        onu.generation = before
            .get(&onu.slot)
            .and_then(|registered| registered.as_ref())
            .and_then(|(_, generation)| *generation);
    }
    Ok(snapshot)
}

fn ipc_handler(
    devices: Arc<Mutex<Vec<SubDevice>>>,
    pon_engine: Arc<Mutex<PonEngine>>,
    services: super::service::States,
    optics: OpticalClient,
) -> impl Fn(std::os::unix::net::UnixStream, &str) + Send + Sync + 'static {
    move |mut stream, line| {
        if let Some(request) = line.strip_prefix("HL202 ") {
            #[cfg(feature = "device-hl202")]
            super::hl202::start(
                stream,
                request.to_string(),
                pon_engine.clone(),
                services.clone(),
            );
            #[cfg(not(feature = "device-hl202"))]
            {
                let _ = request;
                let _ = stream.write_all(b"{\"schema_version\":1,\"error\":{\"code\":\"adapter_disabled\",\"message\":\"HL202 adapter not compiled\"}}");
            }
        } else if let Some(request) = line.strip_prefix("FPGA_DIAG ") {
            let result = serde_json::from_str::<crate::fpga_diag::Request>(request)
                .map_err(std::io::Error::other)
                .and_then(|request| fpga_diagnostic_snapshot(&request, &pon_engine, &services));
            match result {
                Ok(snapshot) => {
                    let _ = serde_json::to_writer(&mut stream, &snapshot);
                }
                Err(error) => {
                    let _ = serde_json::to_writer(
                        &mut stream,
                        &serde_json::json!({"schema_version": 1, "error": error.to_string()}),
                    );
                }
            }
            let _ = stream.write_all(b"\n");
        } else if let Some(request) = line.strip_prefix("FEC_PM ") {
            let result = serde_json::from_str::<super::fec::Command>(request)
                .map_err(std::io::Error::other)
                .and_then(|command| super::fec::control(command, &services));
            let response =
                result.unwrap_or_else(|error| serde_json::json!({"error": error.to_string()}));
            let _ = serde_json::to_writer(&mut stream, &response);
            let _ = stream.write_all(b"\n");
        } else if let Some(request) = line.strip_prefix("OMCI_ATTRIBUTE ") {
            let result = serde_json::from_str::<super::attribute_probe::Command>(request)
                .map_err(std::io::Error::other)
                .and_then(|command| super::attribute_probe::control(command, &services));
            let response =
                result.unwrap_or_else(|error| serde_json::json!({"error": error.to_string()}));
            let _ = serde_json::to_writer(&mut stream, &response);
            let _ = stream.write_all(b"\n");
        } else if let Some(request) = line.strip_prefix("MIB ") {
            let request = request.to_string();
            let result = serde_json::from_str::<super::inventory::MibCommand>(&request)
                .map_err(std::io::Error::other)
                .and_then(|command| {
                    let progress = command
                        .progress_id
                        .as_deref()
                        .map(super::inventory::MibProgressReporter::new)
                        .transpose()?;
                    super::inventory::control(command.onu_id, command.tables, progress, &services)
                });
            match result {
                Ok(report) => {
                    let _ = serde_json::to_writer(&mut stream, &report);
                }
                Err(error) => {
                    let response = serde_json::json!({"schema_version": 1, "error": {"code": format!("{:?}", error.kind()), "message": error.to_string()}});
                    let _ = serde_json::to_writer(&mut stream, &response);
                }
            }
            let _ = stream.write_all(b"\n");
        } else if let Some(request) = line.strip_prefix("PROVISIONING ") {
            let _ = stream.write_all(crate::provisioning::control(request).as_bytes());
        } else if let Some(_request) = line.strip_prefix("ACS ") {
            #[cfg(feature = "acs")]
            let response = crate::acs::control(_request);
            #[cfg(not(feature = "acs"))]
            let response =
                "{\"error\":\"ACS adapter not compiled (enable Cargo feature acs)\"}".to_string();
            let _ = stream.write_all(response.as_bytes());
        } else if line == "CAPABILITIES" {
            let _ = writeln!(stream, "{}", crate::capabilities::json());
        } else if line == "STATUS" {
            let provisioning = match serde_json::from_value(crate::provisioning::snapshot()) {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    let _ = stream.write_all(
                        IpcResponse::err("Invalid provisioning snapshot contract")
                            .to_json_line()
                            .as_bytes(),
                    );
                    return;
                }
            };
            let devs = devices.lock().unwrap();
            let engine = pon_engine.lock().unwrap();
            let mut status = super::status::snapshot(&engine, &services, (&devs, provisioning));
            status.ipc = Some(ipc_server::metrics::snapshot());
            if let Ok(json_str) = serde_json::to_string_pretty(&status) {
                let _ = stream.write_all(json_str.as_bytes());
                let _ = stream.write_all(b"\n");
            }
        } else if let Some(command) = match line {
            "OPTICAL ON" => Some(OpticalCommand::On),
            "OPTICAL OFF" => Some(OpticalCommand::Off),
            "OPTICAL STATUS" => Some(OpticalCommand::Status),
            "OPTICAL RELOAD" => Some(OpticalCommand::Reload),
            "OPTICAL REINITIALIZE" => Some(OpticalCommand::Reinitialize),
            "OPTICAL SHUTDOWN" => Some(OpticalCommand::Shutdown),
            _ => None,
        } {
            let response = match optics.request(command) {
                Ok(response) => response,
                Err(error) => OpticalReply::unavailable(
                    command,
                    optics.hardware_ready.load(Ordering::Acquire),
                    error,
                ),
            };
            if let Err(error) = serde_json::to_writer(&mut stream, &response)
                .map_err(io::Error::other)
                .and_then(|()| stream.write_all(b"\n"))
            {
                eprintln!("[-] Optical IPC response failed: {error}");
            }
        } else {
            let _ = stream.write_all(
                IpcResponse::err("unsupported command")
                    .to_json_line()
                    .as_bytes(),
            );
        }
    }
}

fn restore_optical_state(
    enabled: io::Result<bool>,
    stopping: &AtomicBool,
    prepare: impl FnOnce() -> io::Result<()>,
    mut set_tx: impl FnMut(bool) -> io::Result<()>,
) -> io::Result<OpticalState> {
    set_tx(false)?;
    let enabled = enabled?;
    prepare()?;
    if enabled && !stopping.load(Ordering::Acquire) {
        if let Err(error) = set_tx(true) {
            return match set_tx(false) {
                Ok(()) => Err(error),
                Err(reset) => Err(io::Error::other(format!(
                    "{error}; recovery reset failed: {reset}"
                ))),
            };
        }
        if stopping.load(Ordering::Acquire) {
            set_tx(false)?;
            return Ok(OpticalState::Disabled);
        }
        Ok(OpticalState::Enabled)
    } else {
        Ok(OpticalState::Disabled)
    }
}

fn initialize_hardware(stopping: &AtomicBool) -> io::Result<OpticalState> {
    if stopping.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "controller stopping",
        ));
    }
    let fw_path = Path::new("/lib/firmware/FTTR_TOP.sbit");
    if fw_path.exists() {
        fpga::load_bitstream(fw_path)?;
    }
    let start = Instant::now();
    loop {
        if stopping.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "controller stopping",
            ));
        }
        match fpga::get_fpga_status() {
            fpga::FpgaStatus::Ready => break,
            fpga::FpgaStatus::Failed => return Err(io::Error::other("FPGA programming failed")),
            _ if start.elapsed() >= Duration::from_secs(90) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "FPGA readiness timed out",
                ))
            }
            _ => thread::sleep(Duration::from_millis(100)),
        }
    }
    // Read committed intent before any optical enable. Invalid configuration fails closed.
    restore_optical_state(
        crate::optical::load_enabled(),
        stopping,
        || {
            bosa::configure_discovery_timing()?;
            let (tx, fec) = super::pon_mac::desired();
            let applied = bosa::set_gpon_downstream(tx, fec);
            super::pon_mac::record(
                applied
                    .as_ref()
                    .map(|value| *value)
                    .map_err(|error| io::Error::other(error.to_string())),
            );
            applied?;
            setup_bridge();
            for id in 0..crate::pon_engine::ONU_SLOTS {
                clear_offline_hardware(id)?;
            }
            Ok(())
        },
        bosa::set_optical_tx_enable,
    )
}

pub fn run() {
    crate::runtime::block_on(run_async());
}

async fn run_async() {
    println!("[*] Starting H3C FTTR Master Controller Daemon (Rust fttrd)...");
    let _owner = crate::optical::claim_owner().unwrap_or_else(|error| {
        eprintln!("[-] Optical ownership failed: {error}");
        std::process::exit(1);
    });
    #[cfg(feature = "npu")]
    let npu = crate::npu::Service::start();
    let lifetime = super::lifetime::Lifetime::start()
        .await
        .unwrap_or_else(|error| {
            eprintln!("[-] MQTT startup failed: {error}");
            #[cfg(feature = "npu")]
            npu.stop();
            std::process::exit(1);
        });
    #[cfg(feature = "acs")]
    crate::acs::start_from_env();

    let _ = fs::create_dir_all(ipc::SUBDEV_DIR);

    // 1. Data Structures & IPC Socket Server (Start immediately so fttrctl can query)
    let devices: Arc<Mutex<Vec<SubDevice>>> = Arc::new(Mutex::new(Vec::new()));
    let pon_engine = Arc::new(Mutex::new(PonEngine::new()));
    super::service::init_pon_engine(pon_engine.clone());
    crate::provisioning::start(pon_engine.clone());
    let hardware_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let (optical_requests, optical_receiver) = mpsc::sync_channel(2);
    let shutdown = OpticalClient {
        requests: optical_requests.clone(),
        hardware_ready: hardware_ready.clone(),
        stopping: stopping.clone(),
    };
    let service_states: super::service::States = Arc::new(Mutex::new(BTreeMap::new()));

    if let Ok(listener) = ipc::create_socket_listener() {
        start_ipc_server(
            listener,
            devices.clone(),
            pon_engine.clone(),
            service_states.clone(),
            OpticalClient {
                requests: optical_requests.clone(),
                hardware_ready: hardware_ready.clone(),
                stopping: stopping.clone(),
            },
        );
        println!("[+] IPC socket server listening on {}", ipc::SOCKET_PATH);
    }

    let _ubus_thread = start_ubus_server(
        devices.clone(),
        pon_engine.clone(),
        service_states.clone(),
        OpticalClient {
            requests: optical_requests,
            hardware_ready: hardware_ready.clone(),
            stopping: stopping.clone(),
        },
        stopping.clone(),
    );

    // One hardware owner performs initialization, transitions and PLOAM reception.
    {
        let pon_engine_ploam = pon_engine.clone();
        let _ = thread::Builder::new()
            .name("ploam-loop".into())
            .stack_size(256 * 1024)
            .spawn(move || {
            println!("[+] Started background PLOAM discovery & reception loop.");
            let initial = initialize_hardware(&stopping);
            let (ready, state) = match initial {
                Ok(state) => (true, state),
                Err(error) => {
                    eprintln!("[-] Hardware initialization failed; discovery paused: {error}");
                    if let Err(reset) = bosa::set_optical_tx_enable(false) {
                        eprintln!("[-] Recovery reset failed: {reset}");
                    }
                    (false, OpticalState::Unknown)
                }
            };
            hardware_ready.store(ready, Ordering::Release);
            let mut last_disc = Instant::now() - Duration::from_secs(5);
            let mut last_grant = Instant::now() - Duration::from_secs(5);
            let mut last_report = Instant::now();
            let mut last_idle_sample = Instant::now() - Duration::from_secs(30);
            let (mut tx_ok, mut tx_errors, mut rx_frames, mut rx_errors) = (0u64, 0u64, 0u64, 0u64);
            let (mut grants, mut grant_errors) = (0u64, 0u64);
            let (mut rx_valid, mut rx_bad_crc_or_length) = (0u64, 0u64);
            // Only one quiet-window activation at a time. B4 event bits
            // 16..31 map directly to ONU-ID slots 0..15; no ACK is guessed.
            let mut ranging: Option<RangingSession> = None;
            let mut online: BTreeMap<u8, RangingSession> = BTreeMap::new();
            let mut pending_cleanup = BTreeSet::new();
            let mut services = super::service::Services::new(service_states);
            let mut grant_cycle = 0usize;
            let mut optics = OpticalActor { state };
            let mut mac_allowed = false;
            let mut last_mac_poll = Instant::now() - Duration::from_secs(2);
            let mut last_mac_desired = None;
            loop {
                let ready = hardware_ready.load(Ordering::Acquire);
                {
                    let mut backend = RuntimeOptics {
                        ready, ranging: &mut ranging, online: &mut online,
                        pending_cleanup: &mut pending_cleanup, services: &mut services,
                        engine: &pon_engine_ploam,
                    };
                    for request in optical_receiver.try_iter().take(2) {
                        let response = optics.dispatch(&request, &mut backend, &stopping);
                        if let Err(error) = request.reply.send(response) {
                            eprintln!("[-] Optical reply receiver unavailable: {error}");
                        }
                    }
                    if last_mac_poll.elapsed() >= Duration::from_secs(1) {
                        if let Err(error) = crate::optical::refresh() {
                            eprintln!("[-] Optical configuration: {error}");
                        }
                    }
                    let desired = super::pon_mac::desired();
                    if ready && !stopping.load(Ordering::Acquire)
                        && (last_mac_poll.elapsed() >= Duration::from_secs(1) || last_mac_desired != Some(desired))
                    {
                        // Invalidate workers and carrier before stopping GPON transmission.
                        // Even failed cleanup must still permit disabling the MAC.
                        let result = super::pon_mac::apply(desired, || backend.quiesce(), bosa::set_gpon_downstream);
                        mac_allowed = super::pon_mac::record(result);
                        last_mac_poll = Instant::now();
                        last_mac_desired = Some(desired);
                    }
                }
                if stopping.load(Ordering::Acquire) || !ready || !mac_allowed || optics.state != OpticalState::Enabled {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                for id in pending_cleanup.clone() {
                    if let Ok(_lock) = crate::pon_engine::omci::platform::leases::lock_onu(id) {
                        match clear_offline_hardware(id) {
                            Ok(()) => {
                                pending_cleanup.remove(&id);
                                last_disc = Instant::now() - Duration::from_secs(1);
                                last_grant = Instant::now() - Duration::from_secs(1);
                                println!("[+] ONU {id} offline cleanup complete; fresh discovery enabled");
                            }
                            Err(e) => eprintln!(
                                "[-] ONU {id} offline cleanup failed; discovery held: {e}"
                            ),
                        }
                    }
                }
                if !pending_cleanup.is_empty() {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                // Use independent 200ms overhead and 100ms serial grant
                // timers; advertise before granting.
                if ranging.is_none() && last_disc.elapsed() >= Duration::from_millis(200) {
                    let engine = pon_engine_ploam.lock().unwrap();
                    let frames = engine.build_discovery_frames();
                    drop(engine);
                    for frame in &frames {
                        let bytes = frame.to_bytes();
                        if let Err(e) = bosa::send_ploam_msg(&bytes) {
                            tx_errors += 1;
                            eprintln!("[-] Discovery PLOAM send failed: {e}");
                        } else {
                            tx_ok += 1;
                        }
                    }
                    last_disc = Instant::now();
                }
                if last_grant.elapsed() >= Duration::from_millis(50) {
                    // One directed activation window, then round-robin live ONU
                    // windows. With no activation the first window discovers new SNs.
                    let target = super::activation::grant_target(
                        ranging.as_ref().map(|session| session.id),
                        online.keys().copied(),
                        grant_cycle,
                    );
                    grant_cycle = grant_cycle.wrapping_add(1);
                    let grant = (u32::from(target) << 18)
                        | if target == 0xfe {
                            bosa::SERIAL_NUMBER_GRANT
                        } else {
                            (202 * 19440 + 124) / 125
                        };
                    match bosa::write_fpga_reg(bosa::FPGA_SERIAL_NUMBER_GRANT, grant) {
                        Ok(()) => grants += 1,
                        Err(e) => {
                            grant_errors += 1;
                            if grant_errors == 1 || last_report.elapsed() >= Duration::from_secs(30)
                            {
                                eprintln!("[-] Discovery grant failed: {e}");
                            }
                        }
                    }
                    last_grant = Instant::now();
                    if let Some(RangingSession {
                        id,
                        ranged: Some(at),
                        allocated,
                        confirmed,
                        ..
                    }) = ranging.as_mut()
                    {
                        if !*allocated && at.elapsed() >= Duration::from_millis(40) {
                            let frame = PloamFrame::assign_alloc_id(*id);
                            let mut sent = true;
                            for _ in 0..3 {
                                if bosa::send_ploam_msg(&frame.to_bytes()).is_ok() {
                                    tx_ok += 1;
                                } else {
                                    tx_errors += 1;
                                    sent = false;
                                }
                            }
                            *allocated = sent;
                        }
                        if *allocated && !*confirmed {
                            // Request_Password is downstream ID 09 (upstream
                            // Password is 02). Keep strict validation on replies.
                            let frame = PloamFrame::new(*id, 9, [0; 10]);
                            if bosa::send_ploam_msg(&frame.to_bytes()).is_ok() {
                                tx_ok += 1;
                            } else {
                                tx_errors += 1;
                            }
                        }
                    }
                }

                if let Some(RangingSession {
                    id,
                    armed,
                    measured,
                    ranged,
                    ..
                }) = ranging.as_mut()
                {
                    if ranged.is_none() {
                        if let Ok(events) = bosa::read_fpga_reg(0xb4) {
                            let mask = 1u32 << (16 + *id);
                            if events & mask == 0 {
                                *armed = true;
                            }
                            if *armed && events & mask != 0 {
                                let address = 0x01010020 + 4 * u32::from(*id);
                                if let Ok(bits) = bosa::read_fpga_reg(address) {
                                    if bits != 0 {
                                        println!("[+] Ranging measurement: ONU={id}, address=0x{address:08X}, EqD bits={bits}");
                                        *measured = Some(bits);
                                    }
                                }
                            }
                        }
                    }
                }

                // Poll for incoming PLOAM frames
                if let Ok(rx_data) = bosa::recv_ploam_msg() {
                    if !rx_data.is_empty() {
                        rx_frames += 1;
                        // Upstream 04 is No message, not a ranging/error frame.
                        if rx_data.get(1) != Some(&4)
                            || last_idle_sample.elapsed() >= Duration::from_secs(30)
                        {
                            println!("[*] Upstream PLOAM raw: {:02X?}", rx_data);
                            if rx_data.get(1) == Some(&4) {
                                last_idle_sample = Instant::now();
                            }
                        }
                    }
                    if PloamFrame::from_bytes(&rx_data).is_some() {
                        rx_valid += 1;
                        if let Some(session) = ranging.as_mut() {
                            session.observe(&rx_data, Instant::now());
                        }
                        if let Some(session) = online.get_mut(&rx_data[0]) {
                            session.observe(&rx_data, Instant::now());
                        }
                        let msg_id = rx_data[1];
                        if msg_id == 0x01 && rx_data[0] == 0xff && ranging.is_none() {
                            let mut vendor = [0u8; 4];
                            let mut sn = [0u8; 4];
                            vendor.copy_from_slice(&rx_data[2..6]);
                            sn.copy_from_slice(&rx_data[6..10]);
                            println!(
                                "[+] PLOAM: Captured upstream ONU serial: Vendor={:?}, SN={:02X?}",
                                String::from_utf8_lossy(&vendor),
                                sn
                            );

                            let mut engine = pon_engine_ploam.lock().unwrap();
                            let onu = match engine.try_register_onu(vendor, sn) {
                                Ok(onu) => onu,
                                Err(error) => {
                                    eprintln!("[-] ONU allocation rejected: {error}");
                                    continue;
                                }
                            };
                            let onu_id = onu.onu_id;
                            if online.contains_key(&onu_id) || pending_cleanup.contains(&onu_id) {
                                drop(engine);
                                continue;
                            }
                            if onu_id > 15 {
                                eprintln!("[-] ONU-ID {onu_id} has no verified FPGA ranging slot");
                                drop(engine);
                                continue;
                            }
                            let act_frames = engine.activate_onu(onu_id);
                            drop(engine);
                            let baseline = bosa::read_fpga_reg(0xb4);
                            let mut assigned = baseline.is_ok();
                            if assigned {
                                for _ in 0..3 {
                                    for f in &act_frames {
                                        if let Err(e) = bosa::send_ploam_msg(&f.to_bytes()) {
                                            assigned = false;
                                            tx_errors += 1;
                                            eprintln!("[-] Assign ONU-ID {onu_id} failed: {e}");
                                        } else {
                                            tx_ok += 1;
                                        }
                                    }
                                }
                            }
                            if assigned {
                                let mut serial = [0u8; 8];
                                serial[..4].copy_from_slice(&vendor);
                                serial[4..].copy_from_slice(&sn);
                                ranging = Some(RangingSession {
                                    id: onu_id,
                                    serial,
                                    started: Instant::now(),
                                    armed: baseline.unwrap() & (1u32 << (16 + onu_id)) == 0,
                                    matched: false,
                                    measured: None,
                                    ranged: None,
                                    allocated: false,
                                    confirmed: false,
                                    last_valid_rx: Instant::now(),
                                });
                                last_grant = Instant::now() - Duration::from_secs(1);
                                println!("[*] ONU {onu_id} assigned; requesting directed ranging");
                            }
                        } else if let Some(RangingSession {
                            id,
                            serial,
                            matched,
                            ranged,
                            allocated,
                            confirmed,
                            ..
                        }) = ranging.as_mut()
                        {
                            if msg_id == 1 && rx_data[0] == *id && rx_data[2..10] == serial[..] {
                                *matched = true;
                                println!("[+] Ranging matched unicast serial for ONU {id}");
                            } else if !*confirmed
                                && rx_data[0] == *id
                                && ranged.is_some()
                                && (msg_id == 2
                                    || (*allocated
                                        && msg_id == 9
                                        && rx_data[2] == 0x0e
                                        && rx_data[3..12]
                                            == PloamFrame::assign_alloc_id(*id).to_bytes()[..9]))
                            {
                                *confirmed = true;
                                println!("[+] ONU {id} CRC-valid post-ranging response msg=0x{msg_id:02X}: O5 protocol confirmed");
                                if let Some(onu) = pon_engine_ploam
                                    .lock()
                                    .unwrap()
                                    .active_onus
                                    .iter_mut()
                                    .find(|o| o.onu_id == *id)
                                {
                                    onu.state = OnuState::O5Operation;
                                }
                                // Data-plane carrier still requires separate hardware evidence.
                            }
                        }
                    } else if !rx_data.is_empty() {
                        rx_bad_crc_or_length += 1;
                    }
                } else {
                    rx_errors += 1;
                }

                if let Some(RangingSession {
                    id,
                    matched,
                    measured,
                    ranged,
                    ..
                }) = ranging.as_mut()
                {
                    if *matched && ranged.is_none() {
                        if let Some(bits) = *measured {
                            let frame = PloamFrame::assign_delay(*id, bits);
                            let mut sent = true;
                            for _ in 0..3 {
                                if bosa::send_ploam_msg(&frame.to_bytes()).is_ok() {
                                    tx_ok += 1;
                                } else {
                                    tx_errors += 1;
                                    sent = false;
                                }
                            }
                            if sent {
                                *ranged = Some(Instant::now());
                                if let Some(onu) = pon_engine_ploam
                                    .lock()
                                    .unwrap()
                                    .active_onus
                                    .iter_mut()
                                    .find(|o| o.onu_id == *id)
                                {
                                    onu.eq_delay = Some(bits);
                                }
                                println!("[+] Ranging_Time sent: ONU={id}, measured EqD={bits} bits, copies=3; awaiting valid O5 response");
                            }
                        }
                    }
                }

                if ranging.as_ref().is_some_and(|session| session.confirmed) {
                    let session = ranging.take().unwrap();
                    let id = session.id;
                    let vendor = pon_engine_ploam
                        .lock()
                        .unwrap()
                        .active_onus
                        .iter()
                        .find(|onu| onu.onu_id == id)
                        .unwrap()
                        .vendor_str();
                    online.insert(id, session);
                    services.start(id, vendor);
                    println!("[+] ONU {id} moved to independent online set; discovery remains enabled (online={})", online.len());
                }
                let expired: Vec<_> = online
                    .iter()
                    .filter(|(_, session)| session.expired(Instant::now()))
                    .map(|(&id, _)| id)
                    .collect();
                for id in expired {
                    online.remove(&id);
                    services.stop(id);
                    pon_engine_ploam.lock().unwrap().mark_offline(id);
                    let _ = bosa::set_fttr_carrier(id as u32, false);
                    pending_cleanup.insert(id);
                    println!("[-] ONU {id} online evidence expired; independent cleanup scheduled");
                }
                if let Some(session) = ranging.as_ref().filter(|s| s.expired(Instant::now())) {
                    let id = session.id;
                    println!("[-] ONU {id} activation expired (confirmed={}); revoke O5/EqD and re-register", session.confirmed);
                    pon_engine_ploam.lock().unwrap().mark_offline(id);
                    if let Err(e) = bosa::set_fttr_carrier(id as u32, false) {
                        eprintln!("[-] ONU {id} carrier disable: {e}");
                    }
                    ranging = None;
                    pending_cleanup.insert(id);
                }

                if last_report.elapsed() >= Duration::from_secs(30) {
                    println!("[*] PLOAM counters: SPI TX accepted={tx_ok}, TX errors={tx_errors}, serial grants={grants}, grant errors={grant_errors}, upstream frames={rx_frames}, valid frames={rx_valid}, bad CRC/length={rx_bad_crc_or_length}, RX errors={rx_errors}");
                    last_report = Instant::now();
                }

                // Poll the receiver every 10ms.
                thread::sleep(Duration::from_millis(10));
            }
        });
    }

    // 4. Keep the controller alive and own MQTT supervision when compiled in.
    tokio::select! {
        result = lifetime.wait(devices) => {
            eprintln!("[-] Controller supervision stopped: {result:?}");
            #[cfg(feature = "npu")]
            npu.stop();
            std::process::exit(1);
        }
        result = crate::runtime::termination() => {
            if let Err(error) = result { eprintln!("[-] Signal handler failed: {error}");
            }
            #[cfg(feature = "acs")]
            crate::acs::shutdown();
            #[cfg(feature = "npu")]
            npu.stop();
            crate::runtime::request_stop();
            let optical = crate::runtime::spawn_blocking(move || shutdown.request(OpticalCommand::Shutdown));
            let (optical, ()) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(4), optical),
                crate::runtime::shutdown(),
            );
            match optical {
                Ok(Ok(Ok(_))) => (),
                result => eprintln!("[-] Optical shutdown not confirmed: {result:?}"),
            }
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::pon_engine::ploam::PloamFrame;

    #[test]
    fn optical_boot_restores_off_without_any_enable_and_stop_interrupts_restore() {
        for (desired, stop, expected) in [
            (false, false, vec![false]),
            (true, false, vec![false, true]),
            (true, true, vec![false]),
        ] {
            let stopping = AtomicBool::new(stop);
            let mut writes = Vec::new();
            let state = restore_optical_state(
                Ok(desired),
                &stopping,
                || Ok(()),
                |enabled| {
                    writes.push(enabled);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(writes, expected);
            assert!(
                state
                    == if desired && !stop {
                        OpticalState::Enabled
                    } else {
                        OpticalState::Disabled
                    }
            );
        }
        let stopping = AtomicBool::new(false);
        let mut writes = Vec::new();
        let state = restore_optical_state(
            Ok(true),
            &stopping,
            || Ok(()),
            |enabled| {
                writes.push(enabled);
                if enabled {
                    stopping.store(true, Ordering::Release);
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(state == OpticalState::Disabled);
        assert_eq!(writes, [false, true, false]);
    }

    #[test]
    fn optical_boot_failure_never_authorizes_discovery_and_reasserts_reset() {
        let stopping = AtomicBool::new(false);
        let mut writes = Vec::new();
        let failed = restore_optical_state(
            Ok(true),
            &stopping,
            || Ok(()),
            |enabled| {
                writes.push(enabled);
                if enabled {
                    Err(io::Error::other("calibration failed after reset release"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(failed.is_err());
        assert_eq!(writes, [false, true, false]);
        writes.clear();
        assert!(restore_optical_state(
            Err(io::Error::other("invalid configuration")),
            &stopping,
            || panic!("must not prepare invalid configuration"),
            |enabled| {
                writes.push(enabled);
                Ok(())
            }
        )
        .is_err());
        assert_eq!(writes, [false]);
    }

    #[test]
    fn only_valid_directed_rx_keeps_operation_alive_and_activation_is_bounded() {
        let start = Instant::now();
        let mut session = RangingSession {
            id: 1,
            serial: [0; 8],
            started: start,
            armed: true,
            matched: true,
            measured: Some(248248),
            ranged: Some(start),
            allocated: true,
            confirmed: true,
            last_valid_rx: start,
        };
        let later = start + Duration::from_secs(14);
        session.observe(&PloamFrame::new(2, 4, [0; 10]).to_bytes(), later);
        let mut bad = PloamFrame::new(1, 4, [0; 10]).to_bytes();
        bad[3] ^= 1;
        session.observe(&bad, later);
        session.observe(&bad[..12], later);
        assert!(session.expired(start + Duration::from_secs(15)));
        session.observe(&PloamFrame::new(1, 4, [0; 10]).to_bytes(), later);
        assert!(!session.expired(start + Duration::from_secs(15)));
        assert!(session.expired(later + Duration::from_secs(15)));
        // A measured but never confirmed activation must not remain O4 forever.
        session.confirmed = false;
        assert!(session.expired(start + Duration::from_secs(10)));
    }
}
