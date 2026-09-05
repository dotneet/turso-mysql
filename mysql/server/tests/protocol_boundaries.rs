use turso_mysql_server::{PacketCodec, PacketSequence, PACKET_HEADER_LEN};

#[test]
fn packet_sequence_next_id_wraps_after_ff() {
    let mut sequence = PacketSequence::new(254);

    assert_eq!(sequence.next_sequence_id(), 254);
    assert_eq!(sequence.next_sequence_id(), 255);
    assert_eq!(sequence.next_sequence_id(), 0);
    assert_eq!(sequence.next_sequence_id(), 1);
    assert_eq!(sequence.expected(), 2);
}

#[test]
fn packet_codec_round_trips_three_byte_payload_boundaries() {
    let codec = PacketCodec::new(0x1_0001).unwrap();

    for length in [0xff, 0x100, 0xffff, 0x1_0000, 0x1_0001] {
        let payload = vec![0xa5; length];
        let frame = codec.encode(0x7e, &payload).unwrap();

        assert_eq!(
            &frame[..PACKET_HEADER_LEN],
            &[
                length as u8,
                (length >> 8) as u8,
                (length >> 16) as u8,
                0x7e,
            ]
        );
        assert_eq!(codec.decode(&frame).unwrap().payload, payload.as_slice());
    }
}
