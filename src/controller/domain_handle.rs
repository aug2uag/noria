use std::{self, cell, io};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use mio;
use slog::Logger;

use channel::{tcp, TcpReceiver, TcpSender};
use channel::poll::{KeepPolling, PollEvent, PollingLoop, StopPolling};
use consensus::Epoch;
use dataflow::{self, DomainBuilder, DomainConfig, PersistenceParameters};
use dataflow::payload::ControlReplyPacket;
use dataflow::prelude::*;
use dataflow::statistics::{DomainStats, NodeStats};

use coordination::{CoordinationMessage, CoordinationPayload};
use controller::{WorkerEndpoint, WorkerIdentifier};

#[derive(Debug)]
pub enum WaitError {
    WrongReply(ControlReplyPacket),
}

pub struct DomainInputHandle {
    txs: Vec<TcpSender<Box<Packet>>>,
}

pub(crate) struct BatchSendHandle<'a> {
    dih: &'a mut DomainInputHandle,
    sent: Vec<usize>,
}

impl<'a> BatchSendHandle<'a> {
    pub(crate) fn new(dih: &'a mut DomainInputHandle) -> Self {
        let sent = vec![0; dih.txs.len()];
        Self { dih, sent }
    }

    pub(crate) fn enqueue(
        &mut self,
        mut p: Box<Packet>,
        key: &[usize],
        local: bool,
    ) -> Result<(), tcp::SendError> {
        if self.dih.txs.len() == 1 {
            if local {
                p = p.make_local();
            }
            self.dih.txs[0].send(p)?;
            self.sent[0] += 1;
        } else {
            if key.is_empty() {
                unreachable!("sharded base without a key?");
            }
            if key.len() != 1 {
                // base sharded by complex key
                unimplemented!();
            }
            let key_col = key[0];

            let mut shard_writes = vec![Vec::new(); self.dih.txs.len()];
            let mut data = p.take_data();
            for r in data.drain(..) {
                let shard = {
                    let key = match r {
                        Record::Positive(ref r) | Record::Negative(ref r) => &r[key_col],
                        Record::DeleteRequest(ref k) => &k[0],
                    };
                    dataflow::shard_by(key, self.dih.txs.len())
                };
                shard_writes[shard].push(r);
            }

            for (s, rs) in shard_writes.drain(..).enumerate() {
                if !rs.is_empty() {
                    let mut p = Box::new(p.clone_data()); // ok here, as data previously emptied
                    p.swap_data(rs.into());

                    if local {
                        p = p.make_local();
                    }

                    self.dih.txs[s].send(p)?;
                    self.sent[s] += 1;
                }
            }
        }

        Ok(())
    }

    pub(crate) fn wait(self) -> Result<i64, ()> {
        let mut id = Ok(0);
        for (shard, n) in self.sent.into_iter().enumerate() {
            for _ in 0..n {
                use bincode;
                let res: Result<Result<i64, ()>, _>;
                res = bincode::deserialize_from(&mut (&mut self.dih.txs[shard]).reader());
                id = res.unwrap();
            }
        }

        // XXX: this just returns the last id :/
        id
    }
}

impl DomainInputHandle {
    pub(crate) fn new(txs: Vec<SocketAddr>) -> Result<Self, io::Error> {
        let txs: Result<Vec<_>, _> = txs.iter().map(|addr| TcpSender::connect(addr)).collect();

        Ok(Self { txs: txs? })
    }

    pub(crate) fn sender(&mut self) -> BatchSendHandle {
        BatchSendHandle::new(self)
    }

    pub(crate) fn base_send(
        &mut self,
        p: Box<Packet>,
        key: &[usize],
        local: bool,
    ) -> Result<i64, tcp::SendError> {
        let mut s = BatchSendHandle::new(self);
        s.enqueue(p, key, local)?;
        s.wait().map_err(|_| {
            tcp::SendError::IoError(io::Error::new(io::ErrorKind::Other, "write failed"))
        })
    }
}

pub struct DomainHandle {
    _idx: DomainIndex,

    cr_poll: PollingLoop<ControlReplyPacket>,
    txs: Vec<(TcpSender<Box<Packet>>, bool)>,

    // Which worker each shard is assigned to, if any.
    assignments: Vec<WorkerIdentifier>,
}

