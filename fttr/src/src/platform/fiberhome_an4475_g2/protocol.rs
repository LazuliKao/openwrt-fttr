//! FSL SPI wire protocol recovered from the stock module. Addresses are not MMIO.
use crate::platform::diagnostics::RegisterSpace;
use std::io;

pub const MAX_WORDS: usize = 16;
pub const MAX_ADDRESS: u32 = 0x1fff;
pub const MAX_ROW: u32 = 0xfff;
pub const OPT_CONFIG: u32 = 0x102;
pub const TX_DISABLE: u32 = 1 << 4;
pub const DS_FEC_CONFIG: u32 = 0x202;
pub const DS_FEC_ENABLE: u32 = 1;

/// A read is one TX-then-RX message with continuous CS. Errors must not trigger
/// automatic replay: a write or a consuming read may already have taken effect.
pub trait Bus {
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> io::Result<()>;
}

pub struct Fsl<B> {
    pub(super) bus: B,
}

pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x80 != 0 { 0x07 } else { 0 };
        }
    }
    crc
}

pub(super) fn validate(
    space: RegisterSpace,
    address: u32,
    row: u32,
    count: usize,
) -> io::Result<()> {
    if address > MAX_ADDRESS
        || row > MAX_ROW
        || !(1..=MAX_WORDS).contains(&count)
        || (space == RegisterSpace::Register && row != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid FSL address, row or word count",
        ));
    }
    Ok(())
}

fn header(
    space: RegisterSpace,
    address: u32,
    row: u32,
    count: usize,
    write: bool,
) -> io::Result<[u8; 5]> {
    validate(space, address, row, count)?;
    let op = match (space, write) {
        (RegisterSpace::Register, true) => 1,
        (RegisterSpace::Register, false) => 2,
        (RegisterSpace::Memory, true) => 5,
        (RegisterSpace::Memory, false) => 6,
    };
    let mut bytes = [
        ((address >> 8) as u8) << 3 | op,
        address as u8,
        (((count - 1) as u8) << 4) | (row >> 8) as u8,
        row as u8,
        0,
    ];
    bytes[4] = crc8(&bytes[..4]);
    Ok(bytes)
}

impl<B: Bus> Fsl<B> {
    pub fn new(bus: B) -> Self {
        Self { bus }
    }

