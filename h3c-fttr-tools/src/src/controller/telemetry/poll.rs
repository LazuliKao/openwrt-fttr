use super::Collector;
use crate::pon_engine::omci::platform::leases::{lock_onu, ChannelLease, StockOmccLease};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(in crate::controller) fn start(id: u8, token: Arc<AtomicBool>) {
    crate::runtime::spawn_service("ONU telemetry", async move {
        let mut collector = Collector::new(id, token.clone());
        let mut next = Instant::now();
        while !token.load(Ordering::Acquire) && !crate::runtime::is_stopping() {
            if Instant::now() >= next || collector.configuration_changed() {
                let work = crate::runtime::spawn_blocking(move || {
                    let result = (|| -> std::io::Result<()> {
                        let _owner = lock_onu(id)?;
                        let _stock = StockOmccLease::open(id)?;
                        let _channel = ChannelLease::open(id, 0)?;
                        collector.refresh();
                        Ok(())
                    })();
                    (collector, result)
                })
                .await;
                let Ok((returned, result)) = work else { return };
                collector = returned;
                next = Instant::now() + Duration::from_secs(if result.is_ok() { 30 } else { 1 });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}
