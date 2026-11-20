//! FiberHome AN4475-G2 implementation of the shared platform operation boundary.
use super::{diagnostics::FslDiagnostics, health, runtime, An4475G2, RESOURCES};
use crate::{
    platform::{
        adapter::{PlatformAdapter, PlatformDescriptor},
        backend::{PlatformBackend, PlatformDiagnostics},
        context::{ChannelGuard, ManagementLease, PlatformContext},
        resources::Resources,
        runtime::SelectionError,
    },
    pon_engine::{omci::session::OmciTransport, ploam::PloamFrame},
};
use std::io;

impl PlatformBackend for An4475G2 {
    fn descriptor(&self) -> PlatformDescriptor {
        Self::DESCRIPTOR
    }

    fn resources(&self) -> &'static Resources {
        &RESOURCES
    }

    fn operations(&self) -> fttr_api::PlatformOperations {
        fttr_api::PlatformOperations {
            optical_control: true,
            transceiver_telemetry: false,
            fpga_diagnostics: false,
            bosa_diagnostics: true,
            calibration_profile: Some(fttr_api::PlatformCalibrationProfile::Ux3326),
            npu_offload: false,
            omci_mib: true,
            downstream_fec: true,
        }
    }

    fn transport(&self, id: u8) -> io::Result<Box<dyn OmciTransport>> {
        Ok(Box::new(runtime::FslOmciTransport::new(id)))
    }

    fn management_lease(&self, id: u8) -> io::Result<Box<dyn ManagementLease>> {
        Ok(Box::new(runtime::FslManagementLease::open(id)?))
    }

    fn channel(&self, _id: u8, _channel: u16) -> io::Result<Box<dyn ChannelGuard>> {
        Ok(Box::new(runtime::FslChannelGuard))
    }

    fn exchange_channel(&self, _id: u8, channel: u16) -> io::Result<u16> {
        Ok(channel)
    }

    fn send_ploam(&self, frame: &PloamFrame) -> io::Result<()> {
        runtime::send_ploam(frame)
    }

    fn revoke_business(&self, id: u8) -> io::Result<()> {
        runtime::revoke_business(id)
    }

    fn watch(
        &self,
        platform: PlatformContext,
        id: u8,
        cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _vendor: [u8; 4],
        requests: &std::sync::mpsc::Receiver<crate::controller::inventory::WorkerRequest>,
        wake: std::sync::Arc<crate::platform::openwrt::wake::Wake>,
    ) -> io::Result<()> {
        health::watch(platform, id, cancellation, requests, wake)
    }

    fn run(&self, platform: PlatformContext) -> Result<(), SelectionError> {
        runtime::run(platform);
        Ok(())
    }

    fn diagnostics(&self) -> Option<&dyn PlatformDiagnostics> {
        Some(&FslDiagnostics)
    }
}
