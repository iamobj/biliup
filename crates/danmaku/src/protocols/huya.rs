//! Huya live danmaku protocol implementation.
//!
//! Huya uses TARS binary protocol over WebSocket:
//! - Registration: WSUserInfo wrapped in WebSocketCommand (iCmdType=1)
//! - Messages: WebSocketCommand with iCmdType=7 for push messages
//! - Danmaku messages have message type 1400

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use tracing::debug;

use crate::codec::tars::{TarsInputStream, TarsOutputStream};
use crate::error::{DanmakuError, Result};
use crate::message::{ChatMessage, DEFAULT_COLOR, DanmakuEvent};
use crate::protocols::{
    ConnectionInfo, DecodeResult, HeartbeatConfig, Platform, PlatformContext, RegistrationData,
};

/// WebSocket URL for Huya danmaku.
const WSS_URL: &str = "wss://cdnws.api.huya.com/";

/// Timeout used by the historical Python implementation for loading the room page.
const ROOM_PAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Python generated this once when the Huya module was imported and reused it for
/// both the room page request and the WebSocket handshake.
static USER_AGENT_STRING: LazyLock<String> = LazyLock::new(|| {
    let chrome_version = rand::thread_rng().gen_range(100..=120);
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{chrome_version}.0.0.0 Safari/537.36"
    )
});

static ROOM_UID_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"uid['\"]*:\s*['\"]*(\d+)['\"]*"#).expect("valid Huya UID regex")
});

/// WebSocket command types.
#[allow(dead_code)]
mod cmd_type {
    pub const REGISTER_REQ: i32 = 1;
    pub const REGISTER_RSP: i32 = 2;
    pub const HEARTBEAT: i32 = 5;
    pub const HEARTBEAT_ACK: i32 = 6;
    pub const MSG_PUSH_REQ: i32 = 7;
}

/// Heartbeat packet (pre-encoded TARS).
const HEARTBEAT: &[u8] = &[
    0x00, 0x03, 0x1d, 0x00, 0x00, 0x69, 0x00, 0x00, 0x00, 0x69, 0x10, 0x03, 0x2c, 0x3c, 0x4c, 0x56,
    0x08, 0x6f, 0x6e, 0x6c, 0x69, 0x6e, 0x65, 0x75, 0x69, 0x66, 0x0f, 0x4f, 0x6e, 0x55, 0x73, 0x65,
    0x72, 0x48, 0x65, 0x61, 0x72, 0x74, 0x42, 0x65, 0x61, 0x74, 0x7d, 0x00, 0x00, 0x3c, 0x08, 0x00,
    0x01, 0x06, 0x04, 0x74, 0x52, 0x65, 0x71, 0x1d, 0x00, 0x00, 0x2f, 0x0a, 0x0a, 0x0c, 0x16, 0x00,
    0x26, 0x00, 0x36, 0x07, 0x61, 0x64, 0x72, 0x5f, 0x77, 0x61, 0x70, 0x46, 0x00, 0x0b, 0x12, 0x03,
    0xae, 0xf0, 0x0f, 0x22, 0x03, 0xae, 0xf0, 0x0f, 0x3c, 0x42, 0x6d, 0x52, 0x02, 0x60, 0x5c, 0x60,
    0x01, 0x7c, 0x82, 0x00, 0x0b, 0xb0, 0x1f, 0x9c, 0xac, 0x0b, 0x8c, 0x98, 0x0c, 0xa8, 0x0c, 0x20,
];

/// Huya live danmaku protocol.
pub struct Huya {
    client: reqwest::Client,
}

