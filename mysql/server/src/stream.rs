//! Bounded incremental classic-protocol framing without a runtime or socket.

use std::{collections::VecDeque, error::Error, fmt};

use crate::{PacketCodec, PacketCodecError, MAX_PACKET_PAYLOAD_LEN, PACKET_HEADER_LEN};

/// An owned packet emitted by [`PacketStreamDecoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamPacket {
    /// Sequence number from the packet header.
    pub sequence_id: u8,
    /// Packet payload without the four-byte header.
    pub payload: Vec<u8>,
}

impl StreamPacket {
    /// Encodes this packet with the existing bounded packet codec.
    pub fn encode(&self, codec: PacketCodec) -> Result<Vec<u8>, PacketCodecError> {
        codec.encode(self.sequence_id, &self.payload)
    }
}

/// Incrementally decodes classic packets from arbitrary input chunks.
///
/// The decoder stores at most one incomplete packet. A malformed or oversized
/// header makes it terminal; callers must explicitly call [`Self::reset`]
/// before accepting another stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketStreamDecoder {
    codec: PacketCodec,
    max_buffered_payload_bytes: usize,
    max_packets_per_feed: usize,
    header: [u8; PACKET_HEADER_LEN],
    header_len: usize,
    payload: Option<PartialPayload>,
    terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PartialPayload {
    sequence_id: u8,
    expected_len: usize,
    bytes: Vec<u8>,
}

impl PacketStreamDecoder {
    /// Creates a decoder with independent wire-payload, buffer, and output limits.
    ///
    /// `max_buffered_payload_bytes` excludes the fixed four-byte header. A zero
    /// limit therefore permits only zero-payload packets, without allocating a
    /// payload buffer.
    ///
    /// A non-zero `max_packets_per_feed` bounds the number of owned packets a
    /// single untrusted input chunk can cause the caller to retain.
    pub const fn new(
        codec: PacketCodec,
        max_buffered_payload_bytes: usize,
        max_packets_per_feed: usize,
    ) -> Result<Self, StreamDecoderConfigError> {
        if max_packets_per_feed == 0 {
            return Err(StreamDecoderConfigError::ZeroPacketsPerFeed);
        }
        Ok(Self {
            codec,
            max_buffered_payload_bytes,
            max_packets_per_feed,
            header: [0; PACKET_HEADER_LEN],
            header_len: 0,
            payload: None,
            terminal: false,
        })
    }

    /// Returns the codec used to validate packet payload lengths.
    pub const fn codec(&self) -> PacketCodec {
        self.codec
    }

    /// Returns the maximum payload bytes retained for one incomplete packet.
    pub const fn max_buffered_payload_bytes(&self) -> usize {
        self.max_buffered_payload_bytes
    }

    /// Returns the maximum number of packets emitted by one [`Self::feed`].
    pub const fn max_packets_per_feed(&self) -> usize {
        self.max_packets_per_feed
    }

    /// Returns whether a framing error has made this decoder terminal.
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Returns the currently buffered bytes, excluding bytes already emitted.
    pub fn buffered_payload_bytes(&self) -> usize {
        self.payload
            .as_ref()
            .map_or(0, |payload| payload.bytes.len())
    }

    /// Returns the input bytes retained from the current incomplete frame.
    ///
    /// This includes one to three unparsed header bytes and any payload bytes
    /// copied after a complete header. Once a complete header is parsed, its
    /// fields are kept as decoder state rather than as buffered input bytes.
    pub fn buffered_bytes(&self) -> usize {
        self.header_len + self.buffered_payload_bytes()
    }

    /// Returns whether the decoder is waiting for the rest of a frame.
    ///
    /// A complete header with no payload bytes copied yet is still a partial
    /// frame, even though [`Self::buffered_bytes`] is zero in that state.
    pub fn has_partial_frame(&self) -> bool {
        self.header_len != 0 || self.payload.is_some()
    }

    /// Feeds one arbitrary input chunk and returns every complete packet in it.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<StreamPacket>, StreamDecoderError> {
        if self.terminal {
            return Err(StreamDecoderError::Terminal);
        }

