//! Minimal Huya WUP/TARS client for getCdnTokenInfoEx.

use super::{LiveError, LiveResult};
use rand::Rng;
use reqwest::Client;
use std::collections::BTreeMap;

pub const HUYA_WUP_BASE_URL: &str = "https://wup.huya.com";
pub const HUYA_WUP_YST_URL: &str = "https://snmhuya.yst.aisee.tv";
pub const HUYA_WEB_BASE_URL: &str = "https://www.huya.com";
pub const WUP_UA: &str =
    "HYSDK(Windows,30000002)_APP(pc_exe&7030003&official)_SDK(trans&2.29.0.5493)";

const DEFAULT_TICKET_NUMBER: i32 = -1;
const SERVANT: &str = "liveui";
const FUNC: &str = "getCdnTokenInfoEx";

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TarsType {
    Int8 = 0,
    Int16 = 1,
    Int32 = 2,
    Int64 = 3,
    String1 = 6,
    String4 = 7,
    Map = 8,
    List = 9,
    StructBegin = 10,
    StructEnd = 11,
    Zero = 12,
    Bytes = 13,
}

impl TarsType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Int8),
            1 => Some(Self::Int16),
            2 => Some(Self::Int32),
            3 => Some(Self::Int64),
            6 => Some(Self::String1),
            7 => Some(Self::String4),
            8 => Some(Self::Map),
            9 => Some(Self::List),
            10 => Some(Self::StructBegin),
            11 => Some(Self::StructEnd),
            12 => Some(Self::Zero),
            13 => Some(Self::Bytes),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct TarsOutputStream {
    buffer: Vec<u8>,
}

impl TarsOutputStream {
    fn new() -> Self {
        Self::default()
    }

    fn get_buffer(&self) -> &[u8] {
        &self.buffer
    }

    fn into_buffer(self) -> Vec<u8> {
        self.buffer
    }

    fn write_head(&mut self, tag: u8, tars_type: TarsType) {
        if tag < 15 {
            self.buffer.push((tag << 4) | (tars_type as u8));
        } else {
            self.buffer.push(0xF0 | (tars_type as u8));
            self.buffer.push(tag);
        }
    }

    fn write_int8(&mut self, tag: u8, value: i8) {
        if value == 0 {
            self.write_head(tag, TarsType::Zero);
        } else {
            self.write_head(tag, TarsType::Int8);
            self.buffer.push(value as u8);
        }
    }