impl Huya {
    /// Create a new Huya protocol handler.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Build default headers.
    fn default_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(USER_AGENT_STRING.as_str())
                .expect("generated Huya user agent is a valid header value"),
        );
        headers
    }

    /// Extract room ID from URL.
    fn extract_room_id(url: &str) -> Option<String> {
        // https://www.huya.com/123456 or https://huya.com/roomname
        let re = Regex::new(r"huya\.com/([^/?]+)").ok()?;
        re.captures(url)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    }

    /// Extract the presenter UID used by the danmaku registration packet.
    fn extract_room_uid(room_page: &str) -> Option<u64> {
        ROOM_UID_REGEX
            .captures(room_page)
            .and_then(|captures| captures.get(1))
            .and_then(|value| value.as_str().parse().ok())
    }

    /// Get UID from room page.
    async fn get_room_uid(&self, room_id: &str) -> Result<u64> {
        let url = format!("https://www.huya.com/{}", room_id);
        let headers = Self::default_headers();

        let resp = self
            .client
            .get(&url)
            .headers(headers)
            .timeout(ROOM_PAGE_TIMEOUT)
            .send()
            .await?
            .text()
            .await?;

        Self::extract_room_uid(&resp)
            .ok_or_else(|| DanmakuError::Decode("Failed to extract UID from Huya page".to_string()))
    }

    /// Build WSUserInfo TARS structure.
    fn build_ws_user_info(uid: u64) -> Vec<u8> {
        let mut oos = TarsOutputStream::new();

        // WSUserInfo fields:
        // 0: lUid (int64)
        // 1: bAnonymous (bool)
        // 2: sGuid (string)
        // 3: sToken (string)
        // 4: lTid (int64)
        // 5: lSid (int64)
        // 6: lGroupId (int64)
        // 7: lGroupType (int64)

        oos.write_int64(0, uid as i64);
        oos.write_bool(1, false); // Not anonymous
        oos.write_string(2, ""); // sGuid
        oos.write_string(3, ""); // sToken
        oos.write_int64(4, 0); // lTid
        oos.write_int64(5, 0); // lSid
        oos.write_int64(6, uid as i64); // lGroupId = uid
        oos.write_int64(7, 3); // lGroupType = 3

        oos.get_buffer().to_vec()
    }

    /// Build WebSocketCommand TARS structure.
    fn build_ws_command(cmd_type: i32, data: &[u8]) -> Vec<u8> {
        let mut oos = TarsOutputStream::new();

        // WebSocketCommand fields:
        // 0: iCmdType (int32)
        // 1: vData (bytes)

        oos.write_int32(0, cmd_type);
        oos.write_bytes(1, data);

        oos.get_buffer().to_vec()
    }

    /// Parse a WebSocket message.
    fn parse_message(data: &[u8]) -> Result<Vec<DanmakuEvent>> {
        let mut events = Vec::new();

        // Parse WebSocketCommand
        let mut ios = TarsInputStream::new(data);

        let cmd_type = ios.read_int32(0).unwrap_or(0);

        if cmd_type == cmd_type::MSG_PUSH_REQ {
            // Read vData (bytes at tag 1)
            let inner_data = ios.read_bytes(1).ok_or_else(|| {
                DanmakuError::Decode("Huya push command is missing vData".to_string())
            })?;

            // Parse inner message
            let mut inner_ios = TarsInputStream::new(&inner_data);

            // Check message type at tag 1
            let msg_type = inner_ios.read_int64(1).unwrap_or(0);

            if msg_type == 1400 {
                // Danmaku message - read message body at tag 2
                let msg_data = inner_ios.read_bytes(2).ok_or_else(|| {
                    DanmakuError::Decode("Huya danmaku push is missing message body".to_string())
                })?;
                if let Some(event) = Self::parse_danmaku(&msg_data)? {
                    events.push(event);
                }
            }
        } else if cmd_type == cmd_type::REGISTER_RSP {
            debug!("Huya register response received");
        } else if cmd_type == cmd_type::HEARTBEAT_ACK {
            debug!("Huya heartbeat ack received");
        }

        Ok(events)
    }

    /// Parse a danmaku message.
    fn parse_danmaku(data: &[u8]) -> Result<Option<DanmakuEvent>> {
        let mut ios = TarsInputStream::new(data);

        let name = ios
            .read_struct(0, |user| user.read_string(2))
            .ok_or_else(|| {
                DanmakuError::Decode("Huya danmaku user name is missing or invalid".to_string())
            })?;
        let content = ios.read_string(3).ok_or_else(|| {
            DanmakuError::Decode("Huya danmaku content is missing or invalid".to_string())
        })?;
        let raw_color = ios
            .read_struct(6, |color| color.read_int32(0))
            .ok_or_else(|| {
                DanmakuError::Decode("Huya danmaku color is missing or invalid".to_string())
            })?;

        let color = match raw_color {
            -1 => DEFAULT_COLOR,
            value if value >= 0 => value as u32,
            value => {
                return Err(DanmakuError::Decode(format!(
                    "Huya danmaku color is invalid: {value}"
                )));
            }
        };

        // The Python implementation filtered on user name rather than content.
        if name.is_empty() {
            return Ok(None);
        }

        let chat = ChatMessage::new(content).with_color(color).with_name(name);

        Ok(Some(DanmakuEvent::Chat(chat)))
    }
}