        let mut offset = 0;
        let mut packets = Vec::new();
        while offset < chunk.len() {
            if self.payload.is_none() {
                let copied = self.copy_header(&chunk[offset..]);
                offset += copied;
                if self.header_len < PACKET_HEADER_LEN {
                    break;
                }

                if packets.len() >= self.max_packets_per_feed {
                    return self.fail(StreamDecoderError::PacketsPerFeedExceeded {
                        packets: packets.len(),
                        limit: self.max_packets_per_feed,
                    });
                }

                let payload_len = self.payload_len();
                let sequence_id = self.header[3];
                if payload_len == MAX_PACKET_PAYLOAD_LEN {
                    return self
                        .fail(StreamDecoderError::ContinuationPacketUnsupported { sequence_id });
                }
                if payload_len > self.codec.max_payload_len() {
                    return self.fail(StreamDecoderError::PayloadTooLarge {
                        length: payload_len,
                        limit: self.codec.max_payload_len(),
                    });
                }
                if payload_len > self.max_buffered_payload_bytes {
                    return self.fail(StreamDecoderError::BufferBudgetExceeded {
                        length: payload_len,
                        limit: self.max_buffered_payload_bytes,
                    });
                }

                self.header_len = 0;
                self.payload = Some(PartialPayload {
                    sequence_id,
                    expected_len: payload_len,
                    bytes: Vec::with_capacity(payload_len),
                });
                if payload_len == 0 {
                    let payload = self
                        .payload
                        .take()
                        .expect("zero-length payload was just initialized");
                    packets.push(StreamPacket {
                        sequence_id: payload.sequence_id,
                        payload: payload.bytes,
                    });
                    continue;
                }
            }

            let payload = self
                .payload
                .as_mut()
                .expect("a non-empty payload is initialized before copying");
            let remaining = payload.expected_len - payload.bytes.len();
            let copied = remaining.min(chunk.len() - offset);
            payload
                .bytes
                .extend_from_slice(&chunk[offset..offset + copied]);
            offset += copied;
            if payload.bytes.len() == payload.expected_len {
                let payload = self
                    .payload
                    .take()
                    .expect("a complete payload was just copied");
                packets.push(StreamPacket {
                    sequence_id: payload.sequence_id,
                    payload: payload.bytes,
                });
            }
        }
        Ok(packets)
    }

    /// Discards partial input and clears a terminal error.
    pub fn reset(&mut self) {
        self.header = [0; PACKET_HEADER_LEN];
        self.header_len = 0;
        self.payload = None;
        self.terminal = false;
    }

    fn copy_header(&mut self, input: &[u8]) -> usize {
        let needed = PACKET_HEADER_LEN - self.header_len;
        let copied = needed.min(input.len());
        self.header[self.header_len..self.header_len + copied].copy_from_slice(&input[..copied]);
        self.header_len += copied;
        copied
    }

    fn payload_len(&self) -> usize {
        usize::from(self.header[0])
            | (usize::from(self.header[1]) << 8)
            | (usize::from(self.header[2]) << 16)
    }

    fn fail<T>(&mut self, error: StreamDecoderError) -> Result<T, StreamDecoderError> {
        self.terminal = true;
        self.header_len = 0;
        self.payload = None;
        Err(error)
    }
}

/// Errors returned by incremental packet decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDecoderError {
    /// A previous framing error requires an explicit reset.
    Terminal,
    /// A packet payload exceeds the codec's configured limit.
    PayloadTooLarge { length: usize, limit: usize },
    /// The protocol's continuation-packet form is outside this bounded slice.
    ContinuationPacketUnsupported { sequence_id: u8 },
    /// The packet would exceed the decoder's partial-payload buffer budget.
    BufferBudgetExceeded { length: usize, limit: usize },
    /// A feed would emit more owned packets than its configured output bound.
    PacketsPerFeedExceeded { packets: usize, limit: usize },
}

impl fmt::Display for StreamDecoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Terminal => f.write_str("packet stream decoder is terminal; reset is required"),
            Self::PayloadTooLarge { length, limit } => {
                write!(f, "packet payload length {length} exceeds limit {limit}")
            }
            Self::ContinuationPacketUnsupported { sequence_id } => write!(
                f,
                "continuation packet with sequence id {sequence_id} is unsupported"
            ),
            Self::BufferBudgetExceeded { length, limit } => write!(
                f,
                "packet payload length {length} exceeds partial buffer budget {limit}"
            ),
            Self::PacketsPerFeedExceeded { packets, limit } => write!(
                f,
                "feed would emit {packets} packets, exceeding per-feed limit {limit}"
            ),
        }
    }
}

impl Error for StreamDecoderError {}

/// Invalid bounds for an incremental packet decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDecoderConfigError {
    /// A zero output bound would make every feed fail.
    ZeroPacketsPerFeed,
}

impl fmt::Display for StreamDecoderConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPacketsPerFeed => f.write_str("packets-per-feed limit must be non-zero"),
        }
    }
}

impl Error for StreamDecoderConfigError {}

/// A bounded queue of already-framed packets that supports partial writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketWriteQueue {
    codec: PacketCodec,
    max_queued_bytes: usize,
    max_queued_frames: usize,
    queued_bytes: usize,
    queue: VecDeque<QueuedFrame>,
    terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedFrame {
    frame: Vec<u8>,
    offset: usize,
}

