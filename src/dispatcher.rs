use crate::*;

#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct MessageHandle {
    pub(crate) id: MessageId,
    pub(crate) frag_index: Option<u16>,
    pub(crate) channel: u8,
}
impl MessageHandle {
    pub fn id(&self) -> MessageId {
        self.id
    }
    pub fn parent_id(&self) -> Option<MessageId> {
        self.frag_index
            .map(|frag_index| self.id.wrapping_sub(frag_index))
    }
}

pub(crate) struct MessageDispatcher {
    next_message_id: MessageId,
    sent_frag_map: SentFragMap,
    messages_in_packets: SequenceBuffer<Vec<MessageHandle>>, // HashMap<SentHandle, Vec<MessageHandle>>,
    message_reassembler: MessageReassembler,
    message_inbox: smallmap::Map<u8, Vec<ReceivedMessage>>,
    ack_inbox: smallmap::Map<u8, Vec<MessageId>>,
}

impl Default for MessageDispatcher {
    fn default() -> Self {
        Self {
            next_message_id: 0,
            sent_frag_map: SentFragMap::default(),
            messages_in_packets: SequenceBuffer::with_capacity(10000), // TODO how many unaccounted for packets do we support?
            message_reassembler: MessageReassembler::default(),
            message_inbox: smallmap::Map::default(),
            ack_inbox: smallmap::Map::default(),
        }
    }
}

impl MessageDispatcher {
    // pass in msgs parsed from received network packets.
    pub(crate) fn process_received_message(&mut self, msg: Message) {
        info!("Dispatcher::process_received_message: {msg:?}");
        let received_msg = if msg.fragment().is_none() {
            Some(ReceivedMessage::new_single(msg))
        } else {
            self.message_reassembler.add_fragment(msg)
        };

        if let Some(msg) = received_msg {
            info!("✅ Adding msg to inbox");
            self.message_inbox
                .entry(msg.channel())
                .or_default()
                .push(msg);
        }
    }

    /// get the final messages for the consumer
    pub(crate) fn drain_received_messages(
        &mut self,
        channel: u8,
    ) -> std::vec::Drain<'_, ReceivedMessage> {
        self.message_inbox.entry(channel).or_default().drain(..)
    }

    /// get the acked  messages for the consumer
    pub(crate) fn drain_message_acks(&mut self, channel: u8) -> std::vec::Drain<'_, MessageId> {
        self.ack_inbox.entry(channel).or_default().drain(..)
    }

    /// sets the list of messageids contained in the packet
    pub(crate) fn set_packet_message_handles(
        &mut self,
        packet_handle: SentHandle,
        message_handles: Vec<MessageHandle>,
    ) -> Result<(), PacketeerError> {
        self.messages_in_packets
            .insert(message_handles, packet_handle.0)?;
        Ok(())
    }

    // updates ack_inbox with messages acked as a result of this packet being acks.
    // informs channels of acks so they can cleanup
    pub(crate) fn acked_packet(
        &mut self,
        packet_handle: &SentHandle,
        channel_list: &mut ChannelList,
    ) {
        // check message handles that were just acked - if any are fragments, we need to log that
        // in the frag map, incase it results in a parent message id being acked (ie, all frag messages are now acked)
        if let Some(msg_handles) = self.messages_in_packets.remove(packet_handle.0) {
            info!("Acked packet: {packet_handle:?} --> acked msgs: {msg_handles:?}");
            for msg_handle in &msg_handles {
                // let channel know, so it doesn't retransmit this message:
                channel_list
                    .get_mut(msg_handle.channel)
                    .unwrap()
                    .message_ack_received(msg_handle);
                if let Some(parent_id) = msg_handle.parent_id() {
                    // fragment message
                    if self.sent_frag_map.ack_fragment_message(
                        parent_id,
                        msg_handle.id(), // .frag_index
                                         // .expect("used to calc parent id, so must exist"),
                    ) {
                        self.ack_inbox
                            .entry(msg_handle.channel)
                            .or_default()
                            .push(parent_id);
                    } else {
                        info!("got fragment ack for parent {parent_id}, but not all yet {msg_handle:?} ");
                    }
                } else {
                    // non-fragment messages directly map to an acked message
                    self.ack_inbox
                        .entry(msg_handle.channel)
                        .or_default()
                        .push(msg_handle.id());
                }
            }
        }
    }

    pub(crate) fn add_message_to_channel(
        &mut self,
        pool: &BufPool,
        channel: &mut Box<dyn Channel>,
        payload: &[u8],
    ) -> MessageId {
        if payload.len() <= 1024 {
            let id = self.next_message_id();
            channel.enqueue_message(pool, id, payload, Fragmented::No);
            id
        } else {
            self.add_large_message_to_channel(pool, channel, payload)
        }
    }

    fn add_large_message_to_channel(
        &mut self,
        pool: &BufPool,
        channel: &mut Box<dyn Channel>,
        payload: &[u8],
    ) -> MessageId {
        assert!(payload.len() > 1024);
        // all fragments use the same message id.
        let full_payload_size = payload.len();
        // split into multiple messages.
        // each fragment has a unique message id, but due to sequential allocation you can always
        // calculate the id of the first fragment - ie the one that's returned to the user for acking -
        // by taking the fragment id and subtracting the index.
        //
        // ie the message.id of the fragment with index 0 is the parent ackable message id.
        let remainder = if full_payload_size % 1024 > 0 { 1 } else { 0 };
        let num_fragments = ((full_payload_size / 1024) + remainder) as u16;
        let mut frag_ids = Vec::new();
        // id of first frag message is the parent id for the group
        let mut id = self.next_message_id();
        let parent_id = id;
        for index in 0..num_fragments {
            let payload_size = if index == num_fragments - 1 {
                full_payload_size as u16 - (num_fragments - 1) * 1024
            } else {
                1024_u16
            };
            if index > 0 {
                id = self.next_message_id();
            }
            frag_ids.push(id);
            info!("Adding frag msg {id} frag:{index}/{num_fragments}");
            let fragment = Fragment {
                index,
                num_fragments,
                parent_id,
            };
            let start = index as usize * 1024;
            let end = start + payload_size as usize;
            let frag_payload = &payload[start..end];
            channel.enqueue_message(pool, id, frag_payload, Fragmented::Yes(fragment));
        }
        self.sent_frag_map
            .insert_fragmented_message(parent_id, frag_ids);
        parent_id
    }

    fn next_message_id(&mut self) -> MessageId {
        let ret = self.next_message_id;
        self.next_message_id = self.next_message_id.wrapping_add(1);
        ret
    }
}

