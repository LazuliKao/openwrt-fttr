use super::{
    calibration::{self, CalibrationTable},
    initialization::{
        self,
        defaults::{
            apply_gpon_defaults, configure_nni_vport, DEFAULT_NNI_ETH_TPID,
            DEFAULT_NNI_GEMPORT_OFFSET, NNI_ETH_TPID_CFG, NNI_GEMPORT_OFFSET_CFG, VLAN_TYPE_CFG,
        },
        InitConfig, RAM_INIT_STATUS, SERDES_STATUS,
    },
    optical::{i2c_dev::I2cDevice, OpticalBus},
    profile::{initialize_ux3326s, StockPacer, UX3326S_DEVICE_ADDRESS},
    protocol::{self, Fsl},
    transport::Device,
};
use crate::platform::{
    backend::PlatformDiagnostics,
    diagnostics::{BosaCalibrationEvidence, OltInitEvidence, RegisterSession, RegisterSpace},
    resources::PonMacReadback,
};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

const DOWNSTREAM_OPTICAL_I2C_DEV: &str = "/dev/i2c-0";
const FSL_DEVICE_PATH: &str = "/dev/fsl-1fbd4000.fsl";

pub(crate) struct FslDiagnostics;
impl PlatformDiagnostics for FslDiagnostics {
    fn validate_register_access(
        &self,
        space: RegisterSpace,
        address: u32,
        row: u32,
        words: usize,
    ) -> io::Result<()> {
        protocol::validate(space, address, row, words)
    }
    fn open_registers(&self, endpoint: &Path) -> io::Result<Box<dyn RegisterSession>> {
        Ok(Box::new(Fsl::new(Device::open(endpoint)?)))
    }
    fn read_bosa_reg(&self, address: u32) -> io::Result<u32> {
        let reg = u8::try_from(address).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "UX3326S I2C register address must be within 0..=0xff",
            )
        })?;
        let mut bus = OpticalBus::new(I2cDevice::open(Path::new(DOWNSTREAM_OPTICAL_I2C_DEV))?);
        let mut val = [0u8; 1];
        bus.read(UX3326S_DEVICE_ADDRESS, reg, &mut val)?;
        Ok(u32::from(val[0]))
    }
    fn write_bosa_reg(&self, address: u32, value: u32) -> io::Result<()> {
        let reg = u8::try_from(address).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "UX3326S I2C register address must be within 0..=0xff",
            )
        })?;
        let byte = u8::try_from(value).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "UX3326S I2C register value must be within 0..=0xff",
            )
        })?;
        let mut bus = OpticalBus::new(I2cDevice::open(Path::new(DOWNSTREAM_OPTICAL_I2C_DEV))?);
        bus.write(UX3326S_DEVICE_ADDRESS, reg, &[byte])
    }
    fn calibrate_bosa(&self, source: &Path, offset: u64) -> io::Result<BosaCalibrationEvidence> {
        // Same precondition as the runtime path: programming happens only with
        // downstream TX off, and a refused TX-off must not start 802 writes.
        // The FSL device is exclusive, so this also fails fast while fttrd is
        // running; TX stays off afterwards (the operator re-enables explicitly).
        let mut fsl = Fsl::new(Device::open(Path::new(FSL_DEVICE_PATH))?);
        fsl.set_tx_enabled(false)?;
        drop(fsl);
        let mut file = File::open(source)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut raw = [0u8; calibration::TABLE_BYTES];
        file.read_exact(&mut raw)?;
        program_table(&raw, || Ok(()))
    }
    fn set_optical_tx_enable(&self, enabled: bool) -> io::Result<()> {
        let mut fsl = Fsl::new(Device::open(Path::new(FSL_DEVICE_PATH))?);
        fsl.set_tx_enabled(enabled)
    }
    fn get_optical_tx_enable(&self) -> io::Result<bool> {
        let mut fsl = Fsl::new(Device::open(Path::new(FSL_DEVICE_PATH))?);
        fsl.is_tx_enabled()
    }
}

pub(super) fn program_table(
    raw: &[u8],
    mut checkpoint: impl FnMut() -> io::Result<()>,
) -> io::Result<BosaCalibrationEvidence> {
    checkpoint()?;
    let table = CalibrationTable::parse(&raw)?;
    let mut bus = OpticalBus::new(I2cDevice::open(Path::new(DOWNSTREAM_OPTICAL_I2C_DEV))?);
    struct SleepPacer;
    impl StockPacer for SleepPacer {
        fn after_write(&mut self, _loop_iterations: u32) -> io::Result<()> {
            std::thread::sleep(Duration::from_micros(200));
            Ok(())
        }
    }
    let mut pacer = SleepPacer;
    let chip = initialize_ux3326s(&mut bus, &table, &mut pacer, checkpoint)?;
    Ok(BosaCalibrationEvidence {
        chip_id: chip.discriminator(),
        page: chip.current_page(),
        table_bytes: raw.len(),
    })
}

impl RegisterSession for Fsl<Device> {
    fn read(
        &mut self,
        space: RegisterSpace,
        address: u32,
        row: u32,
        words: usize,
    ) -> io::Result<Vec<u32>> {
        Fsl::read(self, space, address, row, words)
    }
    fn write(
        &mut self,
        space: RegisterSpace,
        address: u32,
        row: u32,
        words: &[u32],
    ) -> io::Result<()> {
        Fsl::write(self, space, address, row, words)
    }
    fn pon_mac_readback(&mut self) -> io::Result<PonMacReadback> {
        Fsl::pon_mac_readback(self)
    }
    fn set_optical_tx_enable(&mut self, enabled: bool) -> io::Result<()> {
        self.set_tx_enabled(enabled)
    }
    fn set_downstream_fec_enable(&mut self, enabled: bool) -> io::Result<()> {
        self.set_fec_enabled(enabled)
    }
    fn initialize_olt(&mut self) -> io::Result<OltInitEvidence> {
        let init = InitConfig::default();
        initialization::release_reset(self, &init, &mut |delay| {
            std::thread::sleep(delay);
            Ok(())
        })?;
        let onu_id_offset = apply_gpon_defaults(self, init.max_onu_id)?;
        configure_nni_vport(self, DEFAULT_NNI_GEMPORT_OFFSET, DEFAULT_NNI_ETH_TPID)?;
        Ok(OltInitEvidence {
            onu_id_offset,
            serdes_status: self.read_register(SERDES_STATUS)?,
            ram_init_status: self.read_register(RAM_INIT_STATUS)?,
            nni_eth_tpid: self.read_register(NNI_ETH_TPID_CFG)? & 0xffff,
            nni_gem_port_offset: self.read_register(NNI_GEMPORT_OFFSET_CFG)? & 0x0fff,
            vlan_type_cfg: self.read_register(VLAN_TYPE_CFG)? & 0xffff,
        })
    }
}
