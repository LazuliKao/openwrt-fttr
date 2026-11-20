use super::*;

#[test]
fn stock_upstream_rate_metadata_is_decoded_and_unknown_values_rejected() {
    for (metadata, expected) in [
        (0, Some(OnuUpstreamRate::Rate1p25G)),
        (1, Some(OnuUpstreamRate::Rate2p5G)),
        (2, None),
        (3, None),
    ] {
        let mut words = [0u32; 13];
        words[0] = (0x123 << 5) | (1 << 4) | metadata;
        let record = PloamRecord::decode(words, true);
        assert_eq!(
            OnuUpstreamRate::from_metadata(record.rate_metadata()),
            expected
        );
    }
}
use crate::platform::fiberhome_an4475_g2::{
    activation::{
        BMP_CFG_RG_REQ, BMP_PLS_REQ, BMP_STA, PORTID_CFG_TAB, RSM_ALM, RSM_CFG_TCONT_CLR,
    },
    downstream::{OMCI_CONTROL, OMCI_MEMORY, PLOAM_DATA as TX_PLOAM_DATA, PLOAM_INFO},
    gdba::{
        read_b_all_set_thd, GDBA_DS_PARA_TAB, GDBA_DS_TAB_REQ_CFG, GDBA_PARA_H_TAB,
        GDBA_PARA_L_TAB, GDBA_TAB_REQ_CFG,
    },
    initialization::{
        defaults::GDBA_SLOT_REQ_CFG, GPON_RAM_INIT_MASK, RAM_INIT_STATUS, SERDES_LOCK_MASK,
        SERDES_STATUS,
    },
    interrupts::{
        read_mask, GPON_INT, GPON_INT_MASK, NNI_INT, NNI_INT_MASK, PLOAM_LOAMI_SUFI_INT,
        PLOAM_RX_INT, PLOAM_RX_INT_MASK, SOFT_RDWR_OMCI_STA,
    },
    upstream::{
        OmciKind, OMCI_CONSUME, OMCI_MARKER, OMCI_MEMORY as RX_OMCI_MEMORY,
        PLOAM_DATA as RX_PLOAM_DATA, PLOAM_LAST, PLOAM_VALID,
    },
};
use std::collections::{BTreeMap, VecDeque};

const SERDES_APB_CTRL: u32 = 0x1103;
const SERDES_APB_RDATA: u32 = 0x1102;
const APB_ACK: u32 = 1 << 17;

struct SimBus {
    regs: BTreeMap<u32, Vec<u32>>,
    mems: BTreeMap<(u32, u32), Vec<u32>>,
    ploam_tx_frames: Vec<[u32; 12]>,
    omci_tx_writes: Vec<(u32, Vec<u32>)>,
    rx_ploams: VecDeque<[u32; 13]>,
}

impl SimBus {
    fn new() -> Self {
        let mut regs = BTreeMap::new();
        regs.insert(SERDES_STATUS, vec![SERDES_LOCK_MASK]);
        regs.insert(RAM_INIT_STATUS, vec![GPON_RAM_INIT_MASK]);
        regs.insert(SERDES_APB_RDATA, vec![0]);
        regs.insert(PLOAM_INFO, vec![0]);
        regs.insert(OMCI_CONTROL, vec![0]);
        regs.insert(RSM_ALM, vec![0]);
        regs.insert(BMP_STA, vec![0]);
        regs.insert(OMCI_MARKER, vec![0xA55A_0001]);
        Self {
            regs,
            mems: BTreeMap::new(),
            ploam_tx_frames: Vec::new(),
            omci_tx_writes: Vec::new(),
            rx_ploams: VecDeque::new(),
        }
    }

    fn push_rx_ploam(&mut self, mut words: [u32; 13]) {
        // Ensure valid/last flags reflect queue depth.
        self.rx_ploams.push_back(words);
        let is_last = u32::from(self.rx_ploams.len() == 1);
        self.regs.insert(PLOAM_VALID, vec![1]);
        self.regs.insert(PLOAM_LAST, vec![is_last]);
        self.regs.insert(PLOAM_RX_INT, vec![1]);
        // Mirror front entry at PLOAM_DATA if this is the first item.
        if let Some(front) = self.rx_ploams.front() {
            words = *front;
            self.regs.insert(RX_PLOAM_DATA, words.to_vec());
        }
    }