    fn write_int16(&mut self, tag: u8, value: i16) {
        if (-128..=127).contains(&value) {
            self.write_int8(tag, value as i8);
        } else {
            self.write_head(tag, TarsType::Int16);
            self.buffer.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn write_int32(&mut self, tag: u8, value: i32) {
        if (-32768..=32767).contains(&value) {
            self.write_int16(tag, value as i16);
        } else {
            self.write_head(tag, TarsType::Int32);
            self.buffer.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn write_int64(&mut self, tag: u8, value: i64) {
        if (i32::MIN as i64..=i32::MAX as i64).contains(&value) {
            self.write_int32(tag, value as i32);
        } else {
            self.write_head(tag, TarsType::Int64);
            self.buffer.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn write_string(&mut self, tag: u8, value: &str) {
        let bytes = value.as_bytes();
        if bytes.len() <= 255 {
            self.write_head(tag, TarsType::String1);
            self.buffer.push(bytes.len() as u8);
        } else {
            self.write_head(tag, TarsType::String4);
            self.buffer
                .extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        }
        self.buffer.extend_from_slice(bytes);
    }

    fn write_bytes(&mut self, tag: u8, value: &[u8]) {
        self.write_head(tag, TarsType::Bytes);
        self.write_head(0, TarsType::Int8);
        self.write_int32(0, value.len() as i32);
        self.buffer.extend_from_slice(value);
    }

    fn write_struct_begin(&mut self, tag: u8) {
        self.write_head(tag, TarsType::StructBegin);
    }

    fn write_struct_end(&mut self) {
        self.write_head(0, TarsType::StructEnd);
    }

    fn write_map_string_bytes(&mut self, tag: u8, value: &BTreeMap<String, Vec<u8>>) {
        self.write_head(tag, TarsType::Map);
        self.write_int32(0, value.len() as i32);
        for (key, bytes) in value {
            self.write_string(0, key);
            self.write_bytes(1, bytes);
        }
    }

    fn write_map_string_string(&mut self, tag: u8, value: &BTreeMap<String, String>) {
        self.write_head(tag, TarsType::Map);
        self.write_int32(0, value.len() as i32);
        for (key, val) in value {
            self.write_string(0, key);
            self.write_string(1, val);
        }
    }

    fn write_user_id(&mut self, tag: u8, s_huya_ua: &str) {
        self.write_struct_begin(tag);
        self.write_int64(0, 0);
        self.write_string(1, "");
        self.write_string(2, "");
        self.write_string(3, s_huya_ua);
        self.write_string(4, "");
        self.write_int32(5, 0);
        self.write_string(6, "");
        self.write_string(7, "");
        self.write_struct_end();
    }

    fn write_cdn_token_ex_req(&mut self, tag: u8, stream_name: &str, s_huya_ua: &str) {
        self.write_struct_begin(tag);
        self.write_string(0, "");
        self.write_string(1, stream_name);
        self.write_int32(2, 0);
        self.write_user_id(3, s_huya_ua);
        self.write_int32(4, 66);
        self.write_struct_end();
    }

    fn write_request_packet(
        &mut self,
        version: i16,
        request_id: i32,
        servant: &str,
        func: &str,
        s_buffer: &[u8],
    ) {
        self.write_int16(1, version);
        self.write_int8(2, 0);
        self.write_int32(3, 0);
        self.write_int32(4, request_id);
        self.write_string(5, servant);
        self.write_string(6, func);
        self.write_bytes(7, s_buffer);
        self.write_int32(8, 0);
        self.write_map_string_string(9, &BTreeMap::new());
        self.write_map_string_string(10, &BTreeMap::new());
    }
}

struct TarsInputStream<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> TarsInputStream<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn peek_head(&self) -> Option<(u8, TarsType)> {
        if self.pos >= self.data.len() {
            return None;
        }
        let byte = self.data[self.pos];
        let mut tag = (byte >> 4) & 0x0F;
        let type_id = byte & 0x0F;
        if tag >= 15 {
            if self.pos + 1 >= self.data.len() {
                return None;
            }
            tag = self.data[self.pos + 1];
        }
        TarsType::from_u8(type_id).map(|t| (tag, t))
    }

    fn read_head(&mut self) -> Option<(u8, TarsType)> {
        if self.pos >= self.data.len() {
            return None;
        }
        let byte = self.data[self.pos];
        let mut tag = (byte >> 4) & 0x0F;
        let type_id = byte & 0x0F;
        self.pos += 1;
        if tag >= 15 {
            if self.pos >= self.data.len() {
                return None;
            }
            tag = self.data[self.pos];
            self.pos += 1;
        }
        TarsType::from_u8(type_id).map(|t| (tag, t))
    }

    fn skip_field(&mut self, tars_type: TarsType) -> Option<()> {
        match tars_type {
            TarsType::Int8 => {
                self.pos += 1;
                Some(())
            }
            TarsType::Int16 => {
                self.pos += 2;
                Some(())
            }
            TarsType::Int32 => {
                self.pos += 4;
                Some(())
            }
            TarsType::Int64 => {
                self.pos += 8;
                Some(())
            }
            TarsType::String1 => {
                let len = *self.data.get(self.pos)? as usize;
                self.pos += 1 + len;
                Some(())
            }
            TarsType::String4 => {
                if self.pos + 4 > self.data.len() {
                    return None;
                }
                let len = u32::from_be_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                ]) as usize;
                self.pos += 4 + len;
                Some(())
            }
            TarsType::Map => {
                let size = self.read_int32_internal()? as usize;
                for _ in 0..size * 2 {
                    let (_, t) = self.read_head()?;
                    self.skip_field(t)?;
                }
                Some(())
            }
            TarsType::List => {
                let size = self.read_int32_internal()? as usize;
                for _ in 0..size {
                    let (_, t) = self.read_head()?;
                    self.skip_field(t)?;
                }
                Some(())
            }
            TarsType::Bytes => {
                self.read_head()?;
                let size = self.read_int32_internal()? as usize;
                self.pos += size;
                Some(())
            }
            TarsType::StructBegin => self.skip_to_struct_end(),
            TarsType::StructEnd | TarsType::Zero => Some(()),
        }
    }

    fn skip_to_struct_end(&mut self) -> Option<()> {
        loop {
            let (_, tars_type) = self.read_head()?;
            if tars_type == TarsType::StructEnd {
                return Some(());
            }
            self.skip_field(tars_type)?;
        }
    }

    fn skip_to_tag(&mut self, target_tag: u8) -> bool {
        while self.pos < self.data.len() {
            let Some((tag, tars_type)) = self.peek_head() else {
                break;
            };
            if tars_type == TarsType::StructEnd {
                return false;
            }
            if tag == target_tag {
                return true;
            }
            if tag > target_tag {
                return false;
            }
            let _ = self.read_head();
            if self.skip_field(tars_type).is_none() {
                return false;
            }
        }
        false
    }

    fn read_int32_internal(&mut self) -> Option<i32> {
        let (_, tars_type) = self.read_head()?;
        match tars_type {
            TarsType::Zero => Some(0),
            TarsType::Int8 => {
                let v = *self.data.get(self.pos)? as i8 as i32;
                self.pos += 1;
                Some(v)
            }
            TarsType::Int16 => {
                if self.pos + 2 > self.data.len() {
                    return None;
                }
                let v = i16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
                self.pos += 2;
                Some(v as i32)
            }
            TarsType::Int32 => {
                if self.pos + 4 > self.data.len() {
                    return None;
                }
                let v = i32::from_be_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                ]);
                self.pos += 4;
                Some(v)
            }
            _ => None,
        }
    }

    fn read_int64_at(&mut self, tag: u8) -> Option<i64> {
        if !self.skip_to_tag(tag) {
            return None;
        }
        let (_, tars_type) = self.read_head()?;
        match tars_type {
            TarsType::Zero => Some(0),
            TarsType::Int8 => {
                let v = *self.data.get(self.pos)? as i8 as i64;
                self.pos += 1;
                Some(v)
            }
            TarsType::Int16 => {
                if self.pos + 2 > self.data.len() {
                    return None;
                }
                let v = i16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
                self.pos += 2;
                Some(v as i64)
            }
            TarsType::Int32 => {
                if self.pos + 4 > self.data.len() {
                    return None;
                }
                let v = i32::from_be_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                ]);
                self.pos += 4;
                Some(v as i64)
            }
            TarsType::Int64 => {
                if self.pos + 8 > self.data.len() {
                    return None;
                }
                let v = i64::from_be_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                    self.data[self.pos + 4],
                    self.data[self.pos + 5],
                    self.data[self.pos + 6],
                    self.data[self.pos + 7],
                ]);
                self.pos += 8;
                Some(v)
            }
            _ => None,
        }
    }

    fn read_string_at(&mut self, tag: u8) -> Option<String> {
        if !self.skip_to_tag(tag) {
            return None;
        }
        let (_, tars_type) = self.read_head()?;
        match tars_type {
            TarsType::String1 => {
                let len = *self.data.get(self.pos)? as usize;
                self.pos += 1;
                if self.pos + len > self.data.len() {
                    return None;
                }
                let s = String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).to_string();
                self.pos += len;
                Some(s)
            }
            TarsType::String4 => {
                if self.pos + 4 > self.data.len() {
                    return None;
                }
                let len = u32::from_be_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                ]) as usize;
                self.pos += 4;
                if self.pos + len > self.data.len() {
                    return None;
                }
                let s = String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).to_string();
                self.pos += len;
                Some(s)
            }
            _ => None,
        }
    }

    fn read_bytes_at(&mut self, tag: u8) -> Option<Vec<u8>> {
        if !self.skip_to_tag(tag) {
            return None;
        }
        let (_, tars_type) = self.read_head()?;
        if tars_type != TarsType::Bytes {
            return None;
        }
        self.read_head()?;
        let size = self.read_int32_internal()? as usize;
        if self.pos + size > self.data.len() {
            return None;
        }
        let bytes = self.data[self.pos..self.pos + size].to_vec();
        self.pos += size;
        Some(bytes)
    }

    fn read_map_string_bytes_at(&mut self, tag: u8) -> Option<BTreeMap<String, Vec<u8>>> {
        if !self.skip_to_tag(tag) {
            return None;
        }
        let (_, tars_type) = self.read_head()?;
        if tars_type != TarsType::Map {
            return None;
        }
        let size = self.read_int32_internal()? as usize;
        let mut map = BTreeMap::new();
        for _ in 0..size {
            let key = {
                let (_, key_type) = self.read_head()?;
                match key_type {
                    TarsType::String1 => {
                        let len = *self.data.get(self.pos)? as usize;
                        self.pos += 1;
                        if self.pos + len > self.data.len() {
                            return None;
                        }
                        let s =
                            String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).to_string();
                        self.pos += len;
                        s
                    }
                    TarsType::String4 => {
                        if self.pos + 4 > self.data.len() {
                            return None;
                        }
                        let len = u32::from_be_bytes([
                            self.data[self.pos],
                            self.data[self.pos + 1],
                            self.data[self.pos + 2],
                            self.data[self.pos + 3],
                        ]) as usize;
                        self.pos += 4;
                        if self.pos + len > self.data.len() {
                            return None;
                        }
                        let s =
                            String::from_utf8_lossy(&self.data[self.pos..self.pos + len]).to_string();
                        self.pos += len;
                        s
                    }
                    _ => return None,
                }
            };

            let (_, value_type) = self.read_head()?;
            if value_type != TarsType::Bytes {
                return None;
            }
            self.read_head()?;
            let size = self.read_int32_internal()? as usize;
            if self.pos + size > self.data.len() {
                return None;
            }
            let bytes = self.data[self.pos..self.pos + size].to_vec();
            self.pos += size;
            map.insert(key, bytes);
        }
        Some(map)
    }
}

