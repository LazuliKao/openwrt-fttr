//! HG6371F-C V40.00.M5000: bounded SCTY scheduling comparison on an existing bridge.
//!
//! Unlike TY-FS803G, this ONU exposes T-CONT 8001 and VEIP 0701. Do not run
//! SCTY's fixed-entity cleanup/creation against it. This opt-in trial requires
//! an already provisioned, slot-matching generic GEM and changes only grant
//! policy. It cannot bootstrap an empty MIB and claims neither CWMP nor IP service.
use crate::controller::business::verify;
use crate::pon_engine::omci::{
    platform::leases::{lock_onu, ChannelLease, StockOmccLease},
    FmcsTransport, OmciSession,
};
use std::{
    io,
    sync::{atomic::AtomicBool, Arc},
};

pub(super) fn configure(id: u8, cancellation: Option<Arc<AtomicBool>>) -> io::Result<()> {
    if id >= crate::pon_engine::ONU_SLOTS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "ONU slot"));
    }
    let _lock = lock_onu(id)?;
    let mut session = OmciSession::new(FmcsTransport::open(id)?);
    session.guard_registration(move || crate::controller::service::operation_confirmed(id));
    if let Some(token) = cancellation {
        session.cancel_when(token);
    }
    let mut lease = StockOmccLease::open(id)?;
    let _channel = ChannelLease::open(id, 0)?;
    let port = 0x100 + u16::from(id);

    // Fail closed on a different topology, stale slot binding or vendor. All
    // probes are read-only; no ONU MEs are created, deleted or overwritten.
    verify_existing_bridge(&mut session, port)?;
    println!("FIBERHOME_HG6371_SCTY_GRANT_TRIAL_START id={id} gem={port:04x}");
    lease.set_business_grant_floor(1300)?;
    lease.set_management_grant_floor(60)?;
    verify(&mut session, 256, 0, 0x8000, b"FHTT")?;
    lease.disarm();
    println!("FIBERHOME_HG6371_SCTY_GRANT_TRIAL_READY id={id} business=1300 management=60 GEM_RX=unverified");
    Ok(())
}

fn verify_existing_bridge(session: &mut OmciSession, port: u16) -> io::Result<()> {
    verify(session, 256, 0, 0x8000, b"FHTT")?;
    verify(session, 329, 0x0701, 0x8000, &[0])?;
    verify(session, 262, 0x8001, 0x8000, &port.to_be_bytes())?;
    let expected = [port.to_be_bytes(), 0x8001u16.to_be_bytes()].concat();
    verify(session, 268, port, 0xc000, &expected)?;
    verify(session, 266, port, 0x8000, &port.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pon_engine::omci::{session::OmciTransport, wire::BaselineFrame};
    use std::collections::VecDeque;

    struct ReadOnlyScript {
        reads: VecDeque<(u16, u16, u16, Vec<u8>)>,
        pending: Option<BaselineFrame>,
    }
    impl OmciTransport for ReadOnlyScript {
        fn send(&mut self, request: &BaselineFrame) -> io::Result<()> {
            // Even a failed ownership probe must never attempt an OMCI write.
            assert_eq!(request.message_type, 0x49);
            let (class, instance, mask, data) = self.reads.pop_front().expect("unexpected GET");
            assert_eq!((request.class_id, request.instance_id), (class, instance));
            assert_eq!(&request.payload[..2], &mask.to_be_bytes());
            let mut payload = [0; 32];
            payload[1..3].copy_from_slice(&mask.to_be_bytes());
            payload[3..3 + data.len()].copy_from_slice(&data);
            self.pending = Some(BaselineFrame::new(
                request.transaction_id,
                0x29,
                class,
                instance,
                payload,
            ));
            Ok(())
        }
        fn receive(&mut self) -> io::Result<Option<BaselineFrame>> {
            Ok(self.pending.take())
        }
    }
    #[test]
    fn rejects_stale_slot_or_foreign_tcont_before_grant_trial() {
        for (alloc, tcont, accepted) in [
            (0x101u16, 0x8001u16, true),
            (0x105, 0x8001, false),
            (0x101, 0x8000, false),
        ] {
            let reads = vec![
                (256, 0, 0x8000, b"FHTT".to_vec()),
                (329, 0x0701, 0x8000, vec![0]),
                (262, 0x8001, 0x8000, alloc.to_be_bytes().to_vec()),
                (
                    268,
                    0x101,
                    0xc000,
                    [0x101u16.to_be_bytes(), tcont.to_be_bytes()].concat(),
                ),
                (266, 0x101, 0x8000, vec![1, 1]),
            ];
            let mut session = OmciSession::new(ReadOnlyScript {
                reads: reads.into(),
                pending: None,
            });
            assert_eq!(
                verify_existing_bridge(&mut session, 0x101).is_ok(),
                accepted
            );
        }
    }
}
