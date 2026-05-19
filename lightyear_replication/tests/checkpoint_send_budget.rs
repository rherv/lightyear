use bytes::Bytes;
use lightyear_netcode::MAX_PACKET_SIZE;
use lightyear_replication::checkpoint::wrap_server_payload;
use lightyear_tick::Tick;

/// Regression coverage for send-side packet budgeting on replicon server channels 0/1.
///
/// These channels are wrapped with a Lightyear checkpoint header immediately before send.
/// If an inner Replicon payload is already near the netcode packet limit (1200 bytes),
/// wrapping can push the final datagram over budget, producing `SizeMismatch(1200, N)`.
#[test]
fn wrapping_channel_0_or_1_payload_can_overflow_netcode_budget() {
    // Current checkpoint wrapper adds: 2-byte magic + 1-byte version + 4-byte tick.
    let pre_wrap_len = MAX_PACKET_SIZE - 6;
    let wrapped = wrap_server_payload(Tick(123), Bytes::from(vec![0_u8; pre_wrap_len]));

    // Near-limit payload fits before wrapping, but overflows afterwards.
    assert_eq!(pre_wrap_len, 1194);
    assert_eq!(wrapped.len(), 1201);
    assert!(wrapped.len() > MAX_PACKET_SIZE);
}

#[test]
fn payload_that_accounts_for_wrapper_stays_within_netcode_budget() {
    // Leave exactly enough room for the wrapper overhead.
    let pre_wrap_len = MAX_PACKET_SIZE - 7;
    let wrapped = wrap_server_payload(Tick(456), Bytes::from(vec![0_u8; pre_wrap_len]));

    assert_eq!(wrapped.len(), MAX_PACKET_SIZE);
}
