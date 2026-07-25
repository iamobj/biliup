use super::{
    DanmakuSource, DownloaderHint, LiveError, LivePlugin, LiveRequest, LiveResult, LiveStatus,
    LiveStream, media_ext_from_url,
};
use super::huya_wup::{self, WUP_UA};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use md5::{Digest, Md5};
use rand::Rng;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const HUYA_WEB_BASE_URL: &str = "https://www.huya.com";
const HUYA_MP_BASE_URL: &str = "https://mp.huya.com";
const HUYA_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const PLATFORMS: &[(&str, &str)] = &[
    ("huya_pc_exe", "0"),
    ("huya_adr", "2"),
    ("huya_ios", "3"),
    ("tv_huya_nftv", "10"),
    ("huya_webh5", "100"),
    ("huya_live", "100"),
    ("tars_mp", "102"),
    ("tars_mobile", "103"),
    ("huya_liveshareh5", "104"),
];

pub struct Huya {
    re: Regex,
}

impl Default for Huya {
    fn default() -> Self {
        Self::new()
    }
}

impl Huya {
    pub fn new() -> Self {
        Self {
            re: Regex::new(r"https?://(?:(?:www|m)\.)?huya\.com").unwrap(),
        }
    }
}

#[async_trait]
impl LivePlugin for Huya {
    fn name(&self) -> &'static str {
        "Huya"
    }

    fn matches(&self, url: &str) -> bool {
        self.re.is_match(url)
    }

    async fn check_stream(&self, request: LiveRequest) -> LiveResult<LiveStatus> {
        HuyaLive::new(request).check_stream().await
    }
}

struct HuyaLive {
    client: Client,
    url: String,
    name: String,
    huya_cdn: String,
    huya_max_ratio: u32,
    huya_protocol: HuyaProtocol,
    huya_imgplus: bool,
    huya_codec: String,
    huya_danmaku: bool,
    huya_mobile_api: bool,
    huya_use_wup: bool,
}

impl HuyaLive {
    fn new(request: LiveRequest) -> Self {
        let options = request.options.huya;
        Self {
            client: request.client,
            url: request.url,
            name: request.name,
            huya_cdn: options.cdn.to_uppercase(),
            huya_max_ratio: options.max_ratio,
            huya_protocol: HuyaProtocol::from_config(&options.protocol),
            huya_imgplus: options.imgplus,
            huya_codec: options.codec,
            huya_danmaku: options.danmaku,
            huya_mobile_api: options.mobile_api,
            huya_use_wup: options.use_wup,
        }
    }

    async fn check_stream(&self) -> LiveResult<LiveStatus> {
        let Some(profile) = self.get_room_profile().await? else {
            return Ok(LiveStatus::Offline);
        };

        if profile.title.starts_with("回放")
            || profile.title.starts_with("重播")
            || profile.title.ends_with("回放")
            || profile.title.ends_with("重播")
        {
            return Ok(LiveStatus::Offline);
        }

        let stream_urls = self.build_stream_urls(&profile.stream_info).await?;
        let raw_stream_url = self.select_stream_url(&stream_urls, &profile)?;
        let stream_headers = if self.should_use_wup() {
            HashMap::from([("User-Agent".to_string(), WUP_UA.to_string())])
        } else {
            HashMap::new()
        };

        Ok(LiveStatus::Live {
            stream: Box::new(LiveStream {
                name: self.name.clone(),
                url: self.url.clone(),
                title: profile.title,
                date: Utc::now(),
                live_cover_url: profile.cover,
                suffix: media_ext_from_url(&raw_stream_url)
                    .unwrap_or_else(|| self.huya_protocol.extension().to_string()),
                raw_stream_url,
                platform: "huya".to_string(),
                stream_headers,
                danmaku: self.danmaku_source(),
                downloader_hint: DownloaderHint::StreamGears,
                runtime_options: None,
            }),
        })
    }

    fn room_id(&self) -> LiveResult<&str> {
        self.url
            .split("huya.com/")
            .nth(1)
            .and_then(|part| part.split('?').next())
            .filter(|part| !part.is_empty())
            .ok_or_else(|| LiveError::custom("虎牙直播间地址错误"))
    }

