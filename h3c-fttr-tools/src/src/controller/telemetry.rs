use crate::{
    model::{MeasurementEvidence, Measurements},
    pon_engine::omci::{
        attributes::{AttributeBlock, AttributeValue},
        FmcsTransport, OmciSession,
    },
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

pub(super) mod fec;
mod inventory;
mod network;
mod poll;
pub(super) use network::data_path;
pub(super) use poll::start;

#[derive(Clone)]
pub(super) struct Snapshot {
    pub rx: Option<f64>,
    pub tx: Option<f64>,
    pub uptime: Option<f64>,
    pub fec: fttr_api::FecStatus,
    pub inventory: Option<inventory::InventorySample>,
    pub measurements: Measurements,
}

impl Snapshot {
    fn unavailable(reason: &str) -> Self {
        Self {
            rx: None,
            tx: None,
            uptime: None,
            fec: fec::unavailable(reason),
            inventory: None,
            measurements: [
                ("optical_rx_power_dbm", "dBm"),
                ("optical_tx_power_dbm", "dBm"),
                ("uptime_seconds", "seconds"),
            ]
            .into_iter()
            .map(|(field, unit)| {
                (
                    field.into(),
                    MeasurementEvidence::unavailable("OMCI GET", unit, reason),
                )
            })
            .collect(),
        }
    }
}

struct Cached {
    fec_generation: u64,
    snapshot: Snapshot,
    at: Instant,
    token: Arc<AtomicBool>,
}

fn cache() -> &'static Mutex<BTreeMap<u8, Cached>> {
    static CACHE: OnceLock<Mutex<BTreeMap<u8, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) fn snapshot(id: u8, online: bool) -> Snapshot {
    let mut snapshot = cached_snapshot(id, online);
    if !crate::optical::fec_enabled() {
        snapshot.fec = fec::unavailable("fec_support_disabled");
    }
    snapshot.fec.downstream_enabled = super::pon_mac::snapshot().fec_enabled;
    snapshot
}

fn cached_snapshot(id: u8, online: bool) -> Snapshot {
    if !online {
        return Snapshot::unavailable("onu_not_operational");
    }
    let Ok(cache) = cache().lock() else {
        return Snapshot::unavailable("cache_unavailable");
    };
    match cache.get(&id) {
        Some(sample)
            if !sample.token.load(Ordering::Acquire)
                && sample.at.elapsed() < Duration::from_secs(90) =>
        {
            let mut snapshot = sample.snapshot.clone();
            if sample.fec_generation != crate::optical::fec_generation() {
                snapshot.fec = fec::unavailable("awaiting_current_fec_configuration");
            }
            if snapshot.fec.measurement.age_seconds.is_some() {
                snapshot.fec.measurement.age_seconds = Some(sample.at.elapsed().as_secs());
            }
            for evidence in snapshot.measurements.values_mut() {
                evidence.age_seconds = Some(sample.at.elapsed().as_secs());
            }
            snapshot
        }
        Some(_) => Snapshot::unavailable("stale_or_cancelled_registration"),
        None => Snapshot::unavailable("awaiting_serialized_omci_poll"),
    }
}

pub(super) struct Collector {
    id: u8,
    token: Arc<AtomicBool>,
    ani: Option<Result<u16, String>>,
    inventory: Option<inventory::InventorySample>,
    fec: fec::Poll,
    fec_generation: u64,
}

impl Collector {
    pub fn configuration_changed(&self) -> bool {
        self.fec_generation != crate::optical::fec_generation()
    }
    pub fn new(id: u8, token: Arc<AtomicBool>) -> Self {
        Self {
            id,
            token,
            ani: None,
            inventory: None,
            fec: fec::Poll::default(),
            fec_generation: crate::optical::fec_generation(),
        }
    }

