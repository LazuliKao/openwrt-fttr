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
        read_b_all_set_thd, GDBA_DS_ACTIVE_TAB, GDBA_DS_PARA_TAB, GDBA_DS_PENDING_TAB,
        GDBA_DS_TAB_REQ_CFG, GDBA_PARA_H_TAB, GDBA_PARA_L_TAB, GDBA_TAB_REQ_CFG,
    },
    initialization::{
        defaults::{DEBUG_PIN_SEL, GDBA_SLOT_REQ_CFG},
        GPON_RAM_INIT_MASK, RAM_INIT_STATUS, SERDES_LOCK_MASK, SERDES_STATUS,
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
    portid_tab_writes: Vec<(u32, Vec<u32>)>,
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
            portid_tab_writes: Vec::new(),
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
                } else if addr == PORTID_CFG_TAB {
                    self.portid_tab_writes.push((row, words.clone()));
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

fn build_configure_port_id_ack(
    onu_id: u8,
    gem_port_id: u16,
    enable: bool,
    sequence: u16,
) -> [u8; 13] {
    let down = configure_port_id_ploam(onu_id, gem_port_id, enable, sequence).unwrap();
    let mut frame = [0u8; 13];
    frame[0] = onu_id;
    frame[1] = PLOAM_MSG_ACK;
    frame[2] = PLOAM_MSG_CONFIGURE_PORT_ID;
    frame[3..12].copy_from_slice(&down[..9]);
    frame
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
    assert_eq!(
        session.bus().get_reg_slice(DEBUG_PIN_SEL, 1),
        vec![0x8c],
        "stock rssi_ad_ctrl(0xff, 0xff, 1) must program DEBUG_PIN_SEL=0x8c"
    );

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

    // Synthesize ranged SN PLOAMu (O4) and transition ONU 0 to AwaitingPortIdAck.
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
    let portid_writes_before_o4 = session.bus().portid_tab_writes.len();
    let pending = session
        .process_o4_o5_activation(
            0,
            &test_sn,
            poll2.ploams[0].ranging_time_raw(),
            &mut no_sleep,
        )
        .expect("O4 activation");
    assert_eq!(pending.state, OnuState::AwaitingPortIdAck);
    assert_eq!(
        pending.eqd,
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
    assert_eq!(session.active_onus().count(), 0);
    assert_eq!(session.pending_port_id_acks().len(), 1);
    assert_eq!(session.pending_port_id_acks()[0].sequence, u16::MAX);

    // Before Configure_Port_ID ACK, PORTID_CFG_TAB must not be updated and send_omci must fail.
    assert_eq!(
        session.bus().portid_tab_writes.len(),
        portid_writes_before_o4,
        "OMCC GEM port must not be programmed before Configure_Port_ID ACK"
    );
    assert_eq!(
        session
            .send_omci(0, &[0; 8], &mut no_sleep)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotConnected
    );

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
            && session.bus().mems.get(&(GDBA_DS_ACTIVE_TAB, 0)) == Some(&vec![0x100, 0])
            && session.bus().mems.get(&(GDBA_DS_PENDING_TAB, 0)) == Some(&vec![0x100, 0]),
        "OMCC T-CONT must be programmed into both GDBA Active and Pending tables"
    );

    // Wrong sequence or disable ACK must be ignored without promoting to Active.
    let wrong_seq_ack = PloamRecord::decode(
        encode_rx_ploam_words(&build_configure_port_id_ack(0, 0, true, 0x1234), 0),
        true,
    );
    assert_eq!(
        session
            .process_ploam_ack(&wrong_seq_ack, &mut no_sleep)
            .unwrap(),
        None
    );
    let disable_ack = PloamRecord::decode(
        encode_rx_ploam_words(&build_configure_port_id_ack(0, 0, false, u16::MAX), 0),
        true,
    );
    assert_eq!(
        session
            .process_ploam_ack(&disable_ack, &mut no_sleep)
            .unwrap(),
        None
    );
    assert_eq!(session.onu(0).unwrap().state, OnuState::AwaitingPortIdAck);
    assert_eq!(session.pending_port_id_acks().len(), 1);

    // Matching Configure_Port_ID ACK transitions ONU to Active, programs PORTID_CFG_TAB,
    // and transmits BER_Interval (28_800_000) 3 times.
    let matching_ack_frame = build_configure_port_id_ack(0, 0, true, u16::MAX);
    session
        .bus_mut()
        .push_rx_ploam(encode_rx_ploam_words(&matching_ack_frame, 0));
    let poll_ack = session.poll_once(4, 4, &mut no_sleep).expect("poll ACK");
    assert_eq!(poll_ack.ploams.len(), 1);
    let activated = session
        .process_ploam_ack(&poll_ack.ploams[0], &mut no_sleep)
        .expect("process ACK")
        .expect("ONU must transition to Active");
    assert_eq!(activated.state, OnuState::Active);
    assert_eq!(session.active_onus().count(), 1);
    assert!(session.pending_port_id_acks().is_empty());
    assert_eq!(
        session.bus().portid_tab_writes.len(),
        portid_writes_before_o4 + 1,
        "OMCC GEM port entry must be programmed after Configure_Port_ID ACK"
    );
    assert_eq!(session.bus().portid_tab_writes.last().unwrap().0, 0);
    assert_eq!(session.bus().ploam_tx_frames.len(), 12);
    assert_eq!(
        session.bus().ploam_tx_frames[9],
        super::super::downstream::pack_ploam(&ber_interval_ploam(0, DEFAULT_BER_INTERVAL))
    );
    // Duplicate ACK is idempotent and ignored.
    assert_eq!(
        session
            .process_ploam_ack(&poll_ack.ploams[0], &mut no_sleep)
            .unwrap(),
        None
    );
    assert_eq!(session.bus().ploam_tx_frames.len(), 12);

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
    // 3 Deactivate_ONU-ID PLOAMs added -> 15 total.
    assert_eq!(session.bus().ploam_tx_frames.len(), 15);
}

#[test]
fn ranging_and_port_id_ack_timeouts_reregister_and_expire_ack_entries() {
    let mut cfg = OltSessionConfig::default();
    cfg.init.max_onu_id = 16;
    let mut session = FslOltSession::new(SimBus::new(), cfg);
    session.initialize(&mut no_sleep).expect("initialize");

    let t0 = Instant::now();
    let sn_a = *b"TEST\x00\x00\x00\x11";
    let sn_b = *b"TEST\x00\x00\x00\x22";

    // ONU A enters Ranging at t0 and times out at t0 + 5s.
    let onu_a = session
        .process_o3_sn_discovery_at(sn_a, OnuUpstreamRate::Rate1p25G, t0, &mut no_sleep)
        .expect("O3 A");
    let before_timeout = session
        .check_timeouts_at(t0 + Duration::from_secs(4), &mut no_sleep)
        .expect("sweep 4s");
    assert_eq!(before_timeout, TimeoutReport::default());
    assert!(session.onu(onu_a.onu_id).is_some());

    let after_o3_timeout = session
        .check_timeouts_at(t0 + Duration::from_secs(5), &mut no_sleep)
        .expect("sweep 5s");
    assert_eq!(after_o3_timeout.timed_out_ranging, vec![onu_a.onu_id]);
    assert!(after_o3_timeout.timed_out_port_id_ack.is_empty());
    assert!(session.onu(onu_a.onu_id).is_none());

    // ONU B enters O3 at t1, O4 at t1 + 1s (AwaitingPortIdAck), and times out at t1 + 6s.
    let t1 = t0 + Duration::from_secs(10);
    let onu_b = session
        .process_o3_sn_discovery_at(sn_b, OnuUpstreamRate::Rate2p5G, t1, &mut no_sleep)
        .expect("O3 B");
    session
        .process_o4_o5_activation_at(
            onu_b.onu_id,
            &sn_b,
            0x200,
            t1 + Duration::from_secs(1),
            &mut no_sleep,
        )
        .expect("O4 B");
    assert_eq!(session.pending_port_id_acks().len(), 1);
    assert_eq!(
        session.onu(onu_b.onu_id).unwrap().state,
        OnuState::AwaitingPortIdAck
    );

    let after_o4_timeout = session
        .check_timeouts_at(t1 + Duration::from_secs(6), &mut no_sleep)
        .expect("sweep O4 timeout");
    assert_eq!(after_o4_timeout.expired_acks, 1);
    assert_eq!(after_o4_timeout.timed_out_port_id_ack, vec![onu_b.onu_id]);
    assert!(session.pending_port_id_acks().is_empty());
    assert!(session.onu(onu_b.onu_id).is_none());
    assert_eq!(session.bus().get_reg_slice(BMP_CFG_ONU_ACTIVE, 4)[0] & 1, 0);
    assert_eq!(
        read_b_all_set_thd(&mut session.mgmt.fsl).expect("restored thd"),
        38880
    );

    // Timed-out ONU can cleanly re-discover after reregister cleanup.
    let rediscovered = session
        .process_o3_sn_discovery_at(
            sn_b,
            OnuUpstreamRate::Rate2p5G,
            t1 + Duration::from_secs(7),
            &mut no_sleep,
        )
        .expect("rediscover B");
    assert_eq!(rediscovered.state, OnuState::Ranging);
}

#[test]
fn blacklist_and_strict_whitelist_gate_discovery() {
    let mut cfg = OltSessionConfig::default();
    cfg.init.max_onu_id = 16;
    let mut session = FslOltSession::new(SimBus::new(), cfg);
    session.initialize(&mut no_sleep).expect("initialize");

    let sn = *b"TEST\x00\x00\x00\x33";
    // Blacklist is keyed on (SN, rate), matching stock fslstack_black_list_find_entry.
    session.add_to_blacklist(sn, OnuUpstreamRate::Rate1p25G);
    assert_eq!(
        session
            .process_o3_sn_discovery(sn, OnuUpstreamRate::Rate1p25G, &mut no_sleep)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(session.bus().ploam_tx_frames.is_empty());

    // Strict whitelist mode rejects non-whitelisted (SN, rate) until explicitly added.
    session.set_auto_whitelist(false);
    assert_eq!(
        session
            .process_o3_sn_discovery(sn, OnuUpstreamRate::Rate2p5G, &mut no_sleep)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    session.add_to_whitelist(sn, OnuUpstreamRate::Rate2p5G);
    let onu = session
        .process_o3_sn_discovery(sn, OnuUpstreamRate::Rate2p5G, &mut no_sleep)
        .expect("whitelisted 2.5G ONU allowed");
    assert_eq!(onu.state, OnuState::Ranging);
}

#[test]
fn alarm_interrupts_respect_stock_losi_lofi_and_dying_gasp_thresholds() {
    let mut cfg = OltSessionConfig::default();
    cfg.init.max_onu_id = 16;
    let mut session = FslOltSession::new(SimBus::new(), cfg);
    session.initialize(&mut no_sleep).expect("initialize");

    let sn = *b"TEST\x00\x00\x00\x44";
    let onu = session
        .process_o3_sn_discovery(sn, OnuUpstreamRate::Rate1p25G, &mut no_sleep)
        .expect("O3");
    session
        .process_o4_o5_activation(onu.onu_id, &sn, 0x100, &mut no_sleep)
        .expect("O4");
    let ack = PloamRecord::decode(
        encode_rx_ploam_words(&build_configure_port_id_ack(onu.onu_id, 0, true, 0), 0),
        true,
    );
    session
        .process_ploam_ack(&ack, &mut no_sleep)
        .expect("ACK")
        .expect("Active");
    assert_eq!(session.find_onu_by_sn(&sn).map(|o| o.onu_id), Some(0));

    // Stock LOSi threshold is >5 hits before deactivating.
    session
        .bus_mut()
        .regs
        .insert(GPON_LOSI_ALM, vec![1 << onu.onu_index, 0, 0, 0]);
    for _ in 0..5 {
        assert!(session
            .evaluate_alarm_interrupts(&[GponInterrupt::Losi])
            .unwrap()
            .is_empty());
    }
    assert_eq!(
        session
            .evaluate_alarm_interrupts(&[GponInterrupt::Losi])
            .unwrap(),
        vec![(0, "losi")]
    );

    // Stock LOFi threshold is >2 hits before deactivating.
    session
        .bus_mut()
        .regs
        .insert(GPON_LOFI_ALM, vec![1 << onu.onu_index, 0, 0, 0]);
    for _ in 0..2 {
        assert!(session
            .evaluate_alarm_interrupts(&[GponInterrupt::Lofi])
            .unwrap()
            .is_empty());
    }
    assert_eq!(
        session
            .evaluate_alarm_interrupts(&[GponInterrupt::Lofi])
            .unwrap(),
        vec![(0, "lofi")]
    );

    // DyingGasp triggers immediately on the active ONU bit.
    session
        .bus_mut()
        .regs
        .insert(GPON_DYING_GASP_ALM, vec![1 << onu.onu_index, 0, 0, 0]);
    assert_eq!(
        session
            .evaluate_alarm_interrupts(&[GponInterrupt::DyingGasp])
            .unwrap(),
        vec![(0, "dying_gasp")]
    );
}