impl Default for Huya {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Platform for Huya {
    fn name(&self) -> &'static str {
        "Huya"
    }

    async fn get_connection_info(
        &self,
        url: &str,
        context: &PlatformContext,
    ) -> Result<ConnectionInfo> {
        // Get room ID
        let room_id = if let Some(ref id) = context.room_id {
            id.clone()
        } else {
            Self::extract_room_id(url)
                .ok_or_else(|| DanmakuError::Decode("Invalid Huya URL".to_string()))?
        };

        // Get UID from room page
        let uid = self.get_room_uid(&room_id).await?;
        debug!("Huya room UID: {}", uid);

        // Build registration packet
        let user_info = Self::build_ws_user_info(uid);
        let reg_packet = Self::build_ws_command(cmd_type::REGISTER_REQ, &user_info);

        Ok(ConnectionInfo::new(WSS_URL)
            .with_registration(vec![RegistrationData::Binary(reg_packet)])
            .with_headers(Self::default_headers()))
    }

    fn heartbeat_config(&self) -> HeartbeatConfig {
        HeartbeatConfig::binary(HEARTBEAT.to_vec(), Duration::from_secs(60))
    }

    fn heartbeat_initial_delay(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn decode_message(&self, msg: &[u8]) -> Result<DecodeResult> {
        let events = Self::parse_message(msg)?;
        Ok(DecodeResult::with_events(events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYTHON_WS_USER_INFO: &str = "0130391c260036004c5c6130397003";
    const PYTHON_REGISTER_PACKET: &str = "00011d00000f0130391c260036004c5c6130397003";
    const PYTHON_DANMAKU_PACKET: &str =
        "00071d00001e1105782d0000170a2605616c6963650b360568656c6c6f6a02001122330b";
    const PYTHON_DEFAULT_COLOR_PACKET: &str =
        "00071d00001b1105782d0000140a2605616c6963650b360568656c6c6f6a00ff0b";
    const PYTHON_EMPTY_NAME_PACKET: &str =
        "00071d0000191105782d0000120a26000b360568656c6c6f6a02001122330b";
    const PYTHON_EMPTY_CONTENT_PACKET: &str =
        "00071d0000191105782d0000120a2605616c6963650b36006a02001122330b";

    fn hex_decode(input: &str) -> Vec<u8> {
        (0..input.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&input[index..index + 2], 16).unwrap())
            .collect()
    }

    fn decode_single_chat(packet: &str) -> ChatMessage {
        let events = Huya::parse_message(&hex_decode(packet)).unwrap();
        assert_eq!(events.len(), 1);
        let DanmakuEvent::Chat(chat) = events.into_iter().next().unwrap() else {
            panic!("expected Huya chat event");
        };
        chat
    }

    #[test]
    fn test_extract_room_id() {
        assert_eq!(
            Huya::extract_room_id("https://www.huya.com/123456"),
            Some("123456".to_string())
        );
        assert_eq!(
            Huya::extract_room_id("https://huya.com/kpl"),
            Some("kpl".to_string())
        );
    }

    #[test]
    fn test_extract_room_uid() {
        assert_eq!(Huya::extract_room_uid(r#""uid":"123456""#), Some(123456));
        assert_eq!(Huya::extract_room_uid("uid: 654321"), Some(654321));
        assert_eq!(Huya::extract_room_uid("no presenter here"), None);
    }

    #[test]
    fn test_default_headers_reuse_python_style_user_agent() {
        let first = Huya::default_headers();
        let second = Huya::default_headers();
        let user_agent = first.get(USER_AGENT).unwrap().to_str().unwrap();
        let pattern = Regex::new(
            r"^Mozilla/5\.0 \(Windows NT 10\.0; Win64; x64\) AppleWebKit/537\.36 \(KHTML, like Gecko\) Chrome/(10\d|11\d|120)\.0\.0\.0 Safari/537\.36$",
        )
        .unwrap();

        assert!(pattern.is_match(user_agent));
        assert_eq!(first.get(USER_AGENT), second.get(USER_AGENT));
    }

    #[test]
    fn test_build_ws_user_info() {
        let data = Huya::build_ws_user_info(12345);
        assert_eq!(data, hex_decode(PYTHON_WS_USER_INFO));

        // Verify structure
        let mut ios = TarsInputStream::new(&data);
        assert_eq!(ios.read_int64(0), Some(12345));
    }

    #[test]
    fn test_build_ws_command() {
        let user_info = Huya::build_ws_user_info(12345);
        let cmd = Huya::build_ws_command(cmd_type::REGISTER_REQ, &user_info);

        assert_eq!(cmd, hex_decode(PYTHON_REGISTER_PACKET));

        // Verify structure
        let mut ios = TarsInputStream::new(&cmd);
        assert_eq!(ios.read_int32(0), Some(cmd_type::REGISTER_REQ));
    }

    #[test]
    fn test_decode_matches_python_nested_tars_behavior() {
        let chat = decode_single_chat(PYTHON_DANMAKU_PACKET);

        assert_eq!(chat.name.as_deref(), Some("alice"));
        assert_eq!(chat.content, "hello");
        assert_eq!(chat.color, 0x112233);
        assert_eq!(chat.uid, None);
    }

    #[test]
    fn test_decode_maps_python_minus_one_color_to_white() {
        let chat = decode_single_chat(PYTHON_DEFAULT_COLOR_PACKET);

        assert_eq!(chat.color, DEFAULT_COLOR);
    }

    #[test]
    fn test_decode_filters_empty_name_like_python() {
        let events = Huya::parse_message(&hex_decode(PYTHON_EMPTY_NAME_PACKET)).unwrap();

        assert!(events.is_empty());
    }

    #[test]
    fn test_decode_keeps_empty_content_like_python() {
        let chat = decode_single_chat(PYTHON_EMPTY_CONTENT_PACKET);

        assert_eq!(chat.name.as_deref(), Some("alice"));
        assert!(chat.content.is_empty());
    }

    #[test]
    fn test_decode_ignores_non_danmaku_push() {
        let mut inner = TarsOutputStream::new();
        inner.write_int64(1, 1401);
        inner.write_bytes(2, b"");
        let packet = Huya::build_ws_command(cmd_type::MSG_PUSH_REQ, inner.get_buffer());

        assert!(Huya::parse_message(&packet).unwrap().is_empty());
    }

    #[test]
    fn test_decode_rejects_malformed_struct_and_invalid_utf8() {
        assert!(Huya::parse_danmaku(&[0x06, 0x00]).is_err());
        assert!(Huya::parse_danmaku(&[0x0a, 0x26, 0x01, 0xff, 0x0b]).is_err());
    }

    #[test]
    fn test_heartbeat_matches_python_timing() {
        let huya = Huya::new();

        assert_eq!(huya.heartbeat_config().interval, Duration::from_secs(60));
        assert_eq!(huya.heartbeat_initial_delay(), Duration::from_secs(60));
    }
}