    fn push_rx_omci(&mut self, row: u32, source: u8, payload: &[u8]) {
        let marker = self
            .regs
            .get(&OMCI_MARKER)
            .and_then(|v| v.first().copied())
            .unwrap_or(0);
        let metadata = (1u32 << 31) | (u32::from(source) << 14) | (payload.len() as u32 & 0x3fff);
        self.mems
            .insert((RX_OMCI_MEMORY, row), vec![metadata, marker]);
        let mut cur_row = (row + 1) & 0xff;
        for chunk in payload.chunks(8) {
            let mut padded = [0u8; 8];
            padded[..chunk.len()].copy_from_slice(chunk);
            let hi = u32::from_be_bytes(padded[..4].try_into().unwrap());
            let lo = u32::from_be_bytes(padded[4..8].try_into().unwrap());
            self.mems.insert((RX_OMCI_MEMORY, cur_row), vec![lo, hi]);
            cur_row = (cur_row + 1) & 0xff;
        }
        self.regs.insert(SOFT_RDWR_OMCI_STA, vec![1]);
    }

    fn get_reg_slice(&self, addr: u32, count: usize) -> Vec<u32> {
        let mut out = vec![0; count];
        if let Some(v) = self.regs.get(&addr) {
            let n = v.len().min(count);
            out[..n].copy_from_slice(&v[..n]);
        }
        out
    }
}

fn decode_be_words(bytes: &[u8]) -> Vec<u32> {
    let (chunks, rem) = bytes.as_chunks::<4>();
    debug_assert!(rem.is_empty());
    chunks.iter().map(|c| u32::from_be_bytes(*c)).collect()
}

impl Bus for SimBus {
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> io::Result<()> {
        let op = tx[0] & 0x7;
        let addr = (u32::from(tx[0] >> 3) << 8) | u32::from(tx[1]);
        let count = usize::from(tx[2] >> 4) + 1;
        let row = (u32::from(tx[2] & 0x0f) << 8) | u32::from(tx[3]);

        match op {
            1 => {
                // Register write
                let mut words = decode_be_words(&tx[5..]);
                match addr {
                    SERDES_APB_CTRL => words[0] |= APB_ACK,
                    TX_PLOAM_DATA => {
                        let mut frame_words = [0u32; 12];
                        let n = words.len().min(12);
                        frame_words[..n].copy_from_slice(&words[..n]);
                        self.ploam_tx_frames.push(frame_words);
                    }
                    PLOAM_INFO => {
                        // Pulse bit 0 submits and completes immediately (BUSY bit 8 stays 0).
                        words[0] &= !(1 << 8);
                    }
                    OMCI_CONTROL => {
                        words[0] &= !(1 << 1);
                    }
                    GDBA_DS_TAB_REQ_CFG | GDBA_TAB_REQ_CFG | GDBA_SLOT_REQ_CFG
                    | RSM_CFG_TCONT_CLR => {
                        words[0] = 0;
                    }
                    BMP_CFG_RG_REQ => {
                        self.regs.insert(BMP_STA, vec![1 << 3]);
                    }
                    BMP_PLS_REQ => {
                        words[0] = 0;
                    }
                    PLOAM_VALID if words[0] == 0 => {
                        let _ = self.rx_ploams.pop_front();
                        if let Some(next) = self.rx_ploams.front().copied() {
                            self.regs.insert(RX_PLOAM_DATA, next.to_vec());
                            self.regs.insert(PLOAM_VALID, vec![1]);
                            let is_last = u32::from(self.rx_ploams.len() == 1);
                            self.regs.insert(PLOAM_LAST, vec![is_last]);
                            return Ok(());
                        } else {
                            self.regs.insert(PLOAM_VALID, vec![0]);
                            self.regs.insert(PLOAM_LAST, vec![1]);
                            return Ok(());
                        }
                    }
                    OMCI_CONSUME => {
                        self.regs.insert(SOFT_RDWR_OMCI_STA, vec![0]);
                        return Ok(());
                    }
                    GPON_INT | PLOAM_LOAMI_SUFI_INT | PLOAM_RX_INT => {
                        let cur = self.get_reg_slice(addr, 1)[0];
                        words[0] = cur & !words[0];
                    }
                    NNI_INT => {
                        words[0] = 0;
                    }
                    _ => {}
                }
                self.regs.insert(addr, words);
            }
            2 => {
                // Register read
                let words = self.get_reg_slice(addr, count);
                for (i, w) in words.into_iter().enumerate() {
                    rx[i * 4..(i + 1) * 4].copy_from_slice(&w.to_be_bytes());
                }
            }
            5 => {
                // Memory write (reversed words on wire -> reverse back to logical)
                let mut words = decode_be_words(&tx[5..]);
                words.reverse();
                if addr == OMCI_MEMORY {
                    self.omci_tx_writes.push((row, words.clone()));
                }
                self.mems.insert((addr, row), words);
            }
            6 => {
                // Memory read (logical -> reverse onto wire)
                let words = self
                    .mems
                    .get(&(addr, row))
                    .cloned()
                    .unwrap_or_else(|| vec![0; count]);
                for (i, w) in words.iter().take(count).rev().enumerate() {
                    rx[i * 4..(i + 1) * 4].copy_from_slice(&w.to_be_bytes());
                }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected op {other}"),
                ));
            }
        }
        Ok(())
    }
}

