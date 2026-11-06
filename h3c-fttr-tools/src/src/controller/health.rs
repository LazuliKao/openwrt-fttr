//! Bounded OMCI GET diagnostic. The optional OMCC trial restores register settings on exit.
use crate::bosa;
#[path = "health_policy.rs"]
mod policy;
use crate::pon_engine::omci::{
    platform::{
        fmcs::{decode_envelope, PacketSocket},
        leases::{lock_onu, ChannelLease, StockOmccLease},
        wake::Wake,
    },
    wire::BaselineFrame,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    io,
    time::{Duration, Instant},
};
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

extern "C" {
    fn signal(number: i32, handler: usize) -> usize;
}
struct CarrierLease {
    id: u8,
    previous: bool,
}
impl Drop for CarrierLease {
    fn drop(&mut self) {
        if let Err(e) = bosa::set_fttr_carrier(self.id as u32, self.previous) {
            eprintln!("carrier restore failed: {e}");
        }
    }
}
fn data_gem_rx(id: u8, bytes: &[u8], kind: u8, vlan: Option<u16>) -> bool {
    kind != 4
        && vlan == Some(0x100 + id as u16)
        && bytes.len() >= 14
        && bytes[6] & 1 == 0
        && bytes[6..12] != [0; 6]
        && bytes[6..12] != [0x55; 6]
        && bytes[6..12] != [0xaa; 6]
}
pub fn diagnostic() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    unsafe {
        signal(2, stop as *const () as usize);
        signal(15, stop as *const () as usize);
    }
    let vendor = crate::ipc::send_ipc_command("STATUS")
        .ok()
        .and_then(|reply| serde_json::from_str::<crate::model::FttrSystemStatus>(&reply).ok())
        .and_then(|status| {
            status
                .sub_gateways
                .into_iter()
                .find(|onu| args.get(1).is_some_and(|id| id == &onu.onu_id.to_string()))
        })
        .and_then(|onu| onu.vendor.as_bytes().try_into().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "live ONU vendor identity unavailable",
            )
        })?;
    run(args, None, vendor, None, None)
}
pub(super) fn watch(
    id: u8,
    cancellation: std::sync::Arc<AtomicBool>,
    vendor: [u8; 4],
    requests: &std::sync::mpsc::Receiver<super::inventory::WorkerRequest>,
    wake: std::sync::Arc<Wake>,
) -> io::Result<()> {
    run(
        vec![
            "integrated-health".into(),
            id.to_string(),
            "60".into(),
            "--stock-omcc".into(),
            "--1905".into(),
            "--watch".into(),
        ],
        Some(cancellation),
        vendor,
        Some(requests),
        Some(wake),
    )
}
fn run(
    args: Vec<String>,
    cancellation: Option<std::sync::Arc<AtomicBool>>,
    vendor: [u8; 4],
    requests: Option<&std::sync::mpsc::Receiver<super::inventory::WorkerRequest>>,
    wake: Option<std::sync::Arc<Wake>>,
) -> io::Result<()> {
    if args.len() < 3
        || args[3..].iter().any(|arg| {
            arg != "--stock-omcc"
                && arg != "--stock-padding"
                && arg != "--mapped-omcc"
                && arg != "--arp"
                && arg != "--1905"
                && arg != "--activate-verified-link"
                && arg != "--watch"
        })
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: fttr-omci-probe ONU_ID SECONDS [--stock-omcc] [--stock-padding] [--mapped-omcc] [--arp] [--1905] [--activate-verified-link] [--watch]"));
    }
    let id: u8 = args[1]
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ONU_ID"))?;
    let seconds: u64 = args[2]
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "SECONDS"))?;
    if id > 15 || seconds == 0 || seconds > 60 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ONU_ID 0..15, SECONDS 1..60",
        ));
    }
    let watch = args.iter().any(|arg| arg == "--watch");
    let mut observations = policy::Observations::new(watch);
    if watch
        && args
            .iter()
            .any(|arg| arg == "--mapped-omcc" || arg == "--stock-padding")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "watch requires ONU 0..15 and the driver OMCI transport",
        ));
    }
    let _operation_lock = lock_onu(id)?;
    if cancellation
        .as_ref()
        .is_some_and(|token| token.load(Ordering::Acquire))
    {
        return Ok(());
    }
    if watch && !super::service::operation_confirmed(id) {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "ONU registration not active",
        ));
    }
    // The watch owns readiness: begin closed and reopen only on fresh evidence.
    let _watch_carrier = if watch {
        bosa::set_fttr_carrier(id as u32, false)?;
        Some(CarrierLease {
            id,
            previous: false,
        })
    } else {
        None
    };
    let omci = if watch {
        PacketSocket::health_omci(id)?
    } else {
        PacketSocket::open("molt_omci")?
    };
    let physical = if watch {
        PacketSocket::health_gem(id)?
    } else {
        PacketSocket::open("fttr0")?
    };
    let discovery = args.iter().any(|arg| arg == "--arp" || arg == "--1905");
    // Watch only consumes physical GEM evidence, not this diagnostic capture.
    let data = if discovery && !watch {
        Some(PacketSocket::open(&crate::pon_engine::onu_interface(id))?)
    } else {
        None
    };
    let _trial = if args[3..].iter().any(|arg| arg == "--stock-omcc") {
        Some(StockOmccLease::open(id)?)
    } else {
        None
    };
    // Restore the initial Alloc-ID before launching watch.
    // A previous diagnostic may have restored a stale host channel mapping.
    let _channel = if watch {
        Some(ChannelLease::open(id, 0)?)
    } else {
        None
    };
    // Avoid reusing the same transaction ID across separate diagnostic runs.
    let mut tci = ((std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_millis() as u16)
        & 0x7fff)
        .max(1);
    let mut payload = [0; 32];
    payload[0] = 0x80; // ONU-G vendor ID, class 256, instance 0.
    let request = BaselineFrame::new(tci, 0x49, 256, 0, payload);
    println!(
        "GET ONU={id} TCI={tci:04x} class=256 instance=0 mask=8000 bytes={:02X?}",
        request.to_bytes()
    );
    let accepted = if args[3..]
        .iter()
        .any(|arg| arg == "--stock-padding" || arg == "--mapped-omcc")
    {
        // Pad the proprietary envelope to 68 bytes.
        // Keep the 48-byte OMCI message and its CRC intact.
        let mut wire = [0u8; 68];
        wire[..6].fill(0xaa);
        wire[6..12].fill(0x55);
        wire[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
        let vid = if args.iter().any(|arg| arg == "--mapped-omcc") {
            0x100 + id as u16
        } else {
            id as u16
        };
        wire[14..16].copy_from_slice(&(0x2000u16 | vid).to_be_bytes());
        wire[16..64].copy_from_slice(&request.to_bytes());
        physical.transmit(&wire)?;
        println!("STOCK_PADDING physical envelope length=68");
        true
    } else {
        observations.send(omci.send_fmcs(id, &request))?
    };
    if accepted {
        println!("TX accepted by socket; awaiting response (not proof of delivery)");
    }
    if discovery {
        let mac = std::fs::read_to_string(format!(
            "/sys/class/net/{}/address",
            crate::pon_engine::onu_interface(id)
        ))?;
        let mac: Vec<u8> = mac
            .trim()
            .split(':')
            .map(|v| u8::from_str_radix(v, 16).map_err(io::Error::other))
            .collect::<io::Result<_>>()?;
        if args.iter().any(|arg| arg == "--arp") {
            let mut arp = [0u8; 60];
            arp[..6].fill(0xff);
            arp[6..12].copy_from_slice(&mac);
            arp[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
            arp[22..28].copy_from_slice(&mac);
            arp[28..32].copy_from_slice(&[192, 168, 2, 254]);
            arp[38..42].copy_from_slice(&[192, 168, 2, 1]);
            // Diagnose the hardware path before marking the virtual port's carrier.
            let mut envelope = [0u8; 68];
            envelope[..12].copy_from_slice(&arp[..12]);
            envelope[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
            envelope[14..16].copy_from_slice(&(0x100u16 + id as u16).to_be_bytes());
            envelope[16..64].copy_from_slice(&arp[12..]);
            physical.transmit(&envelope)?;
            println!("ARP diagnostic target=192.168.2.1 (stock default), sender=192.168.2.254; no IP/interface configuration changed");
            // The backup's WAN PVC uses VLAN 41. Compare tagged and untagged
            // reachability without treating that backup as the active IP profile.
            let mut tagged = Vec::with_capacity(72);
            tagged.extend_from_slice(&envelope[..16]);
            tagged.extend_from_slice(&[0x81, 0, 0, 41]);
            tagged.extend_from_slice(&envelope[16..]);
            physical.transmit(&tagged)?;
            println!("ARP diagnostic also sent with inner VLAN 41 (backup WAN profile)");
        }
        if args.iter().any(|arg| arg == "--1905") {
            // IEEE 1905.1 topology discovery: AL MAC, interface MAC, end TLV.
            // This is discovery only; it does not provision Wi-Fi or a controller.
            let mut wire = [0u8; 68];
            wire[..6].copy_from_slice(&[1, 0x80, 0xc2, 0, 0, 0x13]);
            wire[6..12].copy_from_slice(&mac);
            wire[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
            wire[14..16].copy_from_slice(&(0x100u16 + id as u16).to_be_bytes());
            wire[16..18].copy_from_slice(&0x893au16.to_be_bytes());
            wire[18..26].copy_from_slice(&[0, 0, 0, 0, (tci >> 8) as u8, tci as u8, 0, 0x80]);
            wire[26..29].copy_from_slice(&[1, 0, 6]);
            wire[29..35].copy_from_slice(&mac);
            wire[35..38].copy_from_slice(&[2, 0, 6]);
            wire[38..44].copy_from_slice(&mac);
            // Unlike periodic GETs, this one-shot probe has no in-watch retry.
            // Preserve supervisor retry if it could not be transmitted.
            physical.transmit(&wire)?;
            println!("IEEE1905 topology discovery transmitted; awaiting actual child traffic");
        }
    }
    let start = Instant::now();
    let mut received = 0;
    let mut matched = 0;
    // ONU-G VendorId is an OMCI value and need not equal the PLOAM vendor, e.g.
    // a "SPGA" ONU registered with the serial vendor "SPTC". Liveness is
    // credited to the vendor observed in the first validated response, which
    // the FMCS envelope's ONU-ID and our transaction ID already bind to this
    // session; comparing against the PLOAM vendor alone stalled the watchdog.
    let mut observed_vendor: Option<[u8; 4]> = None;
    let mut gem_received = 0;
    let mut carrier_lease = None;
    let activate = args.iter().any(|arg| arg == "--activate-verified-link");
    let mut last_get = None;
    let mut last_data = None;
    let mut next_get = start + Duration::from_secs(5);
    let mut next_state = start;
    let optical_sn = if watch {
        super::service::onu_serial(id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "ONU identity unavailable")
        })?
    } else {
        String::new()
    };
    let wake = match wake {
        Some(wake) => wake,
        None => std::sync::Arc::new(Wake::new()?),
    };
    let management =
        watch.then(|| crate::provisioning::health_watch::subscribe(&optical_sn, wake.clone()));
    let mut watchdog_start = start;
    let mut telemetry = cancellation
        .clone()
        .filter(|_| watch)
        .map(|token| super::telemetry::Collector::new(id, token));
    let mut next_telemetry = start;
    while !STOP.load(Ordering::Relaxed) && (watch || start.elapsed() < Duration::from_secs(seconds))
    {
        wake.drain();
        if management.as_ref().is_some_and(|state| state.changed()) {
            return Err(io::Error::other("OMCI management configuration changed"));
        }
        if cancellation
            .as_ref()
            .is_some_and(|token| token.load(Ordering::Acquire))
        {
            return Ok(());
        }
        if let Some(request) = requests.and_then(|requests| requests.try_recv().ok()) {
            match request {
                super::inventory::WorkerRequest::Fec(request) => {
                    let result = super::fec::run(&request, cancellation.clone());
                    let responsive = result.is_ok();
                    let _ = request.reply.send(result);
                    next_get = resume_after_omci(&mut last_get, &mut watchdog_start, responsive);
                }
                super::inventory::WorkerRequest::Attribute(request) => {
                    let result = super::attribute_probe::run(&request, cancellation.clone());
                    let responsive = result.is_ok();
                    let _ = request.reply.send(result);
                    next_get = resume_after_omci(&mut last_get, &mut watchdog_start, responsive);
                }
                super::inventory::WorkerRequest::Mib(request) => {
                    let result = if Instant::now() >= request.deadline {
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "MIB request expired before monitor became ready",
                        ))
                    } else {
                        super::inventory::read_owned(
                            id,
                            request.tables,
                            cancellation.clone(),
                            request.deadline,
                            request.progress,
                            request.registration,
                        )
                    };
                    let complete = result.is_ok();
                    let _ = request.reply.send(result);
                    // The upload is serialized with health GETs under the same lock.
                    // Resume with fresh evidence, without rerunning business provisioning.
                    // A completed, validated upload is fresh OMCI liveness evidence.
                    next_get = resume_after_omci(&mut last_get, &mut watchdog_start, complete);
                }
            }
        }
        // Establish the link using the bounded health GET before starting a
        // potentially slow full MIB upload. Otherwise an ONU needing DHCP can
        // remain closed for the entire inventory collection.
        if carrier_lease.is_some()
            && (Instant::now() >= next_telemetry
                || telemetry
                    .as_ref()
                    .is_some_and(|collector| collector.configuration_changed()))
        {
            if let Some(collector) = telemetry.as_mut() {
                let responsive = collector.refresh();
                next_get = resume_after_omci(&mut last_get, &mut watchdog_start, responsive);
            }
            next_telemetry = Instant::now() + Duration::from_secs(60);
        }
        if watch && Instant::now() >= next_state {
            if !super::service::operation_confirmed(id) {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "ONU registration lost; release OMCC and revalidate",
                ));
            }
            next_state = Instant::now() + Duration::from_secs(1);
        }
        if watch && Instant::now() >= next_get {
            let next_tci = (tci.wrapping_add(1) & 0x7fff).max(1);
            if observations
                .send(omci.send_fmcs(id, &BaselineFrame::new(next_tci, 0x49, 256, 0, payload)))?
            {
                tci = next_tci;
            }
            next_get = Instant::now() + Duration::from_secs(5);
        }
        let captures = [
            Some(("molt_omci", &omci)),
            (!watch || last_data.is_none()).then_some(("fttr0", &physical)),
            data.as_ref().map(|d| ("data", d)),
        ];
        let mut rx_buf = [0u8; 2048];
        for (name, capture) in captures.into_iter().flatten() {
            // Bound each drain pass even on a busy physical interface.
            for _ in 0..32 {
                let Some((bytes, kind, vlan)) =
                    observations.receive(capture.receive_into(&mut rx_buf))?
                else {
                    break;
                };
                if !watch {
                    println!(
                        "{}ms {name} packet_type={kind} vlan_tci={vlan:04X?} bytes={:02X?}",
                        start.elapsed().as_millis(),
                        bytes
                    );
                }
                if name == "molt_omci" && kind != 4 {
                    received += 1;
                    if bytes.len() == 56
                        && bytes[..4] == 3u32.to_be_bytes()
                        && bytes[4..8] == (id as u32).to_be_bytes()
                    {
                        match decode_envelope(&bytes, kind, id) {
                            Ok(Some(frame))
                                if frame.transaction_id == tci
                                    && frame.message_type == 0x29
                                    && frame.class_id == 256
                                    && frame.instance_id == 0 =>
                            {
                                let reported = [
                                    frame.payload[3],
                                    frame.payload[4],
                                    frame.payload[5],
                                    frame.payload[6],
                                ];
                                if frame.payload[0] == 0
                                    && frame.payload[1..3] == [0x80, 0]
                                    && (reported == vendor
                                        || *observed_vendor.get_or_insert(reported) == reported)
                                {
                                    matched += 1;
                                    last_get = Some(Instant::now());
                                }
                                if !watch {
                                    println!(
                                        "MATCHED_GET_VALID result={} mask={:02X?} data={:02X?}",
                                        frame.payload[0],
                                        &frame.payload[1..3],
                                        &frame.payload[3..]
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(e) => println!("REJECTED_OMCI {e}"),
                        }
                    }
                }
                // Physical RX, not a socket echo: verified data GEM channel,
                // with a real Ethernet payload distinct from OMCI envelopes.
                if name == "fttr0" && data_gem_rx(id, &bytes, kind, vlan) {
                    gem_received += 1;
                    last_data = Some(Instant::now());
                    if watch && gem_received == 1 {
                        // GEM is session evidence, not a periodic liveness probe.
                        // Once proved, no subsequent business packet is needed.
                        physical.stop_capture()?;
                    }
                    if !watch || gem_received == 1 {
                        println!(
                            "DATA_GEM_RX_CONFIRMED source={:02X?} ethertype={:02X?}",
                            &bytes[6..12],
                            &bytes[12..14]
                        );
                    }
                }
                if (activate || watch)
                    && policy::carrier_ready(last_get, Instant::now())
                    && carrier_lease.is_none()
                {
                    let previous = !watch
                        && std::fs::read_to_string(format!(
                            "/sys/class/net/{}/carrier",
                            crate::pon_engine::onu_interface(id)
                        ))?
                        .trim()
                            == "1";
                    bosa::set_fttr_carrier(id as u32, true)?;
                    carrier_lease = Some(CarrierLease { id, previous });
                    println!("VERIFIED_LINK_ENABLED OMCI_GET=true GEM_RX={} watch={watch}; management/Wi-Fi requires separate verification", last_data.is_some());
                }
            }
        }
        if watch && carrier_lease.is_some() && !policy::carrier_ready(last_get, Instant::now()) {
            drop(carrier_lease.take());
            println!("VERIFIED_LINK_DISABLED stale OMCI evidence");
        }
        observations.report(id, Instant::now());
        if watch && last_get.unwrap_or(watchdog_start).elapsed() >= Duration::from_secs(30) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OMCI watch lost ONU; supervisor must revalidate service configuration",
            ));
        }
        let mut sockets = [omci.raw_fd(), -1, -1];
        let mut count = 1;
        if !watch || last_data.is_none() {
            sockets[count] = physical.raw_fd();
            count += 1;
        }
        if let Some(data) = &data {
            sockets[count] = data.raw_fd();
            count += 1;
        }
        let deadline = if watch {
            let expiry = last_get
                .map(|at| at + Duration::from_secs(15))
                .filter(|_| carrier_lease.is_some());
            [
                Some(next_get),
                Some(next_state),
                carrier_lease.as_ref().map(|_| next_telemetry),
                expiry,
                Some(last_get.unwrap_or(watchdog_start) + Duration::from_secs(30)),
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap()
        } else {
            (start + Duration::from_secs(seconds)).min(Instant::now() + Duration::from_secs(1))
        };
        wake.wait(&sockets[..count], deadline)?;
    }
    println!("RESULT OMCI incoming={received} CRC-valid matched GET responses={matched} data_GEM_received={gem_received}; no OMCI ME configuration was written");
    Ok(())
}