    async fn get_room_profile(&self) -> LiveResult<Option<HuyaRoomProfile>> {
        if self.huya_mobile_api {
            self.get_room_profile_from_api().await
        } else {
            let page = self.get_room_page().await?;
            self.extract_room_profile_from_page(&page)
        }
    }

    async fn get_room_page(&self) -> LiveResult<String> {
        let room_id = self.room_id()?;
        let text = self
            .client
            .get(format!("{HUYA_WEB_BASE_URL}/{room_id}"))
            .header("referer", &self.url)
            .header("user-agent", HUYA_USER_AGENT)
            .send()
            .await
            .map_err(|err| LiveError::custom(format!("获取虎牙直播间页面失败: {err}")))?
            .text()
            .await
            .map_err(|err| LiveError::custom(format!("读取虎牙直播间页面失败: {err}")))?;

        if text.contains("找不到这个主播") || text.contains("该主播涉嫌违规，正在整改中") {
            return Err(LiveError::custom("虎牙直播间不可用"));
        }
        Ok(decode_html_entities(&text))
    }

    async fn get_room_profile_from_api(&self) -> LiveResult<Option<HuyaRoomProfile>> {
        let room_id = self.room_id()?;
        let text = self
            .client
            .get(format!("{HUYA_MP_BASE_URL}/cache.php"))
            .query(&[
                ("m", "Live"),
                ("do", "profileRoom"),
                ("roomid", room_id),
                ("showSecret", "1"),
            ])
            .header("user-agent", HUYA_USER_AGENT)
            .send()
            .await
            .map_err(|err| LiveError::custom(format!("获取虎牙移动端房间信息失败: {err}")))?
            .text()
            .await
            .map_err(|err| LiveError::custom(format!("读取虎牙移动端房间信息失败: {err}")))?;

        let decoded = decode_html_entities(&text);
        let root: Value = serde_json::from_str(&decoded)
            .map_err(|err| LiveError::custom(format!("解析虎牙移动端房间信息失败: {err}")))?;
        let status = root
            .get("status")
            .and_then(|status| status.as_i64())
            .unwrap_or_default();
        if status != 200 {
            let message = root
                .get("message")
                .and_then(|message| message.as_str())
                .unwrap_or("未知错误");
            return Err(LiveError::custom(format!("虎牙移动端接口错误: {message}")));
        }

        self.extract_room_profile_from_api(&root)
    }

    fn extract_room_profile_from_page(&self, page: &str) -> LiveResult<Option<HuyaRoomProfile>> {
        let room_data = extract_json_after(page, r"var\s+TT_ROOM_DATA\s*=\s*", ';')?;
        let room_state = room_data
            .get("state")
            .and_then(|state| state.as_str())
            .unwrap_or_default();

        let stream = extract_stream_json(page)?;
        let bitrate_info = stream
            .get("vMultiStreamInfo")
            .and_then(|info| info.as_array())
            .cloned()
            .unwrap_or_default();

        if room_state != "ON" || bitrate_info.is_empty() {
            return Ok(None);
        }

        let data = stream
            .get("data")
            .and_then(|data| data.as_array())
            .and_then(|data| data.first())
            .ok_or_else(|| LiveError::custom("虎牙流数据为空"))?;
        let live_info = data
            .get("gameLiveInfo")
            .ok_or_else(|| LiveError::custom("虎牙直播信息为空"))?;
        let stream_info = data
            .get("gameStreamInfoList")
            .and_then(|info| info.as_array())
            .cloned()
            .unwrap_or_default();
        if stream_info.is_empty() {
            return Ok(None);
        }

        Ok(Some(HuyaRoomProfile {
            title: live_info
                .get("introduction")
                .and_then(|title| title.as_str())
                .unwrap_or_default()
                .to_string(),
            cover: live_info
                .get("screenshot")
                .and_then(|cover| cover.as_str())
                .unwrap_or_default()
                .replace("http://", "https://"),
            max_bitrate: live_info
                .get("bitRate")
                .and_then(|bitrate| bitrate.as_u64())
                .unwrap_or_default() as u32,
            bitrate_info,
            stream_info,
        }))
    }

