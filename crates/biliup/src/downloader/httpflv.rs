use crate::downloader::flv_parser::{
    AACPacketType, AVCPacketType, CodecId, FrameType, SoundFormat, TagData, TagHeader,
    aac_audio_packet_header, avc_video_packet_header, script_data, tag_data, tag_header,
};
use crate::downloader::flv_writer::{FlvFile, FlvTag, TagDataHeader};
use crate::downloader::util::{
    LifecycleFile, Segmentable, absorb_forward_timestamp_jump_with_max, clamp_regression_monotonic,
    is_timestamp_anomaly, retimestamp_tag_header,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use nom::{Err, IResult};
use reqwest::Response;

use std::time::Duration;
use tokio::time::timeout;
use tracing::{info, warn};

pub async fn download(connection: Connection, file: LifecycleFile<'_>, segment: Segmentable) {
    download_with_boundaries(
        connection,
        file,
        segment,
        Box::new(|_| {}),
        Box::new(|_| {}),
    )
    .await;
}

pub type SegmentBoundaryHook<'a> = Box<dyn FnMut(&str) + Send + Sync + 'a>;

pub async fn download_with_boundaries(
    connection: Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    segment_started: SegmentBoundaryHook<'_>,
    segment_ended: SegmentBoundaryHook<'_>,
) {
    let file_name = file.file_name.clone();
    match parse_flv_with_boundaries(connection, file, segment, segment_started, segment_ended).await
    {
        Ok(_) => {
            info!("Done... {}", file_name);
        }
        Err(e) => {
            warn!("{e}")
        }
    }
}

async fn parse_flv_with_boundaries(
    mut connection: Connection,
    file: LifecycleFile<'_>,
    mut segment: Segmentable,
    mut segment_started: SegmentBoundaryHook<'_>,
    mut segment_ended: SegmentBoundaryHook<'_>,
) -> crate::downloader::error::Result<()> {
    let mut flv_tags_cache: Vec<(TagHeader, Bytes, Bytes)> = Vec::new();
    // println!("parse_flv Segment: {:?}", segment);
    let _previous_tag_size = connection.read_frame(4).await?;

    let mut out = FlvFile::new(file)?;
    segment.set_size_position(9 + 4);
    // let mut downloaded_size = 9 + 4;
    let mut on_meta_data = None;
    let mut aac_sequence_header = None;
    let mut h264_sequence_header: Option<(TagHeader, Bytes, Bytes)> = None;
    let mut prev_video_timestamp: Option<u32> = None;
    let mut prev_audio_timestamp: Option<u32> = None;
    let mut stream_max_ms: Option<u32> = None;
    let mut output_timestamp_base = None::<u32>;
    let mut regression_offset: u32 = 0;
    let mut last_output_ms = None::<u32>;
    let mut current_file_started = false;
    let mut create_new = false;
    loop {
        let tag_header_bytes = connection.read_frame(11).await?;
        if tag_header_bytes.is_empty() {
            // let mut rdr = Cursor::new(tag_header_bytes);
            // println!("{}", rdr.read_u32::<BigEndian>().unwrap());
            break;
        }

        let (_, tag_header) = map_parse_err(tag_header(&tag_header_bytes), "tag header")?;
        // write_tag_header(&mut out, &tag_header)?;

        let bytes = connection.read_frame(tag_header.data_size as usize).await?;
        let previous_tag_size = connection.read_frame(4).await?;
        // out.write(&bytes)?;
        let (i, flv_tag_data) = map_parse_err(
            tag_data(tag_header.tag_type, tag_header.data_size as usize)(&bytes),
            "tag data",
        )?;
        let flv_tag = match flv_tag_data {
            TagData::Audio(audio_data) => {
                let packet_type = if audio_data.sound_format == SoundFormat::AAC {
                    let (_, packet_header) = aac_audio_packet_header(audio_data.sound_data)
                        .expect("Error in parsing aac audio packet header.");
                    if packet_header.packet_type == AACPacketType::SequenceHeader {
                        if aac_sequence_header.is_some() {
                            warn!("Unexpected aac sequence header tag. {tag_header:?}");
                            // panic!("Unexpected aac_sequence_header tag.");
                            // create_new = true;
                        }
                        aac_sequence_header =
                            Some((tag_header, bytes.clone(), previous_tag_size.clone()))
                    }
                    Some(packet_header.packet_type)
                } else {
                    None
                };

                FlvTag {
                    header: tag_header,
                    data: TagDataHeader::Audio {
                        sound_format: audio_data.sound_format,
                        sound_rate: audio_data.sound_rate,
                        sound_size: audio_data.sound_size,
                        sound_type: audio_data.sound_type,
                        packet_type,
                    },
                }
            }
            TagData::Video(video_data) => {
                let (packet_type, composition_time) = if CodecId::H264 == video_data.codec_id {
                    let (_, avc_video_header) = avc_video_packet_header(video_data.video_data)
                        .expect("Error in parsing avc video packet header.");
                    if avc_video_header.packet_type == AVCPacketType::SequenceHeader {
                        if let Some((_, binary_data, _)) = &h264_sequence_header {
                            warn!("Unexpected h264 sequence header tag. {tag_header:?}");
                            if bytes != binary_data {
                                create_new = true;
                                warn!("Different h264 sequence header tag. {tag_header:?}");
                            }
                        }
                        h264_sequence_header =
                            Some((tag_header, bytes.clone(), previous_tag_size.clone()))
                    }
                    (
                        Some(avc_video_header.packet_type),
                        Some(avc_video_header.composition_time),
                    )
                } else {
                    (None, None)
                };

                FlvTag {
                    header: tag_header,
                    data: TagDataHeader::Video {
                        frame_type: video_data.frame_type,
                        codec_id: video_data.codec_id,
                        packet_type,
                        composition_time,
                    },
                }
            }
            TagData::Script => {
                let (_, tag_data) = script_data(i).expect("Error in parsing script tag.");
                if on_meta_data.is_some() {
                    warn!("Unexpected script tag. {tag_header:?}");
                }
                on_meta_data = Some((tag_header, bytes.clone(), previous_tag_size.clone()));

                FlvTag {
                    header: tag_header,
                    data: TagDataHeader::Script(tag_data),
                }
            }
        };
        match &flv_tag {
            FlvTag {
                data:
                    TagDataHeader::Video {
                        frame_type: FrameType::Key,
                        packet_type,
                        ..
                    },
                ..
            } if *packet_type != Some(AVCPacketType::SequenceHeader) => {
                let timestamp = flv_tag.header.timestamp as u64;
                if prev_video_timestamp.is_none() && timestamp != 0 {
                    segment.set_start_time(Duration::from_millis(timestamp));
                }
                segment.set_time_position(Duration::from_millis(timestamp));
                // 关键帧边界：先把缓存 tag 刷到当前文件。
                // 仅对“媒体帧”做时间戳异常检测；script / sequence header 不参与 prev 推进，
                // 否则新段写入的 header(ts=0 或旧 ts) 会和后续媒体帧形成假跳变。
                let mut discard_rest_of_cache = false;
                let mut dropped_after_anomaly = 0usize;
                for (tag_header, flv_tag_data, previous_tag_size_bytes) in flv_tags_cache.drain(..)
                {
                    let is_media_for_ts = is_media_timestamp_tag(&tag_header, &flv_tag_data);
                    let track_prev = if is_media_for_ts {
                        match tag_header.tag_type {
                            crate::downloader::flv_parser::TagType::Video => prev_video_timestamp,
                            crate::downloader::flv_parser::TagType::Audio => prev_audio_timestamp,
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let threshold_ms = segment.timestamp_anomaly_threshold_ms();
                    let is_anomaly = if is_media_for_ts {
                        track_prev
                            .map(|prev| is_timestamp_anomaly(prev, tag_header.timestamp, threshold_ms))
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    if !discard_rest_of_cache && is_anomaly {
                        warn!(
                            "关键帧刷新前检测到{:?}时间戳异常，准备切分文件 previous={:?} current={} delta_ms={}",
                            tag_header.tag_type,
                            track_prev,
                            tag_header.timestamp,
                            tag_header.timestamp as i64 - track_prev.unwrap_or(0) as i64
                        );
                        create_new = true;
                        discard_rest_of_cache = true;
                    } else if !discard_rest_of_cache
                        && is_media_for_ts
                        && track_prev.is_some_and(|prev| tag_header.timestamp < prev)
                    {
                        warn!(
                            "输出流 {:?} DTS 非单调（将自动钳位平滑写入） previous={:?} current={} delta_ms={}",
                            tag_header.tag_type,
                            track_prev,
                            tag_header.timestamp,
                            tag_header.timestamp as i64 - track_prev.unwrap_or(0) as i64
                        );
                    }

                    if discard_rest_of_cache {
                        dropped_after_anomaly += 1;
                        continue;
                    }

                    if is_media_for_ts && !current_file_started {
                        segment_started(&out.file.file_name);
                        current_file_started = true;
                    }
                    let output_header = rebase_media_timestamp_for_write(
                        &tag_header,
                        is_media_for_ts,
                        &mut output_timestamp_base,
                        &mut stream_max_ms,
                        &mut regression_offset,
                        &mut last_output_ms,
                    );
                    out.write_tag(&output_header, &flv_tag_data, &previous_tag_size_bytes)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    if is_media_for_ts {
                        match tag_header.tag_type {
                            crate::downloader::flv_parser::TagType::Video => {
                                prev_video_timestamp = Some(tag_header.timestamp);
                            }
                            crate::downloader::flv_parser::TagType::Audio => {
                                prev_audio_timestamp = Some(tag_header.timestamp);
                            }
                            _ => {}
                        }
                    }
                }
                if dropped_after_anomaly > 0 {
                    warn!("时间戳异常后丢弃 {dropped_after_anomaly} 个缓存 tag，避免写入损坏帧");
                }

                // 当前关键帧本身也参与检测；它一定是媒体帧。
                let threshold_ms = segment.timestamp_anomaly_threshold_ms();
                let keyframe_anomaly = prev_video_timestamp
                    .map(|prev| is_timestamp_anomaly(prev, flv_tag.header.timestamp, threshold_ms))
                    .unwrap_or(false);
                if keyframe_anomaly {
                    warn!(
                        "关键帧处检测到时间戳异常，准备切分文件 previous={:?} current={} delta_ms={}",
                        prev_video_timestamp,
                        flv_tag.header.timestamp,
                        flv_tag.header.timestamp as i64 - prev_video_timestamp.unwrap_or(0) as i64
                    );
                    create_new = true;
                } else if prev_video_timestamp.is_some_and(|prev| flv_tag.header.timestamp < prev) {
                    // 小幅回退：保留在当前文件，避免直播抖动导致碎切（由单调钳位平滑写入）
                    warn!(
                        "关键帧处 DTS 小幅回退，忽略切分 previous={:?} current={} delta_ms={}",
                        prev_video_timestamp,
                        flv_tag.header.timestamp,
                        flv_tag.header.timestamp as i64 - prev_video_timestamp.unwrap_or(0) as i64
                    );
                }

                if segment.needed() || create_new {
                    let reason = if create_new {
                        "timestamp anomaly / codec change"
                    } else {
                        "size or time limit"
                    };
                    info!("{} splitting ({reason}).{segment:?}", out.file.file_name);

                    if current_file_started {
                        segment_ended(&out.file.file_name);
                    }
                    out.create_new()?;
                    segment.set_start_time(Duration::from_millis(timestamp));
                    segment.set_size_position(9 + 4);
                    prev_video_timestamp = None;
                    prev_audio_timestamp = None;
                    stream_max_ms = None;
                    output_timestamp_base = None;
                    regression_offset = 0;
                    last_output_ms = None;
                    current_file_started = false;

                    // 开启新分段时补齐已捕获的头部标签。这些头部并非所有直播流都具备
                    // （例如纯视频流没有 AAC 序列头，部分流缺少 onMetaData 脚本标签），
                    // 缺失时跳过并告警，而不是像原先那样直接 expect/panic 中断录制。
                    // onMetaData
                    if let Some((meta_header, meta_bytes, previous_meta_tag_size)) =
                        on_meta_data.as_ref()
                    {
                        flv_tags_cache.push((
                            retimestamp_tag_header(meta_header, 0),
                            meta_bytes.clone(),
                            previous_meta_tag_size.clone(),
                        ));
                    } else {
                        warn!("切分新文件时缺少 metadata");
                    }
                    if let Some(aac_header) = aac_sequence_header.as_ref() {
                        flv_tags_cache.push((
                            retimestamp_tag_header(&aac_header.0, 0),
                            aac_header.1.clone(),
                            aac_header.2.clone(),
                        ));
                    } else {
                        warn!("切分新文件时缺少 AAC sequence header");
                    }
                    // 始终写入 H264SequenceHeader，否则新段缺少 SPS/PPS 会无法解码。
                    if let Some(h264_header) = h264_sequence_header.as_ref() {
                        flv_tags_cache.push((
                            retimestamp_tag_header(&h264_header.0, 0),
                            h264_header.1.clone(),
                            h264_header.2.clone(),
                        ));
                    } else {
                        warn!("切分新文件时缺少 h264 sequence header，后续视频可能无法播放");
                    }
                    create_new = false;
                }
                if !current_file_started {
                    // A freshly split file only has sequence headers queued.
                    // Persist them and the boundary keyframe immediately so
                    // Start reflects the first media write, not the next GOP.
                    for (cached_header, cached_data, cached_previous_size) in
                        flv_tags_cache.drain(..)
                    {
                        let is_media = is_media_timestamp_tag(&cached_header, &cached_data);
                        if is_media && !current_file_started {
                            segment_started(&out.file.file_name);
                            current_file_started = true;
                        }
                        let output_header = rebase_media_timestamp_for_write(
                            &cached_header,
                            is_media,
                            &mut output_timestamp_base,
                            &mut stream_max_ms,
                            &mut regression_offset,
                            &mut last_output_ms,
                        );
                        out.write_tag(&output_header, &cached_data, &cached_previous_size)?;
                        segment.increase_size((11 + cached_header.data_size + 4) as u64);
                        if is_media && cached_header.tag_type == crate::downloader::flv_parser::TagType::Audio {
                            prev_audio_timestamp = Some(cached_header.timestamp);
                        }
                    }

                    if !current_file_started {
                        segment_started(&out.file.file_name);
                        current_file_started = true;
                    }
                    let output_header = rebase_media_timestamp_for_write(
                        &tag_header,
                        true,
                        &mut output_timestamp_base,
                        &mut stream_max_ms,
                        &mut regression_offset,
                        &mut last_output_ms,
                    );
                    out.write_tag(&output_header, &bytes, &previous_tag_size)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    prev_video_timestamp = Some(tag_header.timestamp);
                } else {
                    flv_tags_cache.push((tag_header, bytes.clone(), previous_tag_size.clone()));
                }
            }
            _ => {
                flv_tags_cache.push((tag_header, bytes.clone(), previous_tag_size.clone()));
            }
        }
    }
    for (tag_header, flv_tag_data, previous_tag_size_bytes) in flv_tags_cache.drain(..) {
        let is_media = is_media_timestamp_tag(&tag_header, &flv_tag_data);
        if is_media && !current_file_started {
            segment_started(&out.file.file_name);
            current_file_started = true;
        }
        let output_header = rebase_media_timestamp_for_write(
            &tag_header,
            is_media,
            &mut output_timestamp_base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_output_ms,
        );
        out.write_tag(&output_header, &flv_tag_data, &previous_tag_size_bytes)?;
    }
    if current_file_started {
        segment_ended(&out.file.file_name);
    }
    Ok(())
}

fn rebase_media_timestamp(
    header: &TagHeader,
    is_media: bool,
    output_timestamp_base: &mut Option<u32>,
) -> TagHeader {
    if !is_media {
        return retimestamp_tag_header(header, 0);
    }
    let base = *output_timestamp_base.get_or_insert(header.timestamp);
    retimestamp_tag_header(header, header.timestamp.saturating_sub(base))
}

/// 写入前：先按 source 前跳压平 base，再 rebase 到段内时间轴，并对容差内回退做单调钳位平滑。
fn rebase_media_timestamp_for_write(
    header: &TagHeader,
    is_media: bool,
    output_timestamp_base: &mut Option<u32>,
    stream_max_ms: &mut Option<u32>,
    regression_offset: &mut u32,
    last_output_ms: &mut Option<u32>,
) -> TagHeader {
    if is_media {
        if let Some(absorbed) = absorb_forward_timestamp_jump_with_max(
            header.timestamp,
            stream_max_ms,
            output_timestamp_base,
        ) {
            warn!(
                "检测到时间戳前跳，已压平输出时间轴 current={} absorbed_ms={absorbed}",
                header.timestamp
            );
        }
    }
    let mut out_header = rebase_media_timestamp(header, is_media, output_timestamp_base);
    if is_media {
        let prev_offset = *regression_offset;
        out_header.timestamp = clamp_regression_monotonic(
            out_header.timestamp,
            regression_offset,
            last_output_ms,
        );
        if *regression_offset > prev_offset {
            let gap = *regression_offset - prev_offset;
            warn!(
                "输出时间戳已单调钳位平滑 tag_type={:?} raw_timestamp={} output_timestamp={} clamped_gap_ms={gap} total_offset_ms={}",
                header.tag_type,
                header.timestamp,
                out_header.timestamp,
                *regression_offset,
            );
        }
    }
    out_header
}

fn is_media_timestamp_tag(tag_header: &TagHeader, body: &Bytes) -> bool {
    match tag_header.tag_type {
        crate::downloader::flv_parser::TagType::Script => false,
        crate::downloader::flv_parser::TagType::Audio => {
            // AAC sequence header 不推进媒体时间轴
            if body.len() >= 2 {
                // sound_format 高 4 bit == 10 (AAC) 且 packet_type == 0
                let sound_format = body[0] >> 4;
                let packet_type = body[1];
                !(sound_format == 10 && packet_type == 0)
            } else {
                true
            }
        }
        crate::downloader::flv_parser::TagType::Video => {
            // AVC sequence header: frame/codec 后 packet_type == 0
            if body.len() >= 2 {
                let codec_id = body[0] & 0x0f;
                let packet_type = body[1];
                // 7 = AVC/H264
                !(codec_id == 7 && packet_type == 0)
            } else {
                true
            }
        }
    }
}

pub fn map_parse_err<'a, T>(
    i_result: IResult<&'a [u8], T>,
    msg: &str,
) -> core::result::Result<(&'a [u8], T), crate::downloader::error::Error> {
    match i_result {
        Ok((i, res)) => Ok((i, res)),
        Err(nom::Err::Incomplete(needed)) => Err(crate::downloader::error::Error::NomIncomplete(
            msg.to_string(),
            needed,
        )),
        Err(Err::Error(e)) => Err(crate::downloader::error::Error::Custom(format!(
            "parse {msg} err: {e:?}"
        ))),
        Err(Err::Failure(f)) => Err(crate::downloader::error::Error::Custom(format!(
            "{msg} Failure: {f:?}"
        ))),
    }
}

pub struct Connection {
    resp: Response,
    buffer: BytesMut,
}

impl Connection {
    pub fn new(resp: Response) -> Connection {
        Connection {
            resp,
            buffer: BytesMut::with_capacity(8 * 1024),
        }
    }

    pub async fn read_frame(
        &mut self,
        chunk_size: usize,
    ) -> crate::downloader::error::Result<Bytes> {
        // let mut buf = [0u8; 8 * 1024];
        loop {
            if chunk_size <= self.buffer.len() {
                let bytes = Bytes::copy_from_slice(&self.buffer[..chunk_size]);
                self.buffer.advance(chunk_size);
                return Ok(bytes);
            }
            // BytesMut::with_capacity(0).deref_mut()
            // tokio::fs::File::open("").read()
            // self.resp.chunk()
            match timeout(Duration::from_secs(30), self.resp.chunk()).await? {
                Ok(Some(chunk)) => {
                    // let n = chunk.len();
                    // println!("Chunk: {:?}", chunk);
                    self.buffer.put(chunk);
                    // self.buffer.put_slice(&buf[..n]);
                }
                _ => {
                    return Ok(self.buffer.split().freeze());
                }
            }
            // let n = match self.resp.read(&mut buf).await {
            //     Ok(n) => n,
            //     Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            //     Err(e) => return Err(e),
            // };

            // if n == 0 {
            //     return Ok(self.buffer.split().freeze());
            // }
            // self.buffer.put_slice(&buf[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::downloader::flv_parser::TagType;
    use bytes::{Buf, BufMut, Bytes, BytesMut};

    #[test]
    fn sequence_headers_are_not_media_timestamp_tags() {
        let audio_seq = TagHeader {
            tag_type: TagType::Audio,
            data_size: 4,
            timestamp: 0,
            stream_id: 0,
        };
        let audio_body = Bytes::from_static(&[0xAF, 0x00, 0x11, 0x90]);
        assert!(!is_media_timestamp_tag(&audio_seq, &audio_body));

        let video_seq = TagHeader {
            tag_type: TagType::Video,
            data_size: 5,
            timestamp: 0,
            stream_id: 0,
        };
        let video_body = Bytes::from_static(&[0x17, 0x00, 0x00, 0x00, 0x00]);
        assert!(!is_media_timestamp_tag(&video_seq, &video_body));

        let video_nalu = TagHeader {
            tag_type: TagType::Video,
            data_size: 5,
            timestamp: 1000,
            stream_id: 0,
        };
        let nalu_body = Bytes::from_static(&[0x17, 0x01, 0x00, 0x00, 0x00]);
        assert!(is_media_timestamp_tag(&video_nalu, &nalu_body));
    }

    #[test]
    fn retimestamp_header_sets_zero() {
        let header = TagHeader {
            tag_type: TagType::Video,
            data_size: 10,
            timestamp: 123456,
            stream_id: 0,
        };
        let zeroed = retimestamp_tag_header(&header, 0);
        assert_eq!(zeroed.timestamp, 0);
        assert_eq!(zeroed.data_size, 10);
    }

    #[test]
    fn forward_jump_is_absorbed_into_output_timeline() {
        let prev = TagHeader {
            tag_type: TagType::Video,
            data_size: 10,
            timestamp: 8_141_000,
            stream_id: 0,
        };
        let jumped = TagHeader {
            timestamp: 8_189_000,
            ..prev
        };
        let mut base = Some(0u32);
        let mut stream_max_ms = None;
        let mut regression_offset = 0u32;
        let mut last_output_ms = None;
        // 先写入 prev 对应的输出点
        assert_eq!(
            rebase_media_timestamp_for_write(
                &prev,
                true,
                &mut base,
                &mut stream_max_ms,
                &mut regression_offset,
                &mut last_output_ms,
            )
            .timestamp,
            8_141_000
        );
        let out = rebase_media_timestamp_for_write(
            &jumped,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_output_ms,
        );
        // 48s 空洞被吸收，仅保留 1ms 递增
        assert_eq!(out.timestamp, 8_141_001);

        // 同步前跳的音频到达，享受同一个 base，不发生二次吸收
        let audio_jumped = TagHeader {
            tag_type: TagType::Audio,
            data_size: 10,
            timestamp: 8_189_020,
            stream_id: 0,
        };
        let audio_out = rebase_media_timestamp_for_write(
            &audio_jumped,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_output_ms,
        );
        assert_eq!(audio_out.timestamp, 8_141_021);

        // 随后发生小幅回退（例如回退到 8_188_500ms），输出依然必须严格单调递增
        let audio_regressed = TagHeader {
            tag_type: TagType::Audio,
            data_size: 10,
            timestamp: 8_188_500,
            stream_id: 0,
        };
        let audio_clamped = rebase_media_timestamp_for_write(
            &audio_regressed,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_output_ms,
        );
        assert!(audio_clamped.timestamp > audio_out.timestamp);
    }

    #[test]
    fn media_timestamps_are_rebased_for_each_output_segment() {
        let first = TagHeader {
            tag_type: TagType::Video,
            data_size: 10,
            timestamp: 9_000,
            stream_id: 0,
        };
        let second = TagHeader {
            timestamp: 9_040,
            ..first
        };
        let header = TagHeader {
            timestamp: 55_000,
            ..first
        };
        let mut base = None;

        assert_eq!(
            rebase_media_timestamp(&header, false, &mut base).timestamp,
            0
        );
        assert_eq!(base, None, "sequence headers must not set the media base");
        assert_eq!(rebase_media_timestamp(&first, true, &mut base).timestamp, 0);
        assert_eq!(
            rebase_media_timestamp(&second, true, &mut base).timestamp,
            40
        );

        base = None;
        assert_eq!(
            rebase_media_timestamp(&header, true, &mut base).timestamp,
            0
        );
    }

    #[test]
    fn byte_it_works() -> Result<(), Box<dyn std::error::Error>> {
        let mut bb = bytes::BytesMut::with_capacity(10);
        println!("chunk {:?}", bb.chunk());
        println!("capacity {}", bb.capacity());
        bb.put(&b"hello"[..]);
        println!("chunk {:?}", bb.chunk());
        println!("remaining {}", bb.remaining());
        bb.advance(5);
        println!("capacity {}", bb.capacity());
        println!("chunk {:?}", bb.chunk());
        println!("remaining {}", bb.remaining());
        bb.put(&b"hello"[..]);
        bb.put(&b"hello"[..]);
        println!("chunk {:?}", bb.chunk());
        println!("capacity {}", bb.capacity());
        println!("remaining {}", bb.remaining());

        let mut buf = BytesMut::with_capacity(11);
        buf.put(&b"hello world"[..]);

        let other = buf.split();
        // buf.advance_mut()

        assert!(buf.is_empty());
        assert_eq!(0, buf.capacity());
        assert_eq!(11, other.capacity());
        assert_eq!(other, b"hello world"[..]);

        Ok(())
    }

    #[test]
    fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        // download(
        //     "test.flv")?;
        Ok(())
    }

    /// 回归测试：纯视频流（没有任何音频标签）在首次分段时不应 panic。
    ///
    /// 该流只包含一个 onMetaData 脚本标签和一个 H264 序列头关键帧，`aac_sequence_header`
    /// 全程为 `None`。修复前，分段重建逻辑会对 `aac_sequence_header` 执行
    /// `expect("aac_sequence_header does not exist")` 而 panic，导致纯视频直播录制中断。
    #[tokio::test]
    async fn pure_video_stream_segments_without_panic() -> Result<(), Box<dyn std::error::Error>> {
        use crate::downloader::util::{LifecycleFile, Segmentable};

        let mut data: Vec<u8> = Vec::new();
        // parse_flv_with_boundaries 起始会先读取 4 字节（上一个 tag 的大小），这里给占位。
        data.extend_from_slice(&[0, 0, 0, 0]);

        // Script 标签（onMetaData），其值为 Null（0x05）。
        // 结构：0x02(字符串) + u16 长度(10) + "onMetaData" + 0x05(Null)
        let script_body: [u8; 14] = [
            0x02, 0x00, 0x0A, b'o', b'n', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a', 0x05,
        ];
        // tag_header: type=18(script), data_size=14, timestamp=0, stream_id=0
        data.extend_from_slice(&[
            0x12, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        data.extend_from_slice(&script_body);
        data.extend_from_slice(&[0, 0, 0, 0]); // previous_tag_size

        // Video 标签：关键帧 + H264 序列头（无音频）。
        // body[0]=0x17 → frame_type=Key(1), codec_id=H264(7)
        // 其后 4 字节：avc packet_type=0(SequenceHeader) + composition_time(i24)=0
        let video_body: [u8; 5] = [0x17, 0x00, 0x00, 0x00, 0x00];
        // tag_header: type=9(video), data_size=5, timestamp=0, stream_id=0
        data.extend_from_slice(&[
            0x09, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        data.extend_from_slice(&video_body);
        data.extend_from_slice(&[0, 0, 0, 0]); // previous_tag_size

        let http_resp = http::Response::builder().status(200).body(data)?;
        let resp = reqwest::Response::from(http_resp);
        let connection = super::Connection::new(resp);

        let dir = tempfile::tempdir()?;
        let file_stem = dir.path().join("pure_video_seg");
        let file = LifecycleFile::new(file_stem.to_str().unwrap(), "flv");

        // expected_size 设得极小，确保首个关键帧即触发分段，进入头部重建路径。
        let segment = Segmentable::new(None, Some(1));

        // 修复前：此调用会 panic（aac_sequence_header does not exist）。
        super::parse_flv_with_boundaries(
            connection,
            file,
            segment,
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn avc_sequence_header_and_av_interleaving_do_not_trigger_anomaly() -> Result<(), Box<dyn std::error::Error>> {
        let mut data = Vec::new();
        // parse_flv_with_boundaries 起始直接读取 4 字节 PreviousTagSize0
        data.extend_from_slice(&[0, 0, 0, 0]);

        // 1. Script tag: onMetaData (ts=0)
        let script_body: [u8; 14] = [
            0x02, 0x00, 0x0A, b'o', b'n', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a', 0x05,
        ];
        data.extend_from_slice(&[0x12, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&script_body);
        data.extend_from_slice(&(11u32 + 14u32).to_be_bytes());

        // 2. Video tag: H264 Sequence Header (ts=0)
        let video_seq: [u8; 5] = [0x17, 0x00, 0x00, 0x00, 0x00];
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&video_seq);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 3. Audio tag: AAC Sequence Header (ts=0)
        let audio_seq: [u8; 4] = [0xAF, 0x00, 0x11, 0x90];
        data.extend_from_slice(&[0x08, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&audio_seq);
        data.extend_from_slice(&(11u32 + 4u32).to_be_bytes());

        // 4. Video Keyframe 1 (ts=1000): NALU (packet_type=1)
        let video_nalu: [u8; 5] = [0x17, 0x01, 0x00, 0x00, 0x00];
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x03, 0xE8, 0x00, 0x00, 0x00, 0x00]); // ts=1000
        data.extend_from_slice(&video_nalu);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 5. Audio packet (ts=1600): 领先视频 600ms 交织到达
        let audio_data: [u8; 4] = [0xAF, 0x01, 0x00, 0x00];
        data.extend_from_slice(&[0x08, 0x00, 0x00, 0x04, 0x00, 0x06, 0x40, 0x00, 0x00, 0x00, 0x00]); // ts=1600
        data.extend_from_slice(&audio_data);
        data.extend_from_slice(&(11u32 + 4u32).to_be_bytes());

        // 6. Video Keyframe 2 (ts=1040): 相对前一个音频 (1600) 回退了 560ms (> 500ms 容差)
        // 但视频轨自身 (1040 >= 1000) 单调递增，不应误判异常切段！
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x04, 0x10, 0x00, 0x00, 0x00, 0x00]); // ts=1040
        data.extend_from_slice(&video_nalu);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 7. 中途补发的 H264 Sequence Header (ts=0)
        // 旧实现会把它的 frame_type=Key 当成媒体关键帧，与 1040 比对报 delta_ms=-1040 并误切段。
        // 新实现必须忽略 sequence header 的时间戳检测！
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // ts=0
        data.extend_from_slice(&video_seq);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 8. Video Keyframe 3 (ts=1080)
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x04, 0x38, 0x00, 0x00, 0x00, 0x00]); // ts=1080
        data.extend_from_slice(&video_nalu);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        let http_resp = http::Response::builder().status(200).body(data)?;
        let resp = reqwest::Response::from(http_resp);
        let connection = super::Connection::new(resp);

        let dir = tempfile::tempdir()?;
        let file_stem = dir.path().join("av_interleaving_test");
        let file = LifecycleFile::new(file_stem.to_str().unwrap(), "flv");

        let mut segment_split_count = 0;
        let segment = Segmentable::new(None, None); // 不设容量/时长上限

        super::parse_flv_with_boundaries(
            connection,
            file,
            segment,
            Box::new(|_| {}),
            Box::new(|_| {
                segment_split_count += 1;
            }),
        )
        .await?;

        // 仅在最后正常退出时触发 1 次 segment_ended，中途绝无误切段
        assert_eq!(segment_split_count, 1, "中途不应发生误判切段");
        Ok(())
    }

    #[tokio::test]
    async fn real_keyframe_regression_triggers_split() -> Result<(), Box<dyn std::error::Error>> {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 0]); // PreviousTagSize0

        // 1. Script tag: onMetaData (ts=0)
        let script_body: [u8; 14] = [
            0x02, 0x00, 0x0A, b'o', b'n', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a', 0x05,
        ];
        data.extend_from_slice(&[0x12, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&script_body);
        data.extend_from_slice(&(11u32 + 14u32).to_be_bytes());

        // 2. Video tag: H264 Sequence Header (ts=0)
        let video_seq: [u8; 5] = [0x17, 0x00, 0x00, 0x00, 0x00];
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&video_seq);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 3. Video Keyframe 1 (ts=4828540, 约 80 分钟): NALU (packet_type=1)
        // 4828540 = 0x0049AC7C -> timestamp: [0x49, 0xAC, 0x7C], extended: 0x00
        let video_nalu: [u8; 5] = [0x17, 0x01, 0x00, 0x00, 0x00];
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x49, 0xAC, 0x7C, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&video_nalu);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 4. 重连后发来的新流头部：onMetaData (ts=0) 与 H264 Sequence Header (ts=0)
        data.extend_from_slice(&[0x12, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&script_body);
        data.extend_from_slice(&(11u32 + 14u32).to_be_bytes());

        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&video_seq);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        // 5. 新流首个真实关键帧 (ts=0): 此时发生真正的回退 (4828540 -> 0)
        data.extend_from_slice(&[0x09, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.extend_from_slice(&video_nalu);
        data.extend_from_slice(&(11u32 + 5u32).to_be_bytes());

        let http_resp = http::Response::builder().status(200).body(data)?;
        let resp = reqwest::Response::from(http_resp);
        let connection = super::Connection::new(resp);

        let dir = tempfile::tempdir()?;
        let file_stem = dir.path().join("regression_split_test");
        let file = LifecycleFile::new(file_stem.to_str().unwrap(), "flv");

        let mut segment_split_count = 0;
        let segment = Segmentable::new(None, None);

        super::parse_flv_with_boundaries(
            connection,
            file,
            segment,
            Box::new(|_| {}),
            Box::new(|_| {
                segment_split_count += 1;
            }),
        )
        .await?;

        // 在 ts=0 关键帧处触发切段 1 次，最后退出触发 1 次，总计 2 次 segment_ended
        assert_eq!(segment_split_count, 2, "真实媒体关键帧回退必须切出新段");
        Ok(())
    }
}