    pub fn read(
        &mut self,
        space: RegisterSpace,
        address: u32,
        row: u32,
        count: usize,
    ) -> io::Result<Vec<u32>> {
        let mut tx = [0; 6];
        tx[..5].copy_from_slice(&header(space, address, row, count, false)?);
        let mut bytes = vec![0; count * 4];
        self.bus.transfer(&tx, &mut bytes)?;
        let mut words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|word| u32::from_be_bytes(word.try_into().unwrap()))
            .collect();
        if space == RegisterSpace::Memory {
            words.reverse();
        }
        Ok(words)
    }

    pub fn write(
        &mut self,
        space: RegisterSpace,
        address: u32,
        row: u32,
        words: &[u32],
    ) -> io::Result<()> {
        let mut tx = header(space, address, row, words.len(), true)?.to_vec();
        if space == RegisterSpace::Memory {
            for word in words.iter().rev() {
                tx.extend_from_slice(&word.to_be_bytes());
            }
        } else {
            for word in words {
                tx.extend_from_slice(&word.to_be_bytes());
            }
        }
        self.bus.transfer(&tx, &mut [])
    }

    pub fn read_register(&mut self, address: u32) -> io::Result<u32> {
        Ok(self.read(RegisterSpace::Register, address, 0, 1)?[0])
    }

    /// The caller keeps the same exclusive session across all three messages.
    pub fn update_bit(&mut self, address: u32, mask: u32, set: bool) -> io::Result<()> {
        let old = self.read_register(address)?;
        let value = if set { old | mask } else { old & !mask };
        if value != old {
            self.write(RegisterSpace::Register, address, 0, &[value])?;
        }
        let readback = self.read_register(address)?;
        if readback & mask != value & mask {
            return Err(io::Error::other("FSL register readback mismatch"));
        }
        Ok(())
    }

    pub fn set_tx_enabled(&mut self, enabled: bool) -> io::Result<()> {
        self.update_bit(OPT_CONFIG, TX_DISABLE, !enabled)
    }

    pub fn is_tx_enabled(&mut self) -> io::Result<bool> {
        Ok(self.read_register(OPT_CONFIG)? & TX_DISABLE == 0)
    }

    pub fn set_fec_enabled(&mut self, enabled: bool) -> io::Result<()> {
        self.update_bit(DS_FEC_CONFIG, DS_FEC_ENABLE, enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Mock {
        messages: Vec<(Vec<u8>, usize)>,
        rx: Vec<u8>,
        register: Option<u32>,
        ignore_write: bool,
        fail_at: usize,
    }
    impl Bus for Mock {
        fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> io::Result<()> {
            self.messages.push((tx.to_vec(), rx.len()));
            if self.fail_at == self.messages.len() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "mock timeout"));
            }
            if let Some(value) = self.register {
                if rx.is_empty() {
                    if !self.ignore_write {
                        self.register = Some(u32::from_be_bytes(tx[5..9].try_into().unwrap()));
                    }
                } else {
                    rx.copy_from_slice(&value.to_be_bytes());
                }
            } else {
                rx.copy_from_slice(&self.rx[..rx.len()]);
            }
            Ok(())
        }
    }

    #[test]
    fn independent_stock_vectors_and_different_word_orders() {
        assert_eq!(crc8(b"123456789"), 0xf4);
        let mut fsl = Fsl::new(Mock {
            rx: vec![0x55, 0x66, 0x77, 0x88, 0x11, 0x22, 0x33, 0x44],
            ..Mock::default()
        });
        fsl.write(RegisterSpace::Register, 0x40f, 0, &[0x101])
            .unwrap();
        assert_eq!(
            fsl.bus.messages[0],
            (vec![0x21, 0x0f, 0, 0, 0x9f, 0, 0, 1, 1], 0)
        );
        assert_eq!(fsl.read_register(0x202).unwrap(), 0x55667788);
        assert_eq!(fsl.bus.messages[1], (vec![0x12, 2, 0, 0, 0x9d, 0], 4));
        fsl.write(
            RegisterSpace::Memory,
            0x956,
            0x123,
            &[0x11223344, 0x55667788],
        )
        .unwrap();
        assert_eq!(
            fsl.bus.messages[2],
            (
                vec![0x4d, 0x56, 0x11, 0x23, 0x97, 0x55, 0x66, 0x77, 0x88, 0x11, 0x22, 0x33, 0x44],
                0
            )
        );
        assert_eq!(
            fsl.read(RegisterSpace::Memory, 0x956, 0x123, 2).unwrap(),
            [0x11223344, 0x55667788]
        );
        assert_eq!(
            fsl.read(RegisterSpace::Register, 0x956, 0, 2).unwrap(),
            [0x55667788, 0x11223344]
        );
    }

    #[test]
    fn bounds_checked_before_io_and_maximum_transfer_supported() {
        let mut fsl = Fsl::new(Mock {
            rx: vec![0; 64],
            ..Mock::default()
        });
        for (space, addr, row, count) in [
            (RegisterSpace::Register, 0x2000, 0, 1),
            (RegisterSpace::Memory, 0, 0x1000, 1),
            (RegisterSpace::Register, 0, 1, 1),
            (RegisterSpace::Register, 0, 0, 0),
            (RegisterSpace::Memory, 0, 0, 17),
        ] {
            assert_eq!(
                fsl.read(space, addr, row, count).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                fsl.write(space, addr, row, &vec![0; count])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(fsl.bus.messages.is_empty());
        fsl.write(RegisterSpace::Register, 0x1fff, 0, &[0; 16])
            .unwrap();
        fsl.read(RegisterSpace::Memory, 0x1fff, 0xfff, 16).unwrap();
        assert_eq!(fsl.bus.messages[0].0.len(), 69);
        assert_eq!(fsl.bus.messages[1].1, 64);
        assert_eq!(&fsl.bus.messages[1].0[..4], &[0xfe, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn tx_inverse_rmw_preserves_neighbors_and_skips_redundant_write() {
        let original = 0xa5a55aff;
        let mut fsl = Fsl::new(Mock {
            register: Some(original),
            ..Mock::default()
        });
        fsl.set_tx_enabled(true).unwrap();
        assert_eq!(fsl.bus.register, Some(original & !TX_DISABLE));
        assert_eq!(fsl.bus.messages.len(), 3);
        fsl.bus.messages.clear();
        fsl.set_tx_enabled(true).unwrap();
        assert_eq!(fsl.bus.messages.len(), 2);
        fsl.set_tx_enabled(false).unwrap();
        assert_eq!(fsl.bus.register, Some(original));
        fsl.set_fec_enabled(false).unwrap();
        assert_eq!(fsl.bus.register, Some(original & !1));
        fsl.set_fec_enabled(true).unwrap();
        assert_eq!(fsl.bus.register, Some(original));
    }

    #[test]
    fn read_write_and_readback_failures_are_not_replayed() {
        for fail_at in 1..=3 {
            let mut fsl = Fsl::new(Mock {
                register: Some(TX_DISABLE),
                fail_at,
                ..Mock::default()
            });
            assert_eq!(
                fsl.set_tx_enabled(true).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert_eq!(fsl.bus.messages.len(), fail_at);
        }
        let mut fsl = Fsl::new(Mock {
            register: Some(TX_DISABLE),
            ignore_write: true,
            ..Mock::default()
        });
        assert!(fsl
            .set_tx_enabled(true)
            .unwrap_err()
            .to_string()
            .contains("readback"));
        assert_eq!(fsl.bus.messages.len(), 3);
    }
}