fn encode_wup_request(stream_name: &str, s_huya_ua: &str, request_id: i32) -> Vec<u8> {
    let mut req_body = TarsOutputStream::new();
    req_body.write_cdn_token_ex_req(0, stream_name, s_huya_ua);

    let mut attrs = BTreeMap::new();
    attrs.insert("tReq".to_string(), req_body.into_buffer());

    let mut attr_stream = TarsOutputStream::new();
    attr_stream.write_map_string_bytes(0, &attrs);

    let mut packet = TarsOutputStream::new();
    packet.write_request_packet(3, request_id, SERVANT, FUNC, attr_stream.get_buffer());
    let packet_bytes = packet.into_buffer();

    let mut framed = Vec::with_capacity(4 + packet_bytes.len());
    framed.extend_from_slice(&((4 + packet_bytes.len()) as i32).to_be_bytes());
    framed.extend_from_slice(&packet_bytes);
    framed
}

fn decode_wup_flv_token(buf: &[u8]) -> LiveResult<String> {
    if buf.len() < 4 {
        return Err(LiveError::custom("虎牙 WUP 响应过短"));
    }
    let mut packet_stream = TarsInputStream::new(&buf[4..]);
    let s_buffer = packet_stream
        .read_bytes_at(7)
        .ok_or_else(|| LiveError::custom("虎牙 WUP 响应缺少 sBuffer"))?;

    let mut attr_stream = TarsInputStream::new(&s_buffer);
    let attrs = attr_stream
        .read_map_string_bytes_at(0)
        .ok_or_else(|| LiveError::custom("虎牙 WUP 响应属性解析失败"))?;
    let rsp_bytes = attrs
        .get("tRsp")
        .ok_or_else(|| LiveError::custom("虎牙 WUP 响应缺少 tRsp"))?;

    let mut rsp_stream = TarsInputStream::new(rsp_bytes);
    // response struct is written at tag 0
    if !rsp_stream.skip_to_tag(0) {
        return Err(LiveError::custom("虎牙 WUP token 结构无效"));
    }
    let (_, tars_type) = rsp_stream
        .read_head()
        .ok_or_else(|| LiveError::custom("虎牙 WUP token 结构无效"))?;
    if tars_type != TarsType::StructBegin {
        return Err(LiveError::custom("虎牙 WUP token 不是结构体"));
    }

    let token = rsp_stream
        .read_string_at(0)
        .ok_or_else(|| LiveError::custom("虎牙 WUP sFlvToken 为空"))?;
    let _ = rsp_stream.read_int64_at(1);
    Ok(token)
}