fn resume_after_omci(
    last_get: &mut Option<Instant>,
    watchdog_start: &mut Instant,
    responsive: bool,
) -> Instant {
    let now = Instant::now();
    *last_get = responsive.then_some(now);
    *watchdog_start = now;
    now
}

#[cfg(test)]
mod tests {
    use super::data_gem_rx;
    #[test]
    fn serialized_collection_resumes_watch_without_crediting_failed_reads() {
        // Given a collection longer than the health watchdog budget.
        let old = std::time::Instant::now() - std::time::Duration::from_secs(61);
        for responsive in [true, false] {
            let mut last_get = Some(old);
            let mut watchdog_start = old;
            // When the serialized OMCI operation returns.
            let next_get = super::resume_after_omci(&mut last_get, &mut watchdog_start, responsive);
            // Then a new health GET runs immediately with only validated liveness credited.
            assert_eq!(watchdog_start, next_get);
            assert_eq!(last_get, responsive.then_some(next_get));
            assert!(watchdog_start > old);
        }
    }
    #[test]
    fn captured_child_topology_query_is_data_but_echo_and_omci_are_not() {
        let mut frame = [0u8; 60];
        frame[..14].copy_from_slice(&[
            2, 0xfe, 0x88, 0x75, 0x81, 1, 0x1c, 0x94, 0x68, 0x5d, 0xf9, 0x98, 0x89, 0x3a,
        ]);
        assert!(data_gem_rx(1, &frame, 3, Some(0x0101)));
        assert!(!data_gem_rx(1, &frame, 4, Some(0x0101)));
        for vlan in [None, Some(0x0102), Some(0x2101)] {
            assert!(!data_gem_rx(1, &frame, 3, vlan));
        }
        assert!(!data_gem_rx(1, &frame[..13], 3, Some(0x0101)));
        frame[6..12].fill(0x55);
        assert!(!data_gem_rx(1, &frame, 3, Some(0x0101)));
        frame[6..12].fill(0);
        assert!(!data_gem_rx(1, &frame, 3, Some(0x0101)));
    }
}
