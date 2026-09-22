use super::TurnInputChannel;
use super::channel_from_value;

#[test]
fn channel_flag_parsing() {
    assert_eq!(channel_from_value(None), TurnInputChannel::LegacySend);
    assert_eq!(channel_from_value(Some("0")), TurnInputChannel::LegacySend);
    assert_eq!(
        channel_from_value(Some("off")),
        TurnInputChannel::LegacySend
    );
    assert_eq!(
        channel_from_value(Some("garbage")),
        TurnInputChannel::LegacySend
    );
    assert_eq!(channel_from_value(Some("1")), TurnInputChannel::V4Command);
    assert_eq!(
        channel_from_value(Some("true")),
        TurnInputChannel::V4Command
    );
    assert_eq!(
        channel_from_value(Some(" yes ")),
        TurnInputChannel::V4Command
    );
}