    fn extract_room_profile_from_api(&self, root: &Value) -> LiveResult<Option<HuyaRoomProfile>> {
        let data = root
            .get("data")
            .ok_or_else(|| LiveError::custom("虎牙移动端 data 为空"))?;
        let live_status = data
            .get("liveStatus")
            .and_then(|status| status.as_str())
            .unwrap_or_default();
        let live_data = data.get("liveData").cloned().unwrap_or(Value::Null);
        let bitrate_raw = live_data
            .get("bitRateInfo")
            .and_then(|info| info.as_str())
            .unwrap_or_default();
        if live_status != "ON" || bitrate_raw.is_empty() {
            return Ok(None);
        }

        let bitrate_info: Vec<Value> = serde_json::from_str(bitrate_raw)
            .map_err(|err| LiveError::custom(format!("解析虎牙码率信息失败: {err}")))?;
        let stream_info = data
            .get("stream")
            .and_then(|stream| stream.get("baseSteamInfoList"))
            .and_then(|info| info.as_array())
            .cloned()
            .unwrap_or_default();
        if stream_info.is_empty() {
            return Ok(None);
        }

        Ok(Some(HuyaRoomProfile {
            title: live_data
                .get("introduction")
                .and_then(|title| title.as_str())
                .unwrap_or_default()
                .to_string(),
            cover: live_data
                .get("screenshot")
                .and_then(|cover| cover.as_str())
                .unwrap_or_default()
                .replace("http://", "https://"),
            max_bitrate: live_data
                .get("bitRate")
                .and_then(|bitrate| bitrate.as_u64())
                .unwrap_or_default() as u32,
            bitrate_info,
            stream_info,
        }))
    }

    fn should_use_wup(&self) -> bool {
        should_use_wup(self.huya_mobile_api, self.huya_imgplus, self.huya_use_wup)
    }

    async fn build_stream_urls(&self, streams_info: &[Value]) -> LiveResult<Vec<(String, String)>> {
        let mut streams = Vec::new();
        let mut cached_anticode: Option<String> = None;

        for stream in streams_info {
            let priority = stream
                .get("iWebPriorityRate")
                .and_then(|priority| priority.as_i64())
                .unwrap_or_default();
            if priority < 0 {
                continue;
            }

            let stream_name = self.get_stream_name(json_str(stream, "sStreamName")?);
            let cdn = json_str(stream, "sCdnType")?.to_string();
            let suffix = json_str(stream, self.huya_protocol.suffix_key())?;
            let base_url =
                json_str(stream, self.huya_protocol.url_key())?.replace("http://", "https://");

            if cached_anticode.is_none() {
                let anti_code = if self.should_use_wup() {
                    let token =
                        huya_wup::get_cdn_token_info_ex(&self.client, &stream_name).await?;
                    let presenter_uid = stream
                        .get("lPresenterUid")
                        .and_then(|uid| uid.as_u64())
                        .unwrap_or_default();
                    build_anticode(&stream_name, &token, Some(presenter_uid))?
                } else {
                    let page_anti_code = json_str(stream, self.huya_protocol.anticode_key())?;
                    if self.huya_mobile_api && self.huya_imgplus {
                        page_anti_code.to_string()
                    } else {
                        let presenter_uid = stream
                            .get("lPresenterUid")
                            .and_then(|uid| uid.as_u64())
                            .unwrap_or_default();
                        build_anticode(&stream_name, page_anti_code, Some(presenter_uid))?
                    }
                };
                cached_anticode = Some(format!("{anti_code}&codec={}", self.huya_codec));
            }

            let anti_code = cached_anticode
                .as_ref()
                .ok_or_else(|| LiveError::custom("虎牙 anticode 生成失败"))?;
            let url = format!("{base_url}/{stream_name}.{suffix}?{anti_code}");
            streams.push((cdn, priority, url));
        }

        streams.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(streams
            .into_iter()
            .filter(|(cdn, _, _)| !matches!(cdn.as_str(), "HY" | "HUYA" | "HYZJ"))
            .map(|(cdn, _, url)| (cdn, url))
            .collect())
    }

    fn get_stream_name(&self, stream_name: &str) -> String {
        if self.huya_imgplus {
            stream_name.to_string()
        } else {
            stream_name.replace("-imgplus", "")
        }
    }

    fn select_stream_url(
        &self,
        stream_urls: &[(String, String)],
        profile: &HuyaRoomProfile,
    ) -> LiveResult<String> {
        let selected_url = stream_urls
            .iter()
            .find(|(cdn, _)| !self.huya_cdn.is_empty() && cdn == &self.huya_cdn)
            .or_else(|| stream_urls.first())
            .map(|(_, url)| url)
            .ok_or_else(|| LiveError::custom("虎牙可用 CDN 为空"))?;

        Ok(self.add_ratio(selected_url, &profile.bitrate_info, profile.max_bitrate))
    }

