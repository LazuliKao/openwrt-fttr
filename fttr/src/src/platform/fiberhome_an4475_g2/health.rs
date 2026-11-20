//! Serialized OMCI liveness, diagnostic work, and per-ONU VPORT carrier gating
//! on the FSL transport (`fttr1..fttr16` 802.1ad ports over `fttr0`).
use super::{vport, RESOURCES};
use crate::{
    controller::{attribute_probe, fec, inventory, service, telemetry},
    platform::{
        context::PlatformContext,
        openwrt::{onu_lock::lock_onu, wake::Wake},
    },
    pon_engine::omci::OmciSession,
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
        Arc,
    },
    time::{Duration, Instant},
};

const PORT_STALE_TIMEOUT: Duration = Duration::from_secs(15);
const WATCH_LOSS_TIMEOUT: Duration = Duration::from_secs(30);

struct PortLease {
    id: u8,
}

impl Drop for PortLease {
    fn drop(&mut self) {
        if let Err(error) = vport::set_port_up(self.id, false) {
            eprintln!("ONU {} VPORT close failed: {error}", self.id);
        }
    }
}

pub(super) fn watch(
    platform: PlatformContext,
    id: u8,
    cancellation: Arc<AtomicBool>,
    requests: &Receiver<inventory::WorkerRequest>,
    wake: Arc<Wake>,
) -> io::Result<()> {
    let _owner = lock_onu(id)?;
    let serial = service::onu_serial(id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "ONU identity unavailable"))?;
    let _lease = platform.management_lease(id)?;
    let _channel = platform.channel(id, 0)?;
    // Begin closed and reopen only after a fresh OMCI liveness GET succeeds.
    let _ = vport::set_port_up(id, false);
    let _watch_guard = PortLease { id };
    let mut port_lease: Option<PortLease> = None;
    let management = crate::provisioning::health_watch::subscribe(&serial, wake.clone());
    let mut session = OmciSession::new(platform.transport(id)?);
    session.guard_registration(move || service::operation_confirmed(id));
    session.cancel_when(cancellation.clone());
    let mut collector = telemetry::Collector::new(platform, id, cancellation.clone());
    let mut next_get = Instant::now();
    let mut next_telemetry = None;
    let mut last_response = Instant::now();
    loop {
        wake.drain();
        if cancellation.load(Ordering::Acquire) {
            return Ok(());
        }
        if !service::operation_confirmed(id)
            || service::onu_serial(id).as_deref() != Some(serial.as_str())
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "ONU registration changed",
            ));
        }
        if management.changed() {
            return Err(io::Error::other("OMCI management configuration changed"));
        }
        if let Ok(request) = requests.try_recv() {
            match request {
                inventory::WorkerRequest::Fec(request) => {
                    let result = fec::run(platform, &request, Some(cancellation.clone()));
                    let _ = request.reply.send(result);
                }
                inventory::WorkerRequest::Attribute(request) => {
                    let result =
                        attribute_probe::run(platform, &request, Some(cancellation.clone()));
                    let _ = request.reply.send(result);
                }
                inventory::WorkerRequest::Mib(request) => {
                    let result = if Instant::now() >= request.deadline {
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "MIB request expired before monitor became ready",
                        ))
                    } else {
                        inventory::read_owned(
                            platform,
                            id,
                            request.tables,
                            Some(cancellation.clone()),
                            request.deadline,
                            request.progress,
                            request.registration,
                        )
                    };
                    let _ = request.reply.send(result);
                }
            }
            // Diagnostics may take a long time, but completing one is not an
            // OMCI liveness response. Recheck before accepting the ONU as live.
            next_get = Instant::now();
        }
        if Instant::now() >= next_get {
            match session.get(256, 0, 0x8000) {
                Ok(_) => {
                    last_response = Instant::now();
                    if port_lease.is_none() {
                        crate::omci_auth::admit(id, || vport::set_port_up(id, true))?;
                        port_lease = Some(PortLease { id });
                        println!(
                            "VERIFIED_LINK_ENABLED interface={} OMCI_GET=true watch=true; management/Wi-Fi requires separate verification",
                            RESOURCES.interface(id).unwrap_or("unmapped")
                        );
                    }
                    if next_telemetry.is_none() {
                        next_telemetry = Some(last_response);
                    }
                }
                Err(_) if cancellation.load(Ordering::Acquire) => return Ok(()),
                Err(error) if last_response.elapsed() < WATCH_LOSS_TIMEOUT => {
                    eprintln!("ONU {id} OMCI liveness sample unavailable: {error}");
                    if port_lease.is_some() && last_response.elapsed() >= PORT_STALE_TIMEOUT {
                        drop(port_lease.take());
                        println!("VERIFIED_LINK_DISABLED stale OMCI evidence");
                    }
                }
                Err(error) => return Err(error),
            }
            next_get = Instant::now() + Duration::from_secs(5);
        }
        if port_lease.is_some() && last_response.elapsed() >= PORT_STALE_TIMEOUT {
            drop(port_lease.take());
            println!("VERIFIED_LINK_DISABLED stale OMCI evidence");
        }
        if port_lease.is_some() && next_telemetry.is_some_and(|deadline| Instant::now() >= deadline)
        {
            let responsive = collector.refresh();
            let completed = Instant::now();
            if responsive {
                last_response = completed;
            }
            next_get = completed;
            next_telemetry = Some(completed + Duration::from_secs(60));
        }
        wake.wait(&[], next_get.min(Instant::now() + Duration::from_secs(1)))?;
    }
}