fn random_hyapp_ua() -> String {
    // Align with DMR UAGenerator.generate_hyapp_ua supported platforms.
    let mut rng = rand::thread_rng();
    let platforms = [
        ("adr", "13.1.0", true),
        ("ios", "13.1.0", false),
        ("nftv", "2.6.10", true),
        ("pc_exe", "7000000", false),
    ];
    let (platform, version, android_like) = platforms[rng.gen_range(0..platforms.len())];
    let mut version = version.to_string();
    if android_like {
        version.push('.');
        version.push_str(&rng.gen_range(3000..5001).to_string());
    }
    let mut ua = format!("{platform}&{version}&official");
    if android_like {
        ua.push('&');
        ua.push_str(&rng.gen_range(28..37).to_string());
    }
    ua
}

pub async fn get_cdn_token_info_ex(client: &Client, stream_name: &str) -> LiveResult<String> {
    let s_huya_ua = random_hyapp_ua();
    let request_id = DEFAULT_TICKET_NUMBER.unsigned_abs() as i32;
    let body = encode_wup_request(stream_name, &s_huya_ua, request_id);

    let use_yst = {
        let mut rng = rand::thread_rng();
        rng.gen_bool(0.5)
    };
    let url = if use_yst {
        format!("{HUYA_WUP_YST_URL}/{SERVANT}/{FUNC}")
    } else {
        HUYA_WUP_BASE_URL.to_string()
    };

    let response = client
        .post(url)
        .header("User-Agent", WUP_UA)
        .header("Origin", HUYA_WEB_BASE_URL)
        .header("Referer", HUYA_WEB_BASE_URL)
        .body(body)
        .send()
        .await
        .map_err(|err| LiveError::custom(format!("请求虎牙 WUP 失败: {err}")))?;

    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| LiveError::custom(format!("读取虎牙 WUP 响应失败: {err}")))?;
    if !status.is_success() {
        return Err(LiveError::custom(format!(
            "虎牙 WUP 请求失败: HTTP {status}"
        )));
    }

    decode_wup_flv_token(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_and_decode_cdn_token_roundtrip() {
        let stream_name = "test-stream-name";
        let s_huya_ua = "pc_exe&7000000&official";
        let request = encode_wup_request(stream_name, s_huya_ua, 1);

        // Decode request attributes and ensure stream name survives.
        let mut packet_stream = TarsInputStream::new(&request[4..]);
        let s_buffer = packet_stream.read_bytes_at(7).expect("sBuffer");
        let mut attr_stream = TarsInputStream::new(&s_buffer);
        let attrs = attr_stream
            .read_map_string_bytes_at(0)
            .expect("request attrs");
        let req_bytes = attrs.get("tReq").expect("tReq");
        let mut req_stream = TarsInputStream::new(req_bytes);
        assert!(req_stream.skip_to_tag(0));
        let (_, t) = req_stream.read_head().unwrap();
        assert_eq!(t, TarsType::StructBegin);
        let decoded_stream_name = req_stream.read_string_at(1).expect("sStreamName");
        assert_eq!(decoded_stream_name, stream_name);

        // Build a synthetic response payload and decode token.
        let mut rsp_struct = TarsOutputStream::new();
        rsp_struct.write_struct_begin(0);
        rsp_struct.write_string(0, "wsSecret=abc&wsTime=1&fm=xx&fs=bgct&ctype=huya_live&t=100");
        rsp_struct.write_int64(1, 123456);
        rsp_struct.write_struct_end();

        let mut attrs = BTreeMap::new();
        attrs.insert("tRsp".to_string(), rsp_struct.into_buffer());
        let mut attr_stream = TarsOutputStream::new();
        attr_stream.write_map_string_bytes(0, &attrs);

        let mut packet = TarsOutputStream::new();
        packet.write_request_packet(3, 1, SERVANT, FUNC, attr_stream.get_buffer());
        let packet_bytes = packet.into_buffer();
        let mut framed = Vec::new();
        framed.extend_from_slice(&((4 + packet_bytes.len()) as i32).to_be_bytes());
        framed.extend_from_slice(&packet_bytes);

        let token = decode_wup_flv_token(&framed).expect("token");
        assert!(token.contains("wsSecret=abc"));
    }
}
