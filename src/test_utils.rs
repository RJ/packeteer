#![allow(unused)]
#![allow(missing_docs)]
//! Test Utils contains a basic test harness that creates two endpoints, and transfers messages
//! via JitterPipe for configurable packet loss etc.
//!
//! Used in integration tests and benchmarks.
use crate::*;

pub fn init_logger() {
    let _ = env_logger::builder()
        .write_style(env_logger::WriteStyle::Always)
        // .is_test(true)
        .try_init();
}

pub fn random_payload_max_frags(max_fragments: u32) -> Vec<u8> {
    let size = 1 + rand::random::<u32>() % (1024 * max_fragments - 1);
    random_payload(size)
}

pub fn random_payload(size: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(size as usize);
    for _ in 0..size {
        b.push(rand::random::<u8>());
    }
    b
}

#[derive(Debug, Default)]
pub struct TransmissionStats {
    pub server_sent: u32,
    pub server_received: u32,
    pub client_sent: u32,
    pub client_received: u32,
}

/// A test harness that creates two Packeteer endpoints, and "connects" them via JitterPipes
pub struct TestHarness {
    pub server: Packeteer,
    pub client: Packeteer,
    pub server_jitter_pipe: JitterPipe<BufHandle>,
    pub client_jitter_pipe: JitterPipe<BufHandle>,
    server_drop_indices: Option<Vec<u32>>,
    pub stats: TransmissionStats,
    pool: BufPool,
}
impl TestHarness {
    pub fn new(config: JitterPipeConfig) -> Self {
        let server_jitter_pipe = JitterPipe::<BufHandle>::new(config.clone());
        let client_jitter_pipe = JitterPipe::<BufHandle>::new(config);
        let server = Packeteer::default();
        let client = Packeteer::default();
        Self {
            server,
            client,
            server_jitter_pipe,
            client_jitter_pipe,
            server_drop_indices: None,
            stats: TransmissionStats::default(),
            pool: BufPool::default(),
        }
    }
    pub fn collect_client_acks(&mut self, channel: u8) -> Vec<MessageId> {
        self.client.drain_message_acks(channel).collect::<Vec<_>>()
    }
    pub fn collect_server_acks(&mut self, channel: u8) -> Vec<MessageId> {
        self.server.drain_message_acks(channel).collect::<Vec<_>>()
    }
    pub fn collect_client_messages(&mut self, channel: u8) -> Vec<ReceivedMessage> {
        self.client
            .drain_received_messages(channel)
            .collect::<Vec<_>>()
    }
    pub fn collect_server_messages(&mut self, channel: u8) -> Vec<ReceivedMessage> {
        self.server
            .drain_received_messages(channel)
            .collect::<Vec<_>>()
    }

    /// advances but any packets with specific indexes the server sends are lost
    /// first packet it sends this tick has index 0, then 1, etc.
    pub fn advance_with_server_outbound_drops(
        &mut self,
        dt: f64,
        drop_indices: Vec<u32>,
    ) -> &TransmissionStats {
        self.server_drop_indices = Some(drop_indices);
        self.advance(dt);
        self.server_drop_indices = None;
        &self.stats
    }
    /// advances time, then advances the server first, then client.
    /// Transmits S2C and C2S messages via configured jitter pipes.
    pub fn advance(&mut self, dt: f64) -> &TransmissionStats {
        info!("🟡 server.update({dt}) --> {} ----", self.server.time + dt);
        self.server.update(dt);
        info!("🟠 client.update({dt}) --> {} ----", self.server.time + dt);
        self.client.update(dt);
        let empty = Vec::new();
        let server_drop_indices = self.server_drop_indices.as_ref().unwrap_or(&empty);

        info!("🟡 server -> compose and send packets");
        let server_sent = self
            .server
            .drain_packets_to_send()
            .fold(0_u32, |acc, packet| {
                if !server_drop_indices.contains(&acc) {
                    self.server_jitter_pipe.insert(packet);
                }
                acc + 1
            });

        info!("🟠 client -> process incoming packets");
        let mut client_received = 0;
        while let Some(p) = self.server_jitter_pipe.take_next() {
            client_received += 1;
            let packet_len = p.len();
            let mut reader = Cursor::new(p.as_slice());
            match read_packet(&mut reader, &self.pool).unwrap() {
                ProtocolPacket::Messages(mut m) => {
                    self.client.process_incoming_packet(packet_len, m);
                }
                _ => panic!("Invalid proto msg"),
            };
        }

        info!("🟠 client -> compose and send packets");
        let client_sent = self.client.drain_packets_to_send().fold(0, |acc, packet| {
            self.client_jitter_pipe.insert(packet);
            acc + 1
        });

        info!("🟡 server -> process incoming packets");
        let mut server_received = 0;
        while let Some(p) = self.client_jitter_pipe.take_next() {
            server_received += 1;
            let packet_len = p.len();
            let mut reader = Cursor::new(p.as_slice());
            match read_packet(&mut reader, &self.pool).unwrap() {
                ProtocolPacket::Messages(mut m) => {
                    self.server.process_incoming_packet(packet_len, m);
                }
                _ => panic!("Invalid proto msg"),
            };
        }

        self.stats.server_received += server_received;
        self.stats.server_sent += server_sent;
        self.stats.client_received += client_received;
        self.stats.client_sent += client_sent;

        &self.stats
    }
}