fn no_sleep(_: Duration) -> io::Result<()> {
    Ok(())
}

fn encode_rx_ploam_words(frame: &[u8; 13], meta: u32) -> [u32; 13] {
    let mut words = [0u32; 13];
    words[0] = meta;
    words[1] = u32::from_be_bytes(frame[9..13].try_into().unwrap());
    words[2] = u32::from_be_bytes(frame[5..9].try_into().unwrap());
    words[3] = u32::from_be_bytes(frame[1..5].try_into().unwrap());
    words[4] = u32::from(frame[0]);
    words
}

#[test]
fn ploam_conversion_roundtrips_with_pon_engine_frame() {
    let sn = *b"TEST\x11\x22\x33\x44";
    let frame = PloamFrame::assign_onu_id(
        0x05,
        sn[..4].try_into().unwrap(),
        sn[4..].try_into().unwrap(),
    );
    let down = to_downstream_ploam(&frame);
    assert_eq!(down[0], 0xff);
    assert_eq!(down[1], 0x03);
    assert_eq!(down[2], 0x05);
    assert_eq!(&down[3..11], &sn);

    let wire = frame.to_bytes();
    let rec = PloamRecord::decode(encode_rx_ploam_words(&wire, 0), true);
    let parsed = parse_upstream_ploam(&rec).expect("parse upstream PLOAM");
    assert_eq!(parsed.onu_id, frame.onu_id);
    assert_eq!(parsed.message_id, frame.message_id);
    assert_eq!(parsed.payload, frame.payload);
}

