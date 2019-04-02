use prelude::*;
use std::collections::HashMap;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Ingress {
    /// Parent domain
    src: Option<DomainIndex>,
    /// The last packet received from each parent
    last_packet_received: HashMap<DomainIndex, usize>,
}

impl Ingress {
    pub fn new() -> Ingress {
        Ingress::default()
    }

    pub fn set_src(&mut self, src: DomainIndex) {
        assert!(self.src.is_none());
        self.src = Some(src);
    }

    pub fn src(&self) -> DomainIndex {
        self.src.expect("ingress should have a parent domain")
    }

    /// Receive a packet, keeping track of the latest packet received from each parent. If the
    /// parent crashes, we can tell the parent's replacement where to resume sending messages.
    pub fn receive_packet(&mut self, m: &Box<Packet>) {
        let (from, label, is_replay) = {
            let mut is_replay = false;
            let id = match m {
                box Packet::Message { ref id, .. } => id.as_ref().unwrap(),
                box Packet::ReplayPiece { ref id, .. } => {
                    is_replay = true;
                    id.as_ref().unwrap()
                },
                _ => unreachable!(),
            };
            (id.from, id.label, is_replay)
        };

        // println!("RECEIVE PACKET #{} <- {}", label, from.index());

        // labels must be increasing UNLESS the message is a replay
        let old_label = self.last_packet_received.get(&from);
        if let Some(old_label) = old_label {
            if label <= *old_label {
                assert!(is_replay);
                assert_eq!(label, *old_label);
            }
        }

        self.last_packet_received.insert(from, label);
    }

    /// Replace an incoming connection from `old` with `new`.
    /// Returns the label of the next message expected from the new connection.
    pub fn new_incoming(&mut self, old: DomainIndex, new: DomainIndex) -> usize {
        assert_eq!(self.src, Some(old));
        self.src = Some(new);
        let label = self.last_packet_received.remove(&old).unwrap_or(0);
        self.last_packet_received.insert(new, label);
        label + 1
    }

    pub(in crate::node) fn take(&mut self) -> Self {
        Clone::clone(self)
    }
}