    fn add_ratio(&self, url: &str, bitrate_info: &[Value], max_bitrate: u32) -> String {
        if self.huya_max_ratio == 0 || url.contains("&ratio") {
            return url.to_string();
        }

        let selected_ratio = bitrate_info
            .iter()
            .filter_map(|info| {
                let bitrate = info
                    .get("iBitRate")
                    .and_then(|bitrate| bitrate.as_u64())
                    .unwrap_or(max_bitrate as u64) as u32;
                (bitrate <= self.huya_max_ratio).then_some(bitrate)
            })
            .max();

        match selected_ratio {
            Some(ratio) if ratio > 0 => format!("{url}&ratio={ratio}"),
            _ => url.to_string(),
        }
    }

    fn danmaku_source(&self) -> Option<DanmakuSource> {
        if !self.huya_danmaku {
            return None;
        }
        Some(DanmakuSource {
            platform: "huya".to_string(),
            url: self.url.clone(),
            room_id: None,
            cookie: None,
            raw: false,
            detail: false,
            extra: HashMap::new(),
            movie_id: None,
            password: None,
        })
    }
}

struct HuyaRoomProfile {
    title: String,
    cover: String,
    max_bitrate: u32,
    bitrate_info: Vec<Value>,
    stream_info: Vec<Value>,
}

enum HuyaProtocol {
    Flv,
    Hls,
}

impl HuyaProtocol {
    fn from_config(value: &str) -> Self {
        if value == "Hls" {
            Self::Hls
        } else {
            Self::Flv
        }
    }

    fn url_key(&self) -> &'static str {
        match self {
            Self::Flv => "sFlvUrl",
            Self::Hls => "sHlsUrl",
        }
    }

    fn suffix_key(&self) -> &'static str {
        match self {
            Self::Flv => "sFlvUrlSuffix",
            Self::Hls => "sHlsUrlSuffix",
        }
    }

    fn anticode_key(&self) -> &'static str {
        match self {
            Self::Flv => "sFlvAntiCode",
            Self::Hls => "sHlsAntiCode",
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            Self::Flv => "flv",
            Self::Hls => "m3u8",
        }
    }
}

fn extract_json_after(page: &str, pattern: &str, end: char) -> LiveResult<Value> {
    let re = Regex::new(pattern).unwrap();
    let Some(mat) = re.find(page) else {
        return Err(LiveError::custom("虎牙房间数据不存在"));
    };
    let start = mat.end();
    let end = page[start..]
        .find(end)
        .map(|idx| start + idx)
        .ok_or_else(|| LiveError::custom("虎牙房间数据不完整"))?;
    serde_json::from_str(page[start..end].trim())
        .map_err(|err| LiveError::custom(format!("解析虎牙房间数据失败: {err}")))
}

fn extract_stream_json(page: &str) -> LiveResult<Value> {
    let Some(start) = page.find("stream: ").map(|idx| idx + "stream: ".len()) else {
        return Err(LiveError::custom("虎牙流数据不存在"));
    };
    let end =
        find_json_value_end(page, start).ok_or_else(|| LiveError::custom("虎牙流数据不完整"))?;
    serde_json::from_str(page[start..end].trim())
        .map_err(|err| LiveError::custom(format!("解析虎牙流数据失败: {err}")))
}

