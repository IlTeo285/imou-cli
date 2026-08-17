use clap::ValueEnum;
use serde::Serialize;

use crate::client::ImouClient;
use crate::error::Result;

/// PTZ move operations, per controlMovePTZ.html's `operation` code table.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
    UpperLeft,
    BottomLeft,
    UpperRight,
    BottomRight,
    ZoomIn,
    ZoomOut,
    Stop,
}

impl Direction {
    fn operation_code(self) -> &'static str {
        match self {
            Direction::Up => "0",
            Direction::Down => "1",
            Direction::Left => "2",
            Direction::Right => "3",
            Direction::UpperLeft => "4",
            Direction::BottomLeft => "5",
            Direction::UpperRight => "6",
            Direction::BottomRight => "7",
            Direction::ZoomIn => "8",
            Direction::ZoomOut => "9",
            Direction::Stop => "10",
        }
    }
}

#[derive(Serialize)]
struct ControlMovePtzParams<'a> {
    #[serde(rename = "deviceId")]
    device_id: &'a str,
    #[serde(rename = "channelId")]
    channel_id: &'a str,
    operation: &'a str,
    duration: String,
}

/// Issues a PTZ move command. `duration_ms` is how long the move runs before
/// the camera auto-stops; pass `Direction::Stop` to cancel an in-progress move.
/// See https://open.imoulife.com/book/http/device/operate/controlMovePTZ.html
pub async fn r#move(
    client: &ImouClient,
    device_id: &str,
    channel_id: &str,
    direction: Direction,
    duration_ms: u64,
) -> Result<()> {
    client
        .call_action(
            "controlMovePTZ",
            ControlMovePtzParams {
                device_id,
                channel_id,
                operation: direction.operation_code(),
                duration: duration_ms.to_string(),
            },
        )
        .await
}
