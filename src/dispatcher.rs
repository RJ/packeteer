use crate::*;
// use log::*;

#[derive(Default, Debug)]
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

#[derive(Default)]
pub(crate) struct MessageDispatcher {
    next_message_id: MessageId,
    sent_frag_map: SentFragMap,
    messages_in_packets: HashMap<SentHandle, Vec<MessageHandle>>,
    message_reassembler: MessageReassembler,
    /// received fully assembled messages ready for the consumer:
    message_inbox: HashMap<u8, Vec<ReceivedMessage>>,
    ack_inbox: HashMap<u8, Vec<MessageId>>,
}

impl MessageDispatcher {
    // pass in msgs parsed from received network packets.
    pub(crate) fn process_received_message(&mut self, msg: Message) {
        // info!("Dispatcher::process_received_message: {msg:?}");
        // in case it's a fragment, do reassembly.
        // this just yields a ReceivedMessage instantly for non-fragments:
        if let Some(received_msg) = self.message_reassembler.add_fragment(&msg) {
            info!("✅ Adding message to inbox {msg:?}",);
            self.message_inbox
                .entry(received_msg.channel)
                .or_default()
                .push(received_msg);
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
    ) {
        self.messages_in_packets
            .insert(packet_handle, message_handles);
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
        if let Some(msg_handles) = self.messages_in_packets.remove(packet_handle) {
            for msg_handle in &msg_handles {
                if let Some(parent_id) = msg_handle.parent_id() {
                    // fragment message
                    if self
                        .sent_frag_map
                        .ack_fragment_message(parent_id, msg_handle.id())
                    {
                        channel_list
                            .get_mut(msg_handle.channel)
                            .unwrap()
                            .message_ack_received(msg_handle);
                        self.ack_inbox
                            .entry(msg_handle.channel)
                            .or_default()
                            .push(parent_id);
                    }
                } else {
                    channel_list
                        .get_mut(msg_handle.channel)
                        .unwrap()
                        .message_ack_received(msg_handle);
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
        channel: &mut Box<dyn Channel>,
        payload: Bytes,
    ) -> MessageId {
        if payload.len() <= 1024 {
            self.add_small_message_to_channel(channel, payload)
        } else {
            self.add_large_message_to_channel(channel, payload)
        }
    }

    fn add_small_message_to_channel(
        &mut self,
        channel: &mut Box<dyn Channel>,
        payload: Bytes,
    ) -> MessageId {
        assert!(payload.len() <= 1024);
        let id = self.next_message_id();
        channel.enqueue_message(id, payload, Fragmented::No);
        id
    }

    fn add_large_message_to_channel(
        &mut self,
        channel: &mut Box<dyn Channel>,
        mut payload: Bytes,
    ) -> MessageId {
        assert!(payload.len() > 1024);
        // all fragments use the same message id.
        let payload_len = payload.len();
        // split into multiple messages.
        // each fragment has a unique message id, but due to sequential allocation you can always
        // calculate the id of the first fragment - ie the one that's returned to the user for acking -
        // by taking the fragment id and subtracting the index.
        //
        // ie the message.id of the fragment with index 0 is the parent ackable message id.
        let remainder = if payload_len % 1024 > 0 { 1 } else { 0 };
        let num_fragments = ((payload_len / 1024) + remainder) as u16;
        let mut frag_ids = Vec::new();
        // id of first frag message is the parent id for the group
        let mut id = self.next_message_id();
        let parent_id = id;
        for index in 0..num_fragments {
            let payload_size = if index == num_fragments - 1 {
                payload_len as u16 - (num_fragments - 1) * 1024
            } else {
                1024_u16
            };
            let frag_payload = payload.split_to(payload_size as usize);
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
            channel.enqueue_message(id, frag_payload, Fragmented::Yes(fragment));
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

/// tracks the unacked message ids assigned to fragments of a larger message id.
/// they're removed as they are acked & once depleted, the original parent message id is acked.
#[derive(Default)]
pub struct SentFragMap {
    m: HashMap<MessageId, Vec<u16>>,
}
impl SentFragMap {
    pub fn insert_fragmented_message(&mut self, id: MessageId, fragment_ids: Vec<u16>) {
        let res = self.m.insert(id, fragment_ids);
        assert!(
            res.is_none(),
            "why are we overwriting something in the fragmap?"
        );
    }
    /// returns true if parent message is whole/acked.
    pub fn ack_fragment_message(&mut self, parent_id: MessageId, fragment_id: MessageId) -> bool {
        let ret = if let Some(v) = self.m.get_mut(&parent_id) {
            v.retain(|id| *id != fragment_id);
            v.is_empty()
        } else {
            false
        };
        if ret {
            if let Some(_removed) = self.m.remove(&parent_id) {
                info!("reassembly complete for {parent_id}");
            } else {
                warn!("No parent message id ({parent_id}) in the fragmap for completed msg");
            }
        }
        ret
    }
}