fn find_json_value_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut idx = start;
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }

    let opening = *bytes.get(idx)?;
    let closing = match opening {
        b'{' => b'}',
        b'[' => b']',
        _ => return None,
    };

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[idx..].iter().copied().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 && byte == closing {
                    return Some(idx + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}


fn should_use_wup(mobile_api: bool, imgplus: bool, use_wup: bool) -> bool {
    // Align with historical biliup: mobile API + imgplus keeps raw anti_code.
    if mobile_api && imgplus {
        return false;
    }
    use_wup
}

fn build_anticode(
    stream_name: &str,
    anti_code: &str,
    uid: Option<u64>,
) -> LiveResult<String> {
    let query = serde_urlencoded::from_str::<HashMap<String, String>>(anti_code)
        .map_err(|err| LiveError::custom(format!("解析虎牙防盗链参数失败: {err}")))?;
    if !query.contains_key("fm") {
        return Ok(anti_code.to_string());
    }

    let (ctype, platform_id) = resolve_platform(&query);
    let is_wap = platform_id == "103";
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| LiveError::custom(format!("获取系统时间失败: {err}")))?;
    let now_secs = now.as_secs();
    let now_millis = now.as_millis() as u64;

    let uid = match uid {
        Some(value) if value > 0 => value,
        _ => generate_random_uid(),
    };
    let seq_id = uid + now_millis;
    let secret_hash = md5_hex(format!("{seq_id}|{ctype}|{platform_id}"));
    let convert_uid = rotl64(uid);
    let calc_uid = if is_wap { uid } else { convert_uid };

    let fm = query
        .get("fm")
        .cloned()
        .ok_or_else(|| LiveError::custom("虎牙 fm 为空"))?;
    let fm_decoded = urlencoding::decode(&fm)
        .map_err(|err| LiveError::custom(format!("解码虎牙 fm 参数失败: {err}")))?
        .to_string();
    let secret_prefix = String::from_utf8(
        STANDARD
            .decode(fm_decoded.as_bytes())
            .map_err(|err| LiveError::custom(format!("解码虎牙 fm base64 失败: {err}")))?,
    )
    .map_err(|err| LiveError::custom(format!("虎牙 fm 参数不是 UTF-8: {err}")))?
    .split('_')
    .next()
    .unwrap_or_default()
    .to_string();

    let mut ws_time = query
        .get("wsTime")
        .cloned()
        .ok_or_else(|| LiveError::custom("虎牙 wsTime 为空"))?;
    // DMR: if int(ws_time,16) - now < 20min, renew to now+1day
    if u64::from_str_radix(&ws_time, 16).unwrap_or_default() < now_secs + 20 * 60 {
        ws_time = format!("{:x}", now_secs + 24 * 60 * 60);
    }

    let secret_str = format!("{secret_prefix}_{calc_uid}_{stream_name}_{secret_hash}_{ws_time}");
    let ws_secret = md5_hex(secret_str);
    let fs = query
        .get("fs")
        .cloned()
        .unwrap_or_else(|| "bgct".to_string());
    let fm_encoded = urlencoding::encode(&fm);

    let mut parts = vec![
        format!("wsSecret={ws_secret}"),
        format!("wsTime={ws_time}"),
        format!("seqid={seq_id}"),
        format!("ctype={ctype}"),
        "ver=1".to_string(),
        format!("fs={fs}"),
        format!("fm={fm_encoded}"),
        format!("t={platform_id}"),
    ];

    if is_wap {
        let mut rng = rand::thread_rng();
        let ws_time_num = u64::from_str_radix(&ws_time, 16).unwrap_or(now_secs);
        let ct = ((ws_time_num as f64 + rng.r#gen::<f64>()) * 1000.0) as u64;
        let uuid = ((((ct as f64 % 1e10) + rng.r#gen::<f64>()) * 1e3) as u64 % 0xffff_ffff) as u32;
        parts.push(format!("uid={uid}"));
        parts.push(format!("uuid={uuid}"));
    } else {
        parts.push(format!("u={convert_uid}"));
    }

    Ok(parts.join("&"))
}

fn resolve_platform(query: &HashMap<String, String>) -> (String, String) {
    match (query.get("ctype"), query.get("t")) {
        (Some(ctype), Some(platform_id)) if !ctype.is_empty() && !platform_id.is_empty() => {
            (ctype.clone(), platform_id.clone())
        }
        (Some(ctype), _) if !ctype.is_empty() => {
            let platform_id = platform_id_for_ctype(ctype).unwrap_or_else(|| "100".to_string());
            (ctype.clone(), platform_id)
        }
        _ => random_platform(),
    }
}

fn platform_id_for_ctype(ctype: &str) -> Option<String> {
    let upper = ctype.to_ascii_uppercase();
    PLATFORMS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&upper) || name.to_ascii_uppercase() == upper)
        .map(|(_, id)| (*id).to_string())
        .or_else(|| {
            // Accept exact enum-style names.
            PLATFORMS
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(ctype))
                .map(|(_, id)| (*id).to_string())
        })
}

fn random_platform() -> (String, String) {
    let mut rng = rand::thread_rng();
    let (name, id) = PLATFORMS[rng.gen_range(0..PLATFORMS.len())];
    (name.to_string(), id.to_string())
}

fn json_str<'a>(value: &'a Value, key: &str) -> LiveResult<&'a str> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| LiveError::custom(format!("虎牙字段 {key} 为空")))
}