    pub fn refresh(&mut self) -> bool {
        if self.ani.is_none()
            || self
                .inventory
                .as_ref()
                .is_none_or(|sample| sample.refresh_due())
        {
            let started = Instant::now();
            match super::inventory::read_owned(
                self.id,
                true,
                Some(self.token.clone()),
                started + Duration::from_secs(600),
                None,
                None,
            ) {
                Ok(report) => {
                    self.inventory =
                        inventory::InventorySample::from_report(&report, Instant::now());
                    let entities: Vec<_> = report
                        .entities
                        .iter()
                        .filter(|e| e.class_id == 263)
                        .collect();
                    self.ani = Some(match entities.as_slice() {
                        [entity] if report.uploaded_complete => Ok(entity.entity_id),
                        [] => Err("ani_g_entity_missing".into()),
                        _ => Err("ani_g_entity_ambiguous_or_incomplete".into()),
                    });
                }
                Err(error) if self.ani.is_none() => self.ani = Some(Err(error.to_string())),
                Err(_) => {}
            }
        }
        let fec_generation = crate::optical::fec_generation();
        if self.fec_generation != fec_generation {
            self.fec = fec::Poll::default();
            self.fec_generation = fec_generation;
        }
        let snapshot = match FmcsTransport::open(self.id) {
            Ok(transport) => {
                let mut session = OmciSession::new(transport);
                let id = self.id;
                session.guard_registration(move || super::service::operation_confirmed(id));
                session.cancel_when(self.token.clone());
                let mut snapshot = Snapshot::unavailable("ani_g_discovery_failed");
                snapshot.inventory = self.inventory.clone();
                let (uptime, evidence) = read(&mut session, (257, 0, 0x0040), false);
                snapshot.uptime = uptime;
                snapshot
                    .measurements
                    .insert("uptime_seconds".into(), evidence);
                match self.ani.as_ref() {
                    Some(Ok(entity)) => {
                        let (rx, evidence) = read(&mut session, (263, *entity, 0x0040), true);
                        snapshot.rx = rx;
                        snapshot
                            .measurements
                            .insert("optical_rx_power_dbm".into(), evidence);
                        let (tx, evidence) = read(&mut session, (263, *entity, 0x0004), true);
                        snapshot.tx = tx;
                        snapshot
                            .measurements
                            .insert("optical_tx_power_dbm".into(), evidence);
                        session.finish_before(Instant::now() + Duration::from_secs(8));
                        if crate::optical::fec_enabled() {
                            let id = self.id;
                            session.guard_registration(move || {
                                super::service::operation_confirmed(id)
                                    && crate::optical::fec_enabled()
                                    && crate::optical::fec_generation() == fec_generation
                            });
                            snapshot.fec = self.fec.read(&mut session, *entity);
                        } else {
                            self.fec = fec::Poll::default();
                            snapshot.fec = fec::unavailable("fec_support_disabled");
                        }
                    }
                    Some(Err(error)) => {
                        for field in ["optical_rx_power_dbm", "optical_tx_power_dbm"] {
                            snapshot.measurements.insert(
                                field.into(),
                                MeasurementEvidence::unavailable(
                                    "OMCI MIB upload / class 263 discovery",
                                    "dBm",
                                    error,
                                ),
                            );
                        }
                    }
                    None => {}
                }
                snapshot
            }
            Err(error) => Snapshot::unavailable(&format!("transport_unavailable: {error}")),
        };
        if matches!(self.ani, Some(Err(_))) {
            self.ani = None;
        }
        let responsive =
            snapshot.uptime.is_some() || snapshot.rx.is_some() || snapshot.tx.is_some();
        if let Ok(mut cache) = cache().lock() {
            cache.insert(
                self.id,
                Cached {
                    fec_generation,
                    snapshot,
                    at: Instant::now(),
                    token: self.token.clone(),
                },
            );
        }
        responsive
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if let Ok(mut cache) = cache().lock() {
            if cache
                .get(&self.id)
                .is_some_and(|sample| Arc::ptr_eq(&sample.token, &self.token))
            {
                cache.remove(&self.id);
            }
        }
    }
}

fn read(
    session: &mut OmciSession,
    target: (u16, u16, u16),
    optical: bool,
) -> (Option<f64>, MeasurementEvidence) {
    session.finish_before(Instant::now() + Duration::from_secs(2));
    let (class, entity, mask) = target;
    let source = format!("OMCI GET class={class} entity=0x{entity:04x} mask=0x{mask:04x}");
    let unit = if optical { "dBm" } else { "seconds" };
    match session.get_attributes(class, entity, mask) {
        Ok(block) => match parse(&block, optical) {
            Some((value, raw)) => (
                Some(value),
                MeasurementEvidence::observed(source, unit, raw),
            ),
            None => (
                None,
                MeasurementEvidence::unavailable(source, unit, "missing_or_invalid_attribute"),
            ),
        },
        Err(error) => (
            None,
            MeasurementEvidence::unavailable(source, unit, format!("get_failed: {error}")),
        ),
    }
}

fn parse(block: &AttributeBlock, optical: bool) -> Option<(f64, String)> {
    let [attribute] = block.attributes.as_slice() else {
        return None;
    };
    let value = match (&attribute.value, optical) {
        (AttributeValue::Signed { value }, true) => f64::from(i16::try_from(*value).ok()?) / 500.0,
        (AttributeValue::Unsigned { value }, false) => f64::from(*value) / 100.0,
        _ => return None,
    };
    Some((value, attribute.raw_hex.clone()))
}

#[cfg(test)]
mod tests;