impl PacketWriteQueue {
    /// Creates an empty queue with total byte and frame-count limits.
    pub fn new(
        codec: PacketCodec,
        max_queued_bytes: usize,
        max_queued_frames: usize,
    ) -> Result<Self, PacketWriteQueueError> {
        if max_queued_bytes == 0 {
            return Err(PacketWriteQueueError::ZeroByteLimit);
        }
        if max_queued_frames == 0 {
            return Err(PacketWriteQueueError::ZeroFrameLimit);
        }
        Ok(Self {
            codec,
            max_queued_bytes,
            max_queued_frames,
            queued_bytes: 0,
            queue: VecDeque::new(),
            terminal: false,
        })
    }

    /// Returns whether a malformed-frame error made this queue terminal.
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Returns the total unsent bytes, including packet headers.
    pub const fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// Returns the number of frames with unsent bytes.
    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }

    /// Returns the queue's total byte limit.
    pub const fn max_queued_bytes(&self) -> usize {
        self.max_queued_bytes
    }

    /// Returns the queue's frame-count limit.
    pub const fn max_queued_frames(&self) -> usize {
        self.max_queued_frames
    }

    /// Queues one complete frame after strict codec validation.
    pub fn enqueue(&mut self, frame: Vec<u8>) -> Result<(), PacketWriteQueueError> {
        if self.terminal {
            return Err(PacketWriteQueueError::Terminal);
        }
        let packet = match self.codec.decode(&frame) {
            Ok(packet) => packet,
            Err(error) => {
                return self.fail(PacketWriteQueueError::PacketCodec(error));
            }
        };
        if packet.payload.len() == MAX_PACKET_PAYLOAD_LEN {
            return self.fail(PacketWriteQueueError::ContinuationPacketUnsupported {
                sequence_id: packet.sequence_id,
            });
        }
        if self.queue.len() >= self.max_queued_frames {
            return Err(PacketWriteQueueError::FrameLimitExceeded {
                limit: self.max_queued_frames,
            });
        }
        if frame.len() > self.max_queued_bytes - self.queued_bytes {
            return Err(PacketWriteQueueError::ByteLimitExceeded {
                queued: self.queued_bytes,
                incoming: frame.len(),
                limit: self.max_queued_bytes,
            });
        }
        self.queued_bytes += frame.len();
        self.queue.push_back(QueuedFrame { frame, offset: 0 });
        Ok(())
    }

    /// Queues every frame in a batch, or queues none of them.
    ///
    /// The queue validates every frame and both aggregate limits before it
    /// changes its contents. A batch preflight error leaves existing queued
    /// frames and the terminal state unchanged.
    pub fn enqueue_batch<I>(&mut self, frames: I) -> Result<(), PacketWriteQueueError>
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        if self.terminal {
            return Err(PacketWriteQueueError::Terminal);
        }

        let available_frames = self.max_queued_frames - self.queue.len();
        let available_bytes = self.max_queued_bytes - self.queued_bytes;
        let mut staged_frames = Vec::new();
        let mut incoming_bytes = 0usize;
        for frame in frames {
            if staged_frames.len() >= available_frames {
                return Err(PacketWriteQueueError::FrameLimitExceeded {
                    limit: self.max_queued_frames,
                });
            }

            let packet = self
                .codec
                .decode(&frame)
                .map_err(PacketWriteQueueError::PacketCodec)?;
            if packet.payload.len() == MAX_PACKET_PAYLOAD_LEN {
                return Err(PacketWriteQueueError::ContinuationPacketUnsupported {
                    sequence_id: packet.sequence_id,
                });
            }

            incoming_bytes = incoming_bytes
                .checked_add(frame.len())
                .ok_or(PacketWriteQueueError::BatchByteLengthOverflow)?;
            if incoming_bytes > available_bytes {
                return Err(PacketWriteQueueError::ByteLimitExceeded {
                    queued: self.queued_bytes,
                    incoming: incoming_bytes,
                    limit: self.max_queued_bytes,
                });
            }
            staged_frames.push(frame);
        }

        self.queued_bytes += incoming_bytes;
        self.queue.reserve(staged_frames.len());
        self.queue.extend(
            staged_frames
                .into_iter()
                .map(|frame| QueuedFrame { frame, offset: 0 }),
        );
        Ok(())
    }

    /// Encodes and queues one payload with the existing packet codec.
    pub fn enqueue_payload(
        &mut self,
        sequence_id: u8,
        payload: &[u8],
    ) -> Result<(), PacketWriteQueueError> {
        if self.terminal {
            return Err(PacketWriteQueueError::Terminal);
        }
        if payload.len() == MAX_PACKET_PAYLOAD_LEN {
            return self.fail(PacketWriteQueueError::ContinuationPacketUnsupported { sequence_id });
        }
        if payload.len() > self.codec.max_payload_len() {
            return Err(PacketWriteQueueError::PacketCodec(
                PacketCodecError::PayloadTooLarge {
                    length: payload.len(),
                    limit: self.codec.max_payload_len(),
                },
            ));
        }
        if self.queue.len() >= self.max_queued_frames {
            return Err(PacketWriteQueueError::FrameLimitExceeded {
                limit: self.max_queued_frames,
            });
        }
        let frame_len = payload.len().checked_add(PACKET_HEADER_LEN).ok_or(
            PacketWriteQueueError::FrameLengthOverflow {
                payload_length: payload.len(),
            },
        )?;
        if frame_len > self.max_queued_bytes - self.queued_bytes {
            return Err(PacketWriteQueueError::ByteLimitExceeded {
                queued: self.queued_bytes,
                incoming: frame_len,
                limit: self.max_queued_bytes,
            });
        }
        let frame = self
            .codec
            .encode(sequence_id, payload)
            .map_err(PacketWriteQueueError::PacketCodec)?;
        debug_assert_eq!(frame.len(), frame_len);
        self.queued_bytes += frame_len;
        self.queue.push_back(QueuedFrame { frame, offset: 0 });
        Ok(())
    }

    /// Returns the unsent portion of the oldest queued frame.
    pub fn front(&self) -> Option<&[u8]> {
        self.queue.front().map(|frame| &frame.frame[frame.offset..])
    }

    /// Marks bytes from the oldest frame as written.
    pub fn advance(&mut self, written: usize) -> Result<(), PacketWriteQueueError> {
        if self.terminal {
            return Err(PacketWriteQueueError::Terminal);
        }
        let Some(frame) = self.queue.front_mut() else {
            return Err(PacketWriteQueueError::NoQueuedFrame);
        };
        let remaining = frame.frame.len() - frame.offset;
        if written > remaining {
            return Err(PacketWriteQueueError::WriteBeyondFrame { written, remaining });
        }
        frame.offset += written;
        self.queued_bytes -= written;
        if frame.offset == frame.frame.len() {
            self.queue.pop_front();
        }
        Ok(())
    }

    /// Discards queued frames and clears a terminal framing error.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.terminal = false;
    }

    fn fail<T>(&mut self, error: PacketWriteQueueError) -> Result<T, PacketWriteQueueError> {
        self.queue.clear();
        self.queued_bytes = 0;
        self.terminal = true;
        Err(error)
    }
}