fn md5_hex(input: String) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn rotl64(value: u64) -> u64 {
    (((value & 0xFFFF_FFFF) << 8) | ((value & 0xFFFF_FFFF) >> 24)) & 0xFFFF_FFFF
        | (value & !0xFFFF_FFFF)
}

fn generate_random_uid() -> u64 {
    let mut rng = rand::thread_rng();
    if rng.gen_bool(0.5) {
        format!("1234{:04}", rng.gen_range(0..10000))
            .parse()
            .unwrap_or(12340000)
    } else {
        format!("140000{:07}", rng.gen_range(0..10000000))
            .parse()
            .unwrap_or(1400000000000)
    }
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn sample_fm() -> String {
        STANDARD.encode("secret_prefix_rest")
    }

    #[test]
    fn extract_stream_json_stops_before_player_config_closing_brace() {
        let page = r#"
            var hyPlayerConfig = {
                stream: {"data":[],"vMultiStreamInfo":[]}
            };
        "#;

        let stream = extract_stream_json(page).unwrap();

        assert_eq!(stream.get("data").unwrap().as_array().unwrap().len(), 0);
        assert!(stream.get("vMultiStreamInfo").unwrap().is_array());
    }

    #[test]
    fn extract_stream_json_stops_before_following_player_fields() {
        let page = r#"
            var hyPlayerConfig = {
                stream: {"data":[],"vMultiStreamInfo":[]},
                liveLineUrl: "https://example.invalid"
            };
        "#;

        let stream = extract_stream_json(page).unwrap();

        assert_eq!(stream.get("data").unwrap().as_array().unwrap().len(), 0);
        assert!(stream.get("vMultiStreamInfo").unwrap().is_array());
    }

    #[test]
    fn find_json_value_end_ignores_braces_in_strings() {
        let input = r#"{"text":"}; { ]","items":[{"value":1}]}
            };
        "#;

        let end = find_json_value_end(input, 0).unwrap();
        let value: Value = serde_json::from_str(&input[..end]).unwrap();

        assert_eq!(value["text"], "}; { ]");
        assert_eq!(value["items"][0]["value"], 1);
    }

    #[test]
    fn build_anticode_without_fm_returns_original() {
        let anti = "wsSecret=abc&wsTime=1";
        let out = build_anticode("stream", anti, Some(1234)).unwrap();
        assert_eq!(out, anti);
    }

    #[test]
    fn build_anticode_pc_includes_u_and_convert_uid() {
        let fm = sample_fm();
        let anti = format!(
            "fm={}&fs=bgct&ctype=huya_live&t=100&wsTime={:x}",
            urlencoding::encode(&fm),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600
        );
        let uid = 12345678u64;
        let out = build_anticode("demo-stream", &anti, Some(uid)).unwrap();
        let convert_uid = rotl64(uid);
        assert!(out.contains(&format!("u={convert_uid}")));
        assert!(!out.contains("uuid="));
        assert!(out.contains("ctype=huya_live"));
        assert!(out.contains("t=100"));
        assert!(out.contains("wsSecret="));
    }

    #[test]
    fn build_anticode_wap_includes_uid_uuid() {
        let fm = sample_fm();
        let anti = format!(
            "fm={}&fs=bgct&ctype=tars_mobile&t=103&wsTime={:x}",
            urlencoding::encode(&fm),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600
        );
        let uid = 12345678u64;
        let out = build_anticode("demo-stream", &anti, Some(uid)).unwrap();
        assert!(out.contains(&format!("uid={uid}")));
        assert!(out.contains("uuid="));
        assert!(!out.contains("u="));
    }

    #[test]
    fn should_use_wup_false_for_mobile_api_imgplus() {
        assert!(!should_use_wup(true, true, true));
    }

    #[test]
    fn should_use_wup_respects_flag_when_not_mobile_imgplus() {
        assert!(!should_use_wup(false, true, false));
        assert!(should_use_wup(false, true, true));
        assert!(should_use_wup(true, false, true));
    }
}
