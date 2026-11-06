//! Explicit opt-in experimental FiberHome profiles; no automatic enrollment.
mod hg6371f_c;

pub(super) fn matches(vendor: &str, equipment: Option<&str>) -> bool {
    vendor == "FHTT" && equipment == Some("HG6371F-C")
}

pub(super) fn configure(
    id: u8,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> std::io::Result<()> {
    hg6371f_c::configure(id, cancellation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trial_requires_exact_vendor_and_model_and_is_not_auto_selected() {
        assert!(matches("FHTT", Some("HG6371F-C")));
        assert!(!matches("SCTY", Some("HG6371F-C")));
        assert!(!matches("FHTT", Some("HG6371F")));
        assert!(!matches("FHTT", None));
        let identity = crate::model::OnuIdentity {
            equipment_id: Some("HG6371F-C".into()),
            ..Default::default()
        };
        assert_eq!(
            crate::devices::select("FHTT", &identity),
            crate::devices::Adapter::Unavailable
        );
    }
}