impl DomainHandle {
    pub fn new<'a>(
        idx: DomainIndex,
        num_shards: usize,
        log: &Logger,
        graph: &mut Graph,
        config: &DomainConfig,
        nodes: Vec<(NodeIndex, bool)>,
        persistence_params: &PersistenceParameters,
        listen_addr: &IpAddr,
        channel_coordinator: &Arc<ChannelCoordinator>,
        debug_addr: &Option<SocketAddr>,
        placer: &'a mut Box<Iterator<Item = (WorkerIdentifier, WorkerEndpoint)>>,
        workers: &'a mut Vec<WorkerEndpoint>,
        epoch: Epoch,
        ts: i64,
    ) -> Self {
        // NOTE: warning to future self...
        // the code currently relies on the fact that the domains that are sharded by the same key
        // *also* have the same number of shards. if this no longer holds, we actually need to do a
        // shuffle, otherwise writes will end up on the wrong shard. keep that in mind.

        let mut txs = Vec::new();
        let mut cr_rxs = Vec::new();
        let mut assignments = Vec::new();
        let mut nodes = Some(Self::build_descriptors(graph, nodes));

        for i in 0..num_shards {
            let nodes = if i == num_shards - 1 {
                nodes.take().unwrap()
            } else {
                nodes.clone().unwrap()
            };

            let control_listener =
                std::net::TcpListener::bind(SocketAddr::new(listen_addr.clone(), 0)).unwrap();
            let domain = DomainBuilder {
                index: idx,
                shard: i,
                nshards: num_shards,
                config: config.clone(),
                nodes,
                persistence_parameters: persistence_params.clone(),
                ts,
                control_addr: control_listener.local_addr().unwrap(),
                debug_addr: debug_addr.clone(),
            };

            // TODO(malte): simple round-robin placement for the moment
            let (identifier, endpoint) = placer.next().unwrap();

            // send domain to worker
            let mut w = endpoint.lock().unwrap();
            debug!(
                log,
                "sending domain {}.{} to worker {:?}",
                domain.index.index(),
                domain.shard,
                w.peer_addr()
            );
            let src = w.local_addr().unwrap();
            w.send(CoordinationMessage {
                epoch,
                source: src,
                payload: CoordinationPayload::AssignDomain(domain),
            }).unwrap();

            assignments.push(identifier);

            let stream =
                mio::net::TcpStream::from_stream(control_listener.accept().unwrap().0).unwrap();
            cr_rxs.push(TcpReceiver::new(stream));
        }

        let mut cr_poll = PollingLoop::from_receivers(cr_rxs);
        cr_poll.run_polling_loop(|event| match event {
            PollEvent::ResumePolling(_) => KeepPolling,
            PollEvent::Process(ControlReplyPacket::Booted(shard, addr)) => {
                channel_coordinator.insert_addr((idx, shard), addr.clone(), false);
                txs.push(channel_coordinator.get_tx(&(idx, shard)).unwrap());

                // TODO(malte): this is a hack, and not an especially neat one. In response to a
                // domain boot message, we broadcast information about this new domain to all
                // workers, which inform their ChannelCoordinators about it. This is required so
                // that domains can find each other when starting up.
                // Moreover, it is required for us to do this *here*, since this code runs on
                // the thread that initiated the migration, and which will query domains to ask
                // if they're ready. No domain will be ready until it has found its neighbours,
                // so by sending out the information here, we ensure that we cannot deadlock
                // with the migration waiting for a domain to become ready when trying to send
                // the information. (We used to do this in the controller thread, with the
                // result of a nasty deadlock.)
                for endpoint in workers.iter() {
                    let mut s = endpoint.lock().unwrap();
                    let msg = CoordinationMessage {
                        epoch,
                        source: s.local_addr().unwrap(),
                        payload: CoordinationPayload::DomainBooted((idx, shard), addr),
                    };

                    s.send(msg).unwrap();
                }

                if txs.len() == num_shards {
                    StopPolling
                } else {
                    KeepPolling
                }
            }
            PollEvent::Process(_) => unreachable!(),
            PollEvent::Timeout => unreachable!(),
        });

        DomainHandle {
            _idx: idx,
            cr_poll,
            txs,
            assignments,
        }
    }

    pub fn shards(&self) -> usize {
        self.txs.len()
    }

    pub fn assignment(&self, shard: usize) -> WorkerIdentifier {
        self.assignments[shard].clone()
    }

    fn build_descriptors(graph: &mut Graph, nodes: Vec<(NodeIndex, bool)>) -> DomainNodes {
        nodes
            .into_iter()
            .map(|(ni, _)| {
                let node = graph.node_weight_mut(ni).unwrap().take();
                node.finalize(graph)
            })
            .map(|nd| (*nd.local_addr(), cell::RefCell::new(nd)))
            .collect()
    }

    pub fn send(&mut self, p: Box<Packet>) -> Result<(), tcp::SendError> {
        for &mut (ref mut tx, is_local) in self.txs.iter_mut() {
            if is_local {
                // TODO: avoid clone on last iteration.
                tx.send(p.clone().make_local())?;
            } else {
                tx.send_ref(&p)?;
            }
        }
        Ok(())
    }

    pub fn send_to_shard(&mut self, i: usize, mut p: Box<Packet>) -> Result<(), tcp::SendError> {
        if self.txs[i].1 {
            p = p.make_local();
        }
        self.txs[i].0.send(p)
    }

    fn wait_for_next_reply(&mut self) -> ControlReplyPacket {
        let mut reply = None;
        self.cr_poll.run_polling_loop(|event| match event {
            PollEvent::Process(packet) => {
                reply = Some(packet);
                StopPolling
            }
            PollEvent::ResumePolling(_) => KeepPolling,
            PollEvent::Timeout => unreachable!(),
        });
        reply.unwrap()
    }

    pub fn wait_for_ack(&mut self) -> Result<(), WaitError> {
        for _ in 0..self.shards() {
            match self.wait_for_next_reply() {
                ControlReplyPacket::Ack(_) => {}
                r => return Err(WaitError::WrongReply(r)),
            }
        }
        Ok(())
    }

    pub fn wait_for_state_size(&mut self) -> Result<usize, WaitError> {
        let mut size = 0;
        for _ in 0..self.shards() {
            match self.wait_for_next_reply() {
                ControlReplyPacket::StateSize(s) => size += s,
                r => return Err(WaitError::WrongReply(r)),
            }
        }
        Ok(size)
    }

    pub fn wait_for_statistics(
        &mut self,
    ) -> Result<Vec<(DomainStats, HashMap<NodeIndex, NodeStats>)>, WaitError> {
        let mut stats = Vec::with_capacity(self.shards());
        for _ in 0..self.shards() {
            match self.wait_for_next_reply() {
                ControlReplyPacket::Statistics(d, s) => stats.push((d, s)),
                r => return Err(WaitError::WrongReply(r)),
            }
        }
        Ok(stats)
    }
}
