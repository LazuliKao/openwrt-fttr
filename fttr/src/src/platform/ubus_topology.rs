use super::*;
use crate::controller::topology::{self, Snapshot};
use std::sync::Arc;

impl Handlers {
    fn topology_snapshot(&self, id: Option<&str>) -> Result<Option<Arc<Snapshot>>, i32> {
        if let Some(id) = id {
            if id.is_empty() || id.len() > 96 {
                return Err(2);
            }
            return Ok(self.topology.lock().map_err(|_| 9)?.get(id));
        }
        // get_status releases all engine/device locks before querying external services.
        let status = self.get_status(EmptyRequest {})?;
        let snapshot = topology::build(status, &self.engine, &self.services);
        // Reject mixed registrations if optical identity changed during local collection.
        {
            let engine = self.engine.lock().map_err(|_| 9)?;
            let services = self.services.lock().map_err(|_| 9)?;
            let observed: Vec<_> = snapshot
                .reply
                .nodes
                .iter()
                .filter_map(|node| match &node.detail {
                    TopologyNodeDetail::Onu(onu) if onu.onu_id.is_some() => Some(onu),
                    _ => None,
                })
                .collect();
            if observed.len() != engine.active_onus.len() {
                return Err(9);
            }
            for onu in observed {
                let id = onu.onu_id.unwrap();
                if !engine.active_onus.iter().any(|current| {
                    let sn = current.full_sn_str().trim().to_ascii_uppercase();
                    current.onu_id == id
                        && Some(current.state) == onu.registration
                        && onu.serial_number.as_deref() == (!sn.is_empty()).then_some(sn.as_str())
                }) {
                    return Err(9);
                }
                if let Some(business) = &onu.business {
                    if services.get(&id).map(|s| s.generation).unwrap_or_default()
                        != business.generation
                    {
                        return Err(9);
                    }
                }
            }
        }
        Ok(Some(self.topology.lock().map_err(|_| 9)?.insert(snapshot)))
    }

    pub(super) fn get_topology(&self, _: EmptyRequest) -> Result<TopologyReply, i32> {
        Ok(self.topology_snapshot(None)?.ok_or(9)?.reply.clone())
    }

    pub(super) fn get_topology_node(
        &self,
        request: TopologyNodeRequest,
    ) -> Result<TopologyNodeReply, i32> {
        validate_node(&request.node_id)?;
        Ok(
            match self.topology_snapshot(request.snapshot_id.as_deref())? {
                Some(snapshot) => snapshot.node(&request.node_id),
                None => TopologyNodeReply::Error {
                    code: TopologyQueryError::SnapshotExpired,
                },
            },
        )
    }

    pub(super) fn get_topology_clients(
        &self,
        request: TopologyClientsRequest,
    ) -> Result<TopologyClientsReply, i32> {
        validate_node(&request.node_id)?;
        if request.limit.is_some_and(|limit| limit == 0 || limit > 256)
            || request.cursor.as_ref().is_some_and(|s| s.len() > 32)
        {
            return Err(2);
        }
        if request.cursor.is_some() && request.snapshot_id.is_none() {
            return Ok(TopologyClientsReply::Error {
                code: TopologyQueryError::InvalidCursor,
            });
        }
        Ok(
            match self.topology_snapshot(request.snapshot_id.as_deref())? {
                Some(snapshot) => snapshot.clients(&request),
                None => TopologyClientsReply::Error {
                    code: TopologyQueryError::SnapshotExpired,
                },
            },
        )
    }
}

fn validate_node(id: &str) -> Result<(), i32> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        Err(2)
    } else {
        Ok(())
    }
}
