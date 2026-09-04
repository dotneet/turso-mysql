#![no_main]

use libfuzzer_sys::fuzz_target;
use turso_mysql_server::{
    decode_command_packet_for_fuzzing, decode_statement_execute_parameters, Packet, PacketCodec,
    PacketStreamDecoder, COMMAND_SEQUENCE_ID, COM_INIT_DB, COM_PING, COM_QUERY, COM_QUIT,
    COM_RESET_CONNECTION, COM_STMT_CLOSE, COM_STMT_EXECUTE, COM_STMT_PREPARE, COM_STMT_RESET,
    COM_STMT_SEND_LONG_DATA, MAX_AUTH_PACKET_PAYLOAD_LENGTH,
    MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH, MAX_COMMAND_PAYLOAD_LENGTH,
    MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH, MAX_PACKET_PAYLOAD_LEN, MAX_STMT_PARAMETER_COUNT,
    PACKET_HEADER_LEN,
};

const fn max_payload_length(left: usize, right: usize) -> usize {
    if left > right {
        left
    } else {
        right
    }
}

const MAX_PACKETS_PER_FEED: usize = 16;
const MAX_PROTOCOL_PAYLOAD_LENGTH: usize = max_payload_length(
    max_payload_length(
        max_payload_length(
            MAX_AUTH_PACKET_PAYLOAD_LENGTH,
            MAX_CLIENT_HANDSHAKE_RESPONSE_PAYLOAD_LENGTH,
        ),
        MAX_INITIAL_HANDSHAKE_PAYLOAD_LENGTH,
    ),
    MAX_COMMAND_PAYLOAD_LENGTH,
);
const MAX_STREAM_BUFFERED_PAYLOAD_LENGTH: usize = MAX_PROTOCOL_PAYLOAD_LENGTH;
const MAX_FUZZ_INPUT_LENGTH: usize = MAX_PROTOCOL_PAYLOAD_LENGTH + PACKET_HEADER_LEN;
const STREAM_CHUNK_SIZES: [usize; 8] = [1, 2, 3, 4, 7, 16, 31, 64];
const STRUCTURED_PARAMETER_COUNTS: [usize; 7] = [
    0,
    1,
    u8::MAX as usize,
    MAX_STMT_PARAMETER_COUNT - 1,
    MAX_STMT_PARAMETER_COUNT,
    MAX_STMT_PARAMETER_COUNT + 1,
    u16::MAX as usize,
];
const STRUCTURED_COMMANDS: [u8; 10] = [
    COM_QUERY,
    COM_INIT_DB,
    COM_PING,
    COM_QUIT,
    COM_STMT_PREPARE,
    COM_STMT_EXECUTE,
    COM_STMT_SEND_LONG_DATA,
    COM_STMT_CLOSE,
    COM_STMT_RESET,
    COM_RESET_CONNECTION,
];

const _: () = assert!(MAX_PROTOCOL_PAYLOAD_LENGTH <= MAX_PACKET_PAYLOAD_LEN);
const _: () = assert!(MAX_PACKETS_PER_FEED > 0);

fn fuzz_protocol(input: &[u8]) {
    let input = &input[..input.len().min(MAX_FUZZ_INPUT_LENGTH)];
    let codec = PacketCodec::new(MAX_PROTOCOL_PAYLOAD_LENGTH)
        .expect("the fixed fuzz codec limit must be valid");

    if let Ok(packet) = codec.decode(input) {
        let _ = decode_command_packet_for_fuzzing(packet);
    }
    let _ = codec.decode_initial_handshake(input);
    let _ = codec.decode_client_handshake_response(input);
    let _ = codec.decode_client_ssl_request(input);
    let _ = codec.decode_client_auth_response(input);
    let _ = codec.decode_auth_more_data(input);
    let _ = codec.decode_auth_switch_request(input);
    let _ = codec.decode_auth_ok(input);

    feed_stream_in_deterministic_chunks(input, codec);

    if let Some(frame) = structured_command_frame(input, codec) {
        feed_stream_in_deterministic_chunks(&frame, codec);
    }

    let parameter_count = input.get(..2).map_or(0, |bytes| {
        usize::from(u16::from_le_bytes([bytes[0], bytes[1]]))
    });
    let parameter_payload = input.get(2..).unwrap_or_default();
    let _ = decode_statement_execute_parameters(parameter_payload, parameter_count, None);

    let structured_count = structured_parameter_count(input);
    let structured_payload = input.get(1..).unwrap_or_default();
    let _ = decode_statement_execute_parameters(structured_payload, structured_count, None);
}

fn feed_stream_in_deterministic_chunks(input: &[u8], codec: PacketCodec) {
    let mut stream = PacketStreamDecoder::new(
        codec,
        MAX_STREAM_BUFFERED_PAYLOAD_LENGTH,
        MAX_PACKETS_PER_FEED,
    )
    .expect("the fixed stream decoder limits must be valid");
    let mut offset = 0;
    let mut chunk_index = 0;
    while offset < input.len() {
        let chunk_size = STREAM_CHUNK_SIZES[chunk_index % STREAM_CHUNK_SIZES.len()];
        let end = offset.saturating_add(chunk_size).min(input.len());
        if let Ok(packets) = stream.feed(&input[offset..end]) {
            for packet in packets {
                let _ = decode_command_packet_for_fuzzing(Packet {
                    sequence_id: packet.sequence_id,
                    payload: &packet.payload,
                });
            }
        }
        offset = end;
        chunk_index += 1;
    }
}

fn structured_command_frame(input: &[u8], codec: PacketCodec) -> Option<Vec<u8>> {
    let command_index = input.first().copied().map_or(0, usize::from) % STRUCTURED_COMMANDS.len();
    let body_end = input.len().min(MAX_PROTOCOL_PAYLOAD_LENGTH);
    let body = input.get(1..body_end).unwrap_or_default();
    let mut payload = Vec::with_capacity(body.len() + 1);
    payload.push(STRUCTURED_COMMANDS[command_index]);
    payload.extend_from_slice(body);
    codec.encode(COMMAND_SEQUENCE_ID, &payload).ok()
}

fn structured_parameter_count(input: &[u8]) -> usize {
    let selector = input.first().copied().map_or(0, usize::from);
    STRUCTURED_PARAMETER_COUNTS[selector % STRUCTURED_PARAMETER_COUNTS.len()]
}

fuzz_target!(|input: &[u8]| {
    fuzz_protocol(input);
});