/// Errors returned by the bounded partial-write queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacketWriteQueueError {
    /// The queue's total byte limit must be non-zero.
    ZeroByteLimit,
    /// The queue's frame-count limit must be non-zero.
    ZeroFrameLimit,
    /// A previous malformed-frame error requires an explicit reset.
    Terminal,
    /// The queued frame failed strict packet validation.
    PacketCodec(PacketCodecError),
    /// The protocol's continuation-packet form is outside this bounded slice.
    ContinuationPacketUnsupported { sequence_id: u8 },
    /// Adding this frame would exceed the total frame-count limit. The caller
    /// can advance queued data and retry without resetting the queue.
    FrameLimitExceeded { limit: usize },
    /// Adding this frame would exceed the total queued-byte limit. The caller
    /// can advance queued data and retry without resetting the queue.
    ByteLimitExceeded {
        queued: usize,
        incoming: usize,
        limit: usize,
    },
    /// Adding the encoded lengths in a batch overflowed `usize`.
    BatchByteLengthOverflow,
    /// Adding the packet header to the payload length overflowed `usize`.
    FrameLengthOverflow { payload_length: usize },
    /// No frame is available to advance.
    NoQueuedFrame,
    /// A write acknowledgement exceeds the oldest frame's remaining bytes.
    WriteBeyondFrame { written: usize, remaining: usize },
}

impl From<PacketCodecError> for PacketWriteQueueError {
    fn from(error: PacketCodecError) -> Self {
        Self::PacketCodec(error)
    }
}

impl fmt::Display for PacketWriteQueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroByteLimit => f.write_str("write queue byte limit must be non-zero"),
            Self::ZeroFrameLimit => f.write_str("write queue frame limit must be non-zero"),
            Self::Terminal => f.write_str("write queue is terminal; reset is required"),
            Self::PacketCodec(error) => write!(f, "packet codec error: {error}"),
            Self::ContinuationPacketUnsupported { sequence_id } => write!(
                f,
                "continuation packet with sequence id {sequence_id} is unsupported"
            ),
            Self::FrameLimitExceeded { limit } => {
                write!(f, "write queue frame limit {limit} would be exceeded")
            }
            Self::ByteLimitExceeded {
                queued,
                incoming,
                limit,
            } => write!(
                f,
                "queued bytes {queued} plus incoming frame {incoming} exceeds limit {limit}"
            ),
            Self::BatchByteLengthOverflow => {
                f.write_str("batch frame lengths overflow the platform byte count")
            }
            Self::FrameLengthOverflow { payload_length } => write!(
                f,
                "payload length {payload_length} cannot be represented with a packet header"
            ),
            Self::NoQueuedFrame => f.write_str("write queue has no queued frame"),
            Self::WriteBeyondFrame { written, remaining } => write!(
                f,
                "write acknowledgement {written} exceeds frame remainder {remaining}"
            ),
        }
    }
}