#[derive(Default, Clone, PartialEq)]
enum FragAckStatus {
    #[default]
    Unknown,
    Complete,
    // lists remaining msg ids we need:
    Partial(Vec<MessageId>),
}

/// tracks the unacked message ids assigned to fragments of a larger message id.
/// they're removed as they are acked & once depleted, the original parent message id is acked.

pub struct SentFragMap {
    m: SequenceBuffer<FragAckStatus>,
    // m: HashMap<MessageId, Vec<u16>>,
}
impl Default for SentFragMap {
    fn default() -> Self {
        Self {
            m: SequenceBuffer::with_capacity(1000),
        }
    }
}
impl SentFragMap {
    pub fn insert_fragmented_message(&mut self, id: MessageId, fragment_ids: Vec<u16>) {
        let _ = self.m.insert(FragAckStatus::Partial(fragment_ids), id);
        // let res = self.m.insert(id, fragment_ids);
        // assert!(
        //     res.is_none(),
        //     "why are we overwriting something in the fragmap?"
        // );
    }
    /// returns true if parent message is whole/acked.
    pub fn ack_fragment_message(&mut self, parent_id: MessageId, fragment_id: MessageId) -> bool {
        let Some(entry) = self.m.get_mut(parent_id) else {
            return false;
        };
        let ret = match entry {
            FragAckStatus::Complete => {
                info!("Message {parent_id} already completed arrived.");
                false
            }
            FragAckStatus::Unknown => {
                info!("Message {parent_id} unknown to frag map");
                false
            }
            FragAckStatus::Partial(ref mut remaining) => {
                remaining.retain(|id| *id != fragment_id);
                //  Remaining fragment indexs for parent 841, fragment_id=1 = [841, 842, 843, 844, 845, 846, 847, 848, 849, 850, 851, 852, 853]
                // oops, am i using msg ids or fragment indexes for this?
                info!("Remaining fragment indexs for parent {parent_id}, fragment_id={fragment_id} = {remaining:?}");
                remaining.is_empty()
            }
        };
        if ret {
            self.m.insert(FragAckStatus::Complete, parent_id).unwrap();
            info!("Message fully acked, all fragments accounted for {parent_id}");
        }
        ret
    }
}