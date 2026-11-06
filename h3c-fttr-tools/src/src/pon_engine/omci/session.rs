use super::{
    attributes::{AttributeBlock, AttributeCodec, AttributeContext, AttributeValue},
    error::invalid,
    messages::{check_result, Action, GetResponse, RequestBody},
    mib::{MibAssembler, MibRecord, MibReport},
    schema::MeRegistry,
    tables::TableAssembler,
    wire::BaselineFrame,
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Project transport boundary; returns only validated, ONU-isolated protocol frames.
pub trait OmciTransport: Send {
    fn send(&mut self, frame: &BaselineFrame) -> io::Result<()>;
    fn receive(&mut self) -> io::Result<Option<BaselineFrame>>;
}

/// OMCI-001/002/003/008 message semantics; retries/guards are project orchestration.
/// No socket, FPGA register or device topology is owned by this object.
pub struct OmciSession {
    transport: Box<dyn OmciTransport>,
    next_tci: u16,
    registration: Option<Box<dyn Fn() -> bool + Send>>,
    cancellation: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
}
impl OmciSession {
    pub fn new(transport: impl OmciTransport + 'static) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u16;
        Self {
            transport: Box::new(transport),
            next_tci: (seed & 0x7fff).max(1),
            registration: None,
            cancellation: None,
            deadline: None,
        }
    }
    pub fn guard_registration(&mut self, guard: impl Fn() -> bool + Send + 'static) {
        self.registration = Some(Box::new(guard));
    }
    pub fn cancel_when(&mut self, token: Arc<AtomicBool>) {
        self.cancellation = Some(token);
    }
    pub fn finish_before(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }
    /// Bound an optional operation without extending or poisoning the caller's deadline.
    pub fn with_timeout<T>(
        &mut self,
        timeout: Duration,
        operation: impl FnOnce(&mut Self) -> io::Result<T>,
    ) -> io::Result<T> {
        let previous = self.deadline;
        let deadline = Instant::now() + timeout;
        self.deadline = Some(previous.map_or(deadline, |outer| outer.min(deadline)));
        let result = operation(self);
        self.deadline = previous;
        result
    }
    fn check_active(&self) -> io::Result<()> {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OMCI diagnostic deadline exceeded",
            ));
        }
        if self
            .cancellation
            .as_ref()
            .is_some_and(|t| t.load(Ordering::Acquire))
            || self.registration.as_ref().is_some_and(|g| !g())
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "ONU registration generation no longer active",
            ));
        }
        Ok(())
    }
    /// Raw vendor requests remain explicit at the device adapter boundary.
    pub fn transact(
        &mut self,
        message_type: u8,
        class_id: u16,
        entity_id: u16,
        payload: [u8; 32],
    ) -> io::Result<BaselineFrame> {
        let request = BaselineFrame::new(self.next_tci, message_type, class_id, entity_id, payload);
        self.next_tci = (self.next_tci.wrapping_add(1) & 0x7fff).max(1);
        for _ in 0..3 {
            self.check_active()?;
            self.transport.send(&request)?;
            // Building a MIB snapshot can take much longer than an ordinary
            // attribute operation. Keep the same TCI while awaiting that reply;
            // rapidly starting another upload can restart the ONU's snapshot.
            let timeout = if message_type == Action::MibUpload.request_type() {
                Duration::from_secs(20)
            } else {
                Duration::from_secs(2)
            };
            let end = Instant::now() + timeout;
            while Instant::now() < end {
                self.check_active()?;
                match self.transport.receive()? {
                    Some(frame) if frame.answers(&request) => {
                        std::thread::sleep(Duration::from_millis(25));
                        return Ok(frame);
                    }
                    Some(_) => std::thread::sleep(Duration::from_millis(2)),
                    None => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "OMCI TCI={:04x} class={class_id} entity={entity_id:04x}",
                request.transaction_id
            ),
        ))
    }
    fn request(
        &mut self,
        action: Action,
        class_id: u16,
        entity_id: u16,
        body: RequestBody<'_>,
    ) -> io::Result<BaselineFrame> {
        self.transact(action.request_type(), class_id, entity_id, body.encode()?)
    }
    pub fn get(&mut self, class_id: u16, entity_id: u16, mask: u16) -> io::Result<BaselineFrame> {
        let frame = self.request(Action::Get, class_id, entity_id, RequestBody::Get { mask })?;
        GetResponse::decode(&frame, mask)?;
        Ok(frame)
    }
    pub fn get_attributes(
        &mut self,
        class_id: u16,
        entity_id: u16,
        mask: u16,
    ) -> io::Result<AttributeBlock> {
        let def = MeRegistry::get(class_id)
            .filter(|d| d.layout_supported)
            .ok_or_else(|| invalid("no complete ME layout"))?;
        let frame = self.get(class_id, entity_id, mask)?;
        let body = GetResponse::decode(&frame, mask)?;
        let block =
            AttributeCodec::decode(def, body.mask, body.data, AttributeContext::GetResponse);
        if block.undecoded_mask != 0 {
            return Err(invalid("GET attribute block cannot be fully decoded"));
        }
        Ok(block)
    }
    pub fn set_attributes(
        &mut self,
        class_id: u16,
        entity_id: u16,
        values: &[(u8, AttributeValue)],
    ) -> io::Result<()> {
        let def = MeRegistry::get(class_id)
            .filter(|d| d.layout_supported)
            .ok_or_else(|| invalid("no complete ME layout"))?;
        let (mask, data) = AttributeCodec::encode(def, values, AttributeContext::SetRequest)?;
        self.set(class_id, entity_id, mask, &data)
    }
    pub fn create_attributes(
        &mut self,
        class_id: u16,
        entity_id: u16,
        values: &[(u8, AttributeValue)],
    ) -> io::Result<()> {
        let def = MeRegistry::get(class_id)
            .filter(|d| d.layout_supported)
            .ok_or_else(|| invalid("no complete ME layout"))?;
        let (_, data) = AttributeCodec::encode(def, values, AttributeContext::CreateRequest)?;
        self.create(class_id, entity_id, &data)
    }
    pub fn read_attributes(
        &mut self,
        class_id: u16,
        entity_id: u16,
        indices: &[u8],
    ) -> io::Result<AttributeBlock> {
        let def = MeRegistry::get(class_id)
            .filter(|d| d.layout_supported && d.actions.contains(&9))
            .ok_or_else(|| invalid("no complete readable ME layout"))?;
        self.get_attributes(
            class_id,
            entity_id,
            AttributeCodec::read_mask(def, indices)?,
        )
    }
    /// Compare exact encoded octets, including fixed-width string padding. Never log values.
    pub fn verify_attributes(
        &mut self,
        class_id: u16,
        entity_id: u16,
        values: &[(u8, AttributeValue)],
    ) -> io::Result<()> {
        let def = MeRegistry::get(class_id)
            .filter(|d| d.layout_supported)
            .ok_or_else(|| invalid("no complete ME layout"))?;
        let indices: Vec<_> = values.iter().map(|(index, _)| *index).collect();
        AttributeCodec::read_mask(def, &indices)?;
        let expected: Vec<_> = values
            .iter()
            .map(|(index, value)| {
                AttributeCodec::encode_value(def.attribute(*index).unwrap(), value)
                    .map(|bytes| (*index, super::wire::hex(&bytes)))
            })
            .collect::<io::Result<_>>()?;
        let block = self.read_attributes(class_id, entity_id, &indices)?;
        for (index, expected) in expected {
            let attr = def.attribute(index).unwrap();
            if !block
                .attributes
                .iter()
                .any(|a| a.definition.index == index && a.raw_hex == expected)
            {
                return Err(invalid(format!(
                    "readback mismatch class={class_id} instance={entity_id:04x} attribute={}",
                    attr.name
                )));
            }
        }
        Ok(())
    }
    pub fn set(&mut self, class_id: u16, entity_id: u16, mask: u16, data: &[u8]) -> io::Result<()> {
        let frame = self.request(
            Action::Set,
            class_id,
            entity_id,
            RequestBody::Set { mask, data },
        )?;
        check_result(&frame)
    }
    pub fn create(&mut self, class_id: u16, entity_id: u16, data: &[u8]) -> io::Result<()> {
        let frame = self.request(
            Action::Create,
            class_id,
            entity_id,
            RequestBody::Create { data },
        )?;
        check_result(&frame)
    }
    pub fn delete(&mut self, class_id: u16, entity_id: u16) -> io::Result<()> {
        let frame = self.request(Action::Delete, class_id, entity_id, RequestBody::Empty)?;
        check_result(&frame)
    }
    pub fn get_table(&mut self, class_id: u16, entity_id: u16, mask: u16) -> io::Result<Vec<u8>> {
        if mask.count_ones() != 1 {
            return Err(invalid("GET NEXT requires one table attribute"));
        }
        let frame = self.get(class_id, entity_id, mask)?;
        let length = u32::from_be_bytes(frame.payload[3..7].try_into().unwrap()) as usize;
        let mut table = TableAssembler::new(length)?;
        for sequence in 0..table.segments() as u16 {
            let frame = self.request(
                Action::GetNext,
                class_id,
                entity_id,
                RequestBody::GetNext { mask, sequence },
            )?;
            let body = GetResponse::decode(&frame, mask)?;
            table.push(sequence, body.data)?;
        }
        table.finish()
    }
    pub fn upload_mib(&mut self) -> io::Result<MibReport> {
        self.upload_mib_with_progress(|_, _| Ok(()))
    }
    pub fn upload_mib_with_progress(
        &mut self,
        mut progress: impl FnMut(usize, usize) -> io::Result<()>,
    ) -> io::Result<MibReport> {
        let frame = self.request(Action::MibUpload, 2, 0, RequestBody::Empty)?;
        let count = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
        if count == 0 || count > 2048 {
            return Err(invalid(format!(
                "invalid/beyond supported MIB upload count: {count} (raw {:02x}{:02x})",
                frame.payload[0], frame.payload[1]
            )));
        }
        let mut mib = MibAssembler::new(count);
        progress(0, usize::from(count))?;
        for sequence in 0..count {
            let frame = self.request(
                Action::MibUploadNext,
                2,
                0,
                RequestBody::UploadNext { sequence },
            )?;
            mib.push(MibRecord::decode(sequence, &frame.payload)?);
            progress(usize::from(sequence) + 1, usize::from(count))?;
        }
        Ok(mib.finish())
    }
}