#[test]
fn full_offline_olt_lifecycle_init_discover_activate_omci_fec_and_deactivate() {
    let mut cfg = OltSessionConfig::default();
    cfg.init.max_onu_id = 16;

    let mut session = FslOltSession::new(SimBus::new(), cfg);
    session.initialize(&mut no_sleep).expect("initialize");
    assert!(session.is_initialized());
    assert_eq!(session.base_b_all_set_thd(), 38880);
    assert_eq!(session.dba_granularity_8x(), 64);

    let masks = read_mask(&mut session.mgmt.fsl).expect("read_mask");
    assert!(masks.ploam_rx && masks.omci_rx && masks.losi && masks.tiwi);
    assert_eq!(
        session.bus().get_reg_slice(GPON_INT_MASK, 1),
        vec![u32::MAX]
    );
    assert_eq!(session.bus().get_reg_slice(PLOAM_RX_INT_MASK, 1), vec![1]);
    assert_eq!(session.bus().get_reg_slice(NNI_INT_MASK, 1)[0] & 1, 1);

    // Synthesize SN discovery (O3) PLOAMu from unassigned ONU (onu_id = 0xff, msg = 0x01).
    let test_sn = *b"TEST\x00\x00\x00\x01";
    let mut sn_frame = [0u8; 13];
    sn_frame[0] = 0xff;
    sn_frame[1] = 0x01;
    sn_frame[2..10].copy_from_slice(&test_sn);
    session
        .bus_mut()
        .push_rx_ploam(encode_rx_ploam_words(&sn_frame, 0));

    let poll1 = session.poll_once(4, 4, &mut no_sleep).expect("poll O3 SN");
    assert_eq!(poll1.interrupts, vec![GponInterrupt::PloamRx]);
    assert_eq!(poll1.ploams.len(), 1);
    assert_eq!(poll1.ploams[0].onu_id(), None);
    assert_eq!(poll1.ploams[0].message_id(), 0x01);

    // Trigger O3 processing -> assigns ONU-ID 0 + triggers range request.
    let mut delays = Vec::new();
    let onu = session
        .process_o3_sn_discovery(test_sn, OnuUpstreamRate::Rate2p5G, &mut |d| {
            delays.push(d);
            Ok(())
        })
        .expect("O3 process");
    assert_eq!(onu.onu_id, 0);
    assert_eq!(onu.onu_index, 0);
    assert_eq!(onu.state, OnuState::Ranging);
    assert!(delays.contains(&cfg.o3_range_delay));
    // Assign_ONU-ID repeated 3 times.
    assert_eq!(session.bus().ploam_tx_frames.len(), 3);

    // Synthesize ranged SN PLOAMu (O4 -> O5) and activate ONU 0.
    let raw_rt = 0x400u32 << 5;
    let mut ranged_frame = [0u8; 13];
    ranged_frame[0] = onu.onu_id;
    ranged_frame[1] = 0x01;
    ranged_frame[2..10].copy_from_slice(&test_sn);
    session
        .bus_mut()
        .push_rx_ploam(encode_rx_ploam_words(&ranged_frame, raw_rt));

    let poll2 = session.poll_once(4, 4, &mut no_sleep).expect("poll O4 SN");
    assert_eq!(poll2.ploams.len(), 1);
    assert_eq!(poll2.ploams[0].onu_id(), Some(0));

    let other_sn = *b"TEST\x00\x00\x00\x02";
    assert_eq!(
        session
            .process_o4_o5_activation(
                0,
                &other_sn,
                poll2.ploams[0].ranging_time_raw(),
                &mut no_sleep
            )
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(session.bus().ploam_tx_frames.len(), 3);
    assert_eq!(session.onu(0).unwrap().state, OnuState::Ranging);
    // A rejected identity must not consume a sequence; exercise wrap at the
    // successful send and inspect the encoded frame rather than only a counter.
    assert_eq!(session.next_port_id_sequence, 0);
    session.next_port_id_sequence = u16::MAX;
    let activated = session
        .process_o4_o5_activation(
            0,
            &test_sn,
            poll2.ploams[0].ranging_time_raw(),
            &mut no_sleep,
        )
        .expect("O4/O5 activation");
    assert_eq!(activated.state, OnuState::Active);
    assert_eq!(
        activated.eqd,
        eqd_from_ranging(poll2.ploams[0].ranging_time_raw(), true)
    );
    // Each stock O4 message is sent three times, with identical copies.
    let frames = &session.bus().ploam_tx_frames;
    assert_eq!(frames.len(), 9);
    assert_eq!(frames[3], frames[4]);
    assert_eq!(frames[4], frames[5]);
    assert_eq!(frames[6], frames[7]);
    assert_eq!(frames[7], frames[8]);
    assert_ne!(frames[3], frames[6]);
    assert_eq!(
        frames[6],
        super::super::downstream::pack_ploam(
            &configure_port_id_ploam(0, 0, true, u16::MAX).unwrap()
        )
    );
    assert_eq!(session.next_port_id_sequence, 0);
    assert_eq!(session.active_onus().count(), 1);

    // Active GDBA threshold recalculated cleanly below baseline 38880.
    let thd_no_fec = read_b_all_set_thd(&mut session.mgmt.fsl).expect("read thd");
    assert!(thd_no_fec < 38880);
    assert!(
        session.bus().get_reg_slice(BMP_CFG_ONU_ACTIVE, 4)[0] & 1 == 1,
        "ONU 0 active bit must be set"
    );
    assert!(
        session.bus().mems.contains_key(&(GDBA_DS_PARA_TAB, 0))
            && session.bus().mems.contains_key(&(GDBA_PARA_H_TAB, 0))
            && session.bus().mems.contains_key(&(GDBA_PARA_L_TAB, 0))
    );
    assert!(
        session.bus().mems.contains_key(&(PORTID_CFG_TAB, 0)),
        "OMCC GEM port entry must be programmed"
    );

    // Enabling US FEC shrinks B_ALL_SET_THD by ~239/255.
    let thd_with_fec = session
        .set_onu_us_fec_and_recalc(0, true)
        .expect("enable fec")
        .expect("b_all_mode enabled");
    assert_eq!(
        thd_with_fec,
        ((239u32 * u32::from(thd_no_fec)) / 0xff) as u16
    );

    // Send & receive OMCI frame via cursor = 0.
    let tx_omci = vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    session
        .send_omci(0, &tx_omci, &mut no_sleep)
        .expect("send_omci");
    assert!(!session.bus().omci_tx_writes.is_empty());

    let rx_omci_payload = vec![0xaa, 0xbb, 0xcc, 0xdd, 0x11, 0x22, 0x33, 0x44];
    session.bus_mut().push_rx_omci(0, 0, &rx_omci_payload);
    let poll3 = session
        .poll_once(4, 4, &mut no_sleep)
        .expect("poll OMCI RX");
    assert_eq!(poll3.interrupts, vec![GponInterrupt::OmciRx]);
    assert_eq!(poll3.omci_frames.len(), 1);
    assert_eq!(poll3.omci_frames[0].kind, OmciKind::Omci);
    assert_eq!(poll3.omci_frames[0].source, 0);
    assert_eq!(poll3.omci_frames[0].payload, rx_omci_payload);

    // Deactivate ONU -> clears active bitmap, FEC bitmap, GEM port, recalculates B_ALL_SET_THD back to 38880.
    session
        .deactivate_onu(0, &[], &[], &mut no_sleep)
        .expect("deactivate_onu");
    assert!(session.onu(0).is_none());
    assert_eq!(session.bus().get_reg_slice(BMP_CFG_ONU_ACTIVE, 4)[0] & 1, 0);
    assert_eq!(
        read_b_all_set_thd(&mut session.mgmt.fsl).expect("restored thd"),
        38880
    );
    // 3 Deactivate_ONU-ID PLOAMs added -> 12 total.
    assert_eq!(session.bus().ploam_tx_frames.len(), 12);
}
