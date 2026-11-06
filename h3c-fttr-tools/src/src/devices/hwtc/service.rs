//! HWTC standard OMCI bridge profile, initially verified on K155-11.
use crate::controller::business::verify;
use crate::pon_engine::omci::{
    platform::leases::{lock_onu, ChannelLease, StockOmccLease},
    FmcsTransport, OmciSession,
};
use std::{
    io,
    sync::{atomic::AtomicBool, Arc},
};

pub fn configure(id: u8, cancellation: Option<Arc<AtomicBool>>) -> io::Result<()> {
    configure_profile(id, cancellation, super::Profile::Standard)
}

/// First-stage V152 provisioning: a transparent, untagged Ethernet path to
/// the non-OMCI domain. Internal WAN allocation still requires device evidence.
pub(super) fn configure_profile(
    id: u8,
    cancellation: Option<Arc<AtomicBool>>,
    profile: super::Profile,
) -> io::Result<()> {
    if id >= crate::pon_engine::ONU_SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ONU ID must be 0..15",
        ));
    }
    let _lock = lock_onu(id)?;
    let mut session = OmciSession::new(FmcsTransport::open(id)?);
    session.guard_registration(move || crate::controller::service::operation_confirmed(id));
    if let Some(token) = cancellation {
        session.cancel_when(token);
    }
    let mut lease = StockOmccLease::open(id)?;
    let _channel = ChannelLease::open(id, 0)?;
    verify(&mut session, 256, 0, 0x8000, b"HWTC")?;
    if profile == super::Profile::V152 {
        super::v152::service::verify_identity(&mut session)?;
    }
    if profile == super::Profile::K155 {
        super::k155::service::verify_identity(&mut session)?;
    }
    // Dispatch is vendor-wide by request. Confirm the live ONU-G vendor before
    // any profile writes; per-model topology still requires validation.
    // The live MIB exposes these ONU-created endpoints and priority queues.
    for (class, instance) in [
        (262, 0x8000),
        (277, 0x8000),
        (11, 0x101),
        (11, 0x102),
        (329, 0x601),
    ] {
        session.get(class, instance, 0x8000)?;
    }
    let mut loid = [0; 24];
    loid[..10].copy_from_slice(b"subGateway");
    let mut password = [0; 12];
    password[..10].copy_from_slice(b"0000000000");
    verify_credential(&mut session, 0x0040, "LOID", &loid)?;
    verify_credential(&mut session, 0x0020, "password", &password)?;
    // G.988 ONU-G credentials status, not H3C private class 65530.
    session.set_attributes(
        256,
        0,
        &[(
            12,
            crate::pon_engine::omci::attributes::AttributeValue::number(1),
        )],
    )?;
    verify(&mut session, 256, 0, 0x0010, &[1])?;
    println!("HWTC_CREDENTIALS_STATUS_READBACK_OK");
    let port = 0x100u16 + id as u16;
    super::bridge::configure(&mut session, port)?;
    if profile == super::Profile::V152 {
        super::v152::service::after_bridge(&mut session)?;
    }
    verify(&mut session, 256, 0, 0x8000, b"HWTC")?;
    println!("HWTC_SERVICE_READBACK_OK gem={port:04x}; data evidence still required");
    crate::controller::management::bootstrap(&mut session, id, 0x601);
    lease.disarm();
    println!("HWTC_LEASE_PERSISTED id={id}");
    Ok(())
}

// Credentials are checked, never overwritten. Include both values in failures
// so status consumers can distinguish configuration errors from OMCI failures.
fn verify_credential(
    session: &mut OmciSession,
    mask: u16,
    name: &str,
    expected: &[u8],
) -> io::Result<()> {
    let response = session.get(256, 0, mask)?;
    let reported = response.payload.get(3..3 + expected.len()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HWTC {name} readback truncated mask={mask:04x}"),
        )
    })?;
    if reported != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            credential_mismatch(name, reported, expected),
        ));
    }
    println!("READBACK_OK class=256 instance=0000 mask={mask:04x}");
    Ok(())
}

fn credential_mismatch(name: &str, reported: &[u8], expected: &[u8]) -> String {
    let text = |bytes: &[u8]| {
        let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let reported_text = text(reported);
    let expected_text = text(expected);
    format!("HWTC {name} verification failed: ONU reported={reported_text:?}, required={expected_text:?}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn password_mismatch_shows_values_without_nul_padding() {
        let error = super::credential_mismatch("password", b"00000000\0\0\0\0", b"0000000000\0\0");
        assert_eq!(
            error,
            "HWTC password verification failed: ONU reported=\"00000000\", required=\"0000000000\""
        );
        assert!(!error.contains('\0'));
    }
}