impl Error for PacketWriteQueueError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dispatch_command_frame, ClassicConnection, ClientHandshakeResponseConfig,
        CommandExecutionResult, CommandExecutor, CommandOkResult, ConnectionState,
        InitialAuthenticationResult, InitialHandshakeSettings, TransportSecurity,
        CACHING_SHA2_PASSWORD_PLUGIN, COMMAND_SEQUENCE_ID, COM_PING,
        REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
    };

    const CODEC: PacketCodec = PacketCodec {
        max_payload_len: 64,
    };
    const HANDSHAKE_CODEC: PacketCodec = PacketCodec {
        max_payload_len: 4096,
    };

    #[test]
    fn decoder_handles_empty_fragmented_and_coalesced_input() {
        let mut decoder = PacketStreamDecoder::new(CODEC, 64, 8).unwrap();
        assert!(decoder.feed(&[]).unwrap().is_empty());

        let first = CODEC.encode(7, b"hello").unwrap();
        let second = CODEC.encode(8, b"world").unwrap();
        assert!(decoder.feed(&first[..2]).unwrap().is_empty());
        assert!(decoder.feed(&[]).unwrap().is_empty());
        assert!(decoder.feed(&first[2..5]).unwrap().is_empty());
        let mut coalesced = first[5..].to_vec();
        coalesced.extend_from_slice(&second);
        assert_eq!(
            decoder.feed(&coalesced).unwrap(),
            vec![
                StreamPacket {
                    sequence_id: 7,
                    payload: b"hello".to_vec(),
                },
                StreamPacket {
                    sequence_id: 8,
                    payload: b"world".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn decoder_reports_partial_header_and_payload_state() {
        let mut decoder = PacketStreamDecoder::new(CODEC, 64, 8).unwrap();
        assert_eq!(decoder.buffered_bytes(), 0);
        assert!(!decoder.has_partial_frame());

        assert!(decoder.feed(&[3, 0]).unwrap().is_empty());
        assert_eq!(decoder.buffered_bytes(), 2);
        assert!(decoder.has_partial_frame());

        assert!(decoder.feed(&[0, 9, b'a']).unwrap().is_empty());
        assert_eq!(decoder.buffered_bytes(), 1);
        assert!(decoder.has_partial_frame());

        assert_eq!(
            decoder.feed(b"bc").unwrap(),
            vec![StreamPacket {
                sequence_id: 9,
                payload: b"abc".to_vec(),
            }]
        );
        assert_eq!(decoder.buffered_bytes(), 0);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn decoder_reports_a_complete_header_waiting_for_payload_as_partial() {
        let mut decoder = PacketStreamDecoder::new(CODEC, 64, 8).unwrap();
        assert!(decoder.feed(&[1, 0, 0, 2]).unwrap().is_empty());
        assert_eq!(decoder.buffered_bytes(), 0);
        assert!(decoder.has_partial_frame());

        decoder.reset();
        assert_eq!(decoder.buffered_bytes(), 0);
        assert!(!decoder.has_partial_frame());
    }

    #[test]
    fn decoder_accepts_zero_payload_and_sequence_wrap() {
        let mut decoder = PacketStreamDecoder::new(CODEC, 0, 2).unwrap();
        let mut input = CODEC.encode(u8::MAX, &[]).unwrap();
        input.extend_from_slice(&CODEC.encode(0, &[]).unwrap());
        assert_eq!(
            decoder.feed(&input).unwrap(),
            vec![
                StreamPacket {
                    sequence_id: u8::MAX,
                    payload: Vec::new(),
                },
                StreamPacket {
                    sequence_id: 0,
                    payload: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn decoder_rejects_oversize_before_payload_allocation_and_requires_reset() {
        let codec = PacketCodec::new(8).unwrap();
        let mut decoder = PacketStreamDecoder::new(codec, 8, 8).unwrap();
        let oversize_header = [9, 0, 0, 4];
        assert_eq!(
            decoder.feed(&oversize_header),
            Err(StreamDecoderError::PayloadTooLarge {
                length: 9,
                limit: 8,
            })
        );
        assert!(decoder.is_terminal());
        assert_eq!(decoder.buffered_payload_bytes(), 0);
        assert_eq!(decoder.feed(&[]), Err(StreamDecoderError::Terminal));

        decoder.reset();
        assert!(!decoder.is_terminal());
        assert_eq!(
            decoder.feed(&codec.encode(1, b"ok").unwrap()).unwrap()[0].payload,
            b"ok"
        );
    }

    #[test]
    fn decoder_rejects_payload_buffer_budget_at_header_completion() {
        let mut decoder = PacketStreamDecoder::new(CODEC, 2, 8).unwrap();
        let header = [3, 0, 0, 1];
        assert_eq!(
            decoder.feed(&header),
            Err(StreamDecoderError::BufferBudgetExceeded {
                length: 3,
                limit: 2,
            })
        );
        assert!(decoder.is_terminal());
        assert_eq!(decoder.buffered_payload_bytes(), 0);
    }

    #[test]
    fn writer_preflights_byte_budget_before_encoding_large_payloads() {
        let codec = PacketCodec::new(MAX_PACKET_PAYLOAD_LEN).unwrap();
        let mut writer = PacketWriteQueue::new(codec, PACKET_HEADER_LEN, 1).unwrap();
        let payload = vec![0; 1024];
        assert_eq!(
            writer.enqueue_payload(1, &payload),
            Err(PacketWriteQueueError::ByteLimitExceeded {
                queued: 0,
                incoming: PACKET_HEADER_LEN + payload.len(),
                limit: PACKET_HEADER_LEN,
            })
        );
        assert_eq!(writer.queued_bytes(), 0);
        assert!(!writer.is_terminal());
        writer.enqueue_payload(1, &[]).unwrap();
        assert_eq!(writer.queued_frames(), 1);
    }

    #[test]
    fn decoder_explicitly_rejects_continuation_packets() {
        let codec = PacketCodec::new(MAX_PACKET_PAYLOAD_LEN).unwrap();
        let mut decoder = PacketStreamDecoder::new(codec, MAX_PACKET_PAYLOAD_LEN, 8).unwrap();
        assert_eq!(
            decoder.feed(&[0xff, 0xff, 0xff, 3]),
            Err(StreamDecoderError::ContinuationPacketUnsupported { sequence_id: 3 })
        );
        assert!(decoder.is_terminal());
    }

    #[test]
    fn decoder_requires_a_nonzero_packets_per_feed_limit() {
        assert_eq!(
            PacketStreamDecoder::new(CODEC, 64, 0),
            Err(StreamDecoderConfigError::ZeroPacketsPerFeed)
        );
    }

    #[test]
    fn decoder_limits_coalesced_packets_before_payload_allocation_and_requires_reset() {
        let mut decoder = PacketStreamDecoder::new(CODEC, 64, 2).unwrap();
        let mut input = CODEC.encode(1, &[]).unwrap();
        input.extend_from_slice(&CODEC.encode(2, &[]).unwrap());
        input.extend_from_slice(&[3, 0, 0, 3]);

        assert_eq!(
            decoder.feed(&input),
            Err(StreamDecoderError::PacketsPerFeedExceeded {
                packets: 2,
                limit: 2,
            })
        );
        assert!(decoder.is_terminal());
        assert_eq!(decoder.buffered_payload_bytes(), 0);
        assert_eq!(decoder.feed(&[]), Err(StreamDecoderError::Terminal));

        decoder.reset();
        assert_eq!(
            decoder.feed(&CODEC.encode(4, b"ok").unwrap()).unwrap(),
            vec![StreamPacket {
                sequence_id: 4,
                payload: b"ok".to_vec(),
            }]
        );
    }

    #[derive(Debug, Default)]
    struct PingExecutor;

    impl CommandExecutor for PingExecutor {
        fn execute_init_db(
            &mut self,
            _database: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }

        fn execute_query(
            &mut self,
            _sql: &str,
        ) -> Result<CommandExecutionResult, crate::FrontendErrorKind> {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
    }

    fn ready_connection() -> ClassicConnection {
        let capabilities = REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES;
        let mut connection = ClassicConnection::with_test_nonce(
            InitialHandshakeSettings {
                capability_flags: capabilities,
                ..InitialHandshakeSettings::default()
            },
            HANDSHAKE_CODEC,
            TransportSecurity::Secure,
            [0xa5; crate::AUTH_PLUGIN_DATA_LENGTH],
        )
        .unwrap();
        connection.send_initial_handshake().unwrap();
        let response = ClientHandshakeResponseConfig::new(
            capabilities,
            0,
            crate::DEFAULT_UTF8MB4_COLLATION,
            "root",
            [0; 32],
            None::<String>,
            Some(CACHING_SHA2_PASSWORD_PLUGIN),
            None,
        )
        .encode(HANDSHAKE_CODEC, 1)
        .unwrap();
        connection
            .receive_client_handshake_frame(&response)
            .unwrap();
        connection
            .apply_initial_authentication_result(InitialAuthenticationResult::FastAuthSuccess)
            .unwrap();
        connection.send_authentication_ok().unwrap();
        connection
    }

    #[test]
    fn writer_and_reader_round_trip_a_dispatcher_response_with_partial_writes() {
        let mut connection = ready_connection();
        let command = CODEC.encode(COMMAND_SEQUENCE_ID, &[COM_PING]).unwrap();
        let mut command_reader = PacketStreamDecoder::new(CODEC, 64, 8).unwrap();
        let packet = command_reader.feed(&command[..2]).unwrap();
        assert!(packet.is_empty());
        let packet = command_reader.feed(&command[2..]).unwrap();
        assert_eq!(packet.len(), 1);
        let command_frame = packet[0].encode(CODEC).unwrap();
        let mut executor = PingExecutor;
        let frames =
            dispatch_command_frame(&mut connection, &mut executor, &command_frame).unwrap();
        assert_eq!(connection.state(), ConnectionState::Ready);

        let mut writer = PacketWriteQueue::new(CODEC, 128, 4).unwrap();
        for frame in frames {
            writer.enqueue(frame).unwrap();
        }
        let mut wire = Vec::new();
        while let Some(front) = writer.front() {
            let count = front.len().min(2);
            wire.extend_from_slice(&front[..count]);
            writer.advance(count).unwrap();
        }
        assert_eq!(writer.queued_bytes(), 0);

        let mut response_reader = PacketStreamDecoder::new(CODEC, 64, 8).unwrap();
        let responses = response_reader.feed(&wire).unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].sequence_id, 1);
        assert_eq!(responses[0].payload[0], crate::AUTH_OK_HEADER);
    }

    #[test]
    fn writer_preserves_order_and_enforces_queue_limits() {
        let mut writer = PacketWriteQueue::new(CODEC, 10, 1).unwrap();
        let first = CODEC.encode(1, b"one").unwrap();
        let second = CODEC.encode(2, b"two").unwrap();
        writer.enqueue(first.clone()).unwrap();
        assert_eq!(writer.queued_bytes(), first.len());
        assert_eq!(
            writer.enqueue(second),
            Err(PacketWriteQueueError::FrameLimitExceeded { limit: 1 })
        );
        let mut byte_limited = PacketWriteQueue::new(CODEC, first.len(), 2).unwrap();
        byte_limited.enqueue(first.clone()).unwrap();
        assert_eq!(
            byte_limited.enqueue(CODEC.encode(2, b"two").unwrap()),
            Err(PacketWriteQueueError::ByteLimitExceeded {
                queued: first.len(),
                incoming: first.len(),
                limit: first.len(),
            })
        );
        assert!(!byte_limited.is_terminal());
        let mut output = Vec::new();
        while let Some(front) = writer.front() {
            let count = front.len();
            output.extend_from_slice(front);
            writer.advance(count).unwrap();
        }
        assert_eq!(output, first);
        assert_eq!(writer.advance(1), Err(PacketWriteQueueError::NoQueuedFrame));
    }

    #[test]
    fn writer_batch_preflights_byte_limit_without_queuing_a_prefix() {
        let first = CODEC.encode(1, b"one").unwrap();
        let second = CODEC.encode(2, b"two").unwrap();
        let third = CODEC.encode(3, b"tri").unwrap();
        let mut writer = PacketWriteQueue::new(CODEC, first.len() + second.len(), 3).unwrap();
        writer.enqueue(first.clone()).unwrap();

        assert_eq!(
            writer.enqueue_batch(vec![second.clone(), third.clone()]),
            Err(PacketWriteQueueError::ByteLimitExceeded {
                queued: first.len(),
                incoming: second.len() + third.len(),
                limit: first.len() + second.len(),
            })
        );
        assert_eq!(writer.queued_bytes(), first.len());
        assert_eq!(writer.queued_frames(), 1);
        assert_eq!(writer.front(), Some(first.as_slice()));
        assert!(!writer.is_terminal());

        let remaining = writer.front().unwrap().len();
        writer.advance(remaining).unwrap();
        writer.enqueue_batch(vec![second.clone(), third]).unwrap();
        assert_eq!(writer.queued_frames(), 2);
        assert_eq!(writer.front(), Some(second.as_slice()));
    }

    #[test]
    fn writer_batch_preflights_frame_limit_and_malformed_frames() {
        let first = CODEC.encode(1, b"one").unwrap();
        let second = CODEC.encode(2, b"two").unwrap();
        let third = CODEC.encode(3, b"tri").unwrap();
        let mut writer = PacketWriteQueue::new(CODEC, 128, 2).unwrap();
        writer.enqueue(first.clone()).unwrap();

        assert_eq!(
            writer.enqueue_batch(vec![second.clone(), third]),
            Err(PacketWriteQueueError::FrameLimitExceeded { limit: 2 })
        );
        assert_eq!(writer.queued_bytes(), first.len());
        assert_eq!(writer.front(), Some(first.as_slice()));

        let remaining = writer.front().unwrap().len();
        writer.advance(remaining).unwrap();
        let malformed = vec![1, 0, 0, 4, b'x', b'y'];
        assert_eq!(
            writer.enqueue_batch(vec![second, malformed]),
            Err(PacketWriteQueueError::PacketCodec(
                PacketCodecError::TrailingBytes {
                    expected: 5,
                    actual: 6,
                }
            ))
        );
        assert_eq!(writer.queued_bytes(), 0);
        assert_eq!(writer.front(), None);
        assert!(!writer.is_terminal());
    }

    #[test]
    fn writer_batch_stops_consuming_after_the_frame_limit() {
        let frame = CODEC.encode(1, b"one").unwrap();
        let mut writer = PacketWriteQueue::new(CODEC, frame.len() * 4, 2).unwrap();
        let mut yielded = 0;
        let frames = std::iter::from_fn(|| {
            assert!(
                yielded < 3,
                "batch iterator was consumed past the frame limit"
            );
            yielded += 1;
            Some(frame.clone())
        });

        assert_eq!(
            writer.enqueue_batch(frames),
            Err(PacketWriteQueueError::FrameLimitExceeded { limit: 2 })
        );
        assert_eq!(yielded, 3);
        assert_eq!(writer.queued_bytes(), 0);
        assert_eq!(writer.queued_frames(), 0);
        assert_eq!(writer.front(), None);
        assert!(!writer.is_terminal());
    }

    #[test]
    fn writer_batch_stops_consuming_after_the_byte_limit() {
        let frame = CODEC.encode(1, b"one").unwrap();
        let mut writer = PacketWriteQueue::new(CODEC, frame.len() * 2, 8).unwrap();
        let mut yielded = 0;
        let frames = std::iter::from_fn(|| {
            assert!(
                yielded < 3,
                "batch iterator was consumed past the byte limit"
            );
            yielded += 1;
            Some(frame.clone())
        });

        assert_eq!(
            writer.enqueue_batch(frames),
            Err(PacketWriteQueueError::ByteLimitExceeded {
                queued: 0,
                incoming: frame.len() * 3,
                limit: frame.len() * 2,
            })
        );
        assert_eq!(yielded, 3);
        assert_eq!(writer.queued_bytes(), 0);
        assert_eq!(writer.queued_frames(), 0);
        assert_eq!(writer.front(), None);
        assert!(!writer.is_terminal());
    }

    #[test]
    fn writer_batch_accepts_an_empty_batch_without_changing_the_queue() {
        let mut writer = PacketWriteQueue::new(CODEC, 128, 2).unwrap();
        writer.enqueue_payload(1, b"pending").unwrap();
        let queued_bytes = writer.queued_bytes();

        writer.enqueue_batch(Vec::new()).unwrap();
        assert_eq!(writer.queued_bytes(), queued_bytes);
        assert_eq!(writer.queued_frames(), 1);
    }

    #[test]
    fn writer_malformed_frame_is_terminal_and_reset_discards_pending_data() {
        let mut writer = PacketWriteQueue::new(CODEC, 128, 2).unwrap();
        writer.enqueue_payload(1, b"pending").unwrap();
        let malformed = vec![1, 0, 0, 1, b'x', b'e'];
        assert!(matches!(
            writer.enqueue(malformed),
            Err(PacketWriteQueueError::PacketCodec(
                PacketCodecError::TrailingBytes { .. }
            ))
        ));
        assert!(writer.is_terminal());
        assert_eq!(writer.front(), None);
        let oversized = vec![0; CODEC.max_payload_len() + 1];
        assert_eq!(
            writer.enqueue_payload(2, &oversized),
            Err(PacketWriteQueueError::Terminal)
        );
        let continuation = vec![0; MAX_PACKET_PAYLOAD_LEN];
        assert_eq!(
            writer.enqueue_payload(2, &continuation),
            Err(PacketWriteQueueError::Terminal)
        );
        writer.reset();
        assert!(!writer.is_terminal());
        assert_eq!(writer.queued_bytes(), 0);
        writer.enqueue_payload(2, b"ok").unwrap();
        assert_eq!(writer.queued_frames(), 1);
    }
}
