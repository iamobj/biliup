use crate::downloader::flv_parser::{
    AACPacketType, AVCPacketType, CodecId, FrameType, SoundFormat, TagData, TagHeader, TagType,
    aac_audio_packet_header, avc_video_packet_header, script_data, tag_data, tag_header,
};
use crate::downloader::flv_writer::{FlvFile, FlvTag, TagDataHeader};
use crate::downloader::preview::{self, ChunkKind, PreviewSink};
use crate::downloader::util::{
    LifecycleFile, Segmentable, absorb_forward_timestamp_jump_with_max, clamp_regression_monotonic,
    is_timestamp_anomaly, retimestamp_tag_header,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use nom::{Err, IResult};
use reqwest::Response;

use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info, warn};

pub async fn download(
    connection: Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    preview: Option<PreviewSink>,
) {
    download_with_boundaries(
        connection,
        file,
        segment,
        preview,
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
    preview: Option<PreviewSink>,
    segment_started: SegmentBoundaryHook<'_>,
    segment_ended: SegmentBoundaryHook<'_>,
) {
    let file_name = file.file_name.clone();
    match parse_flv_with_boundaries(
        connection,
        file,
        segment,
        preview,
        segment_started,
        segment_ended,
    )
    .await
    {
        Ok(_) => {
            info!("Done... {}", file_name);
        }
        Err(e) => {
            warn!("{e}")
        }
    }
}

pub async fn parse_flv(
    connection: Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    preview: Option<PreviewSink>,
) -> crate::downloader::error::Result<()> {
    parse_flv_with_boundaries(
        connection,
        file,
        segment,
        preview,
        Box::new(|_| {}),
        Box::new(|_| {}),
    )
    .await
}

async fn parse_flv_with_boundaries(
    mut connection: Connection,
    file: LifecycleFile<'_>,
    mut segment: Segmentable,
    mut preview: Option<PreviewSink>,
    mut segment_started: SegmentBoundaryHook<'_>,
    mut segment_ended: SegmentBoundaryHook<'_>,
) -> crate::downloader::error::Result<()> {
    let mut flv_tags_cache: Vec<(TagHeader, Bytes, Bytes)> = Vec::new();
    // println!("parse_flv Segment: {:?}", segment);
    let _previous_tag_size = connection.read_frame(4).await?;

    let mut out = FlvFile::new(file)?;
    segment.set_size_position(9 + 4);
    if let Some(sink) = preview.as_mut() {
        sink.push(
            ChunkKind::Header,
            Bytes::from_static(&preview::flv::FILE_HEADER),
        );
    }
    // let mut downloaded_size = 9 + 4;
    let mut on_meta_data = None;
    let mut aac_sequence_header = None;
    let mut h264_sequence_header: Option<(TagHeader, Bytes, Bytes)> = None;
    let mut prev_video_timestamp: Option<u32> = None;
    let mut prev_audio_timestamp: Option<u32> = None;
    let mut stream_max_ms: Option<u32> = None;
    let mut output_timestamp_base = None::<u32>;
    let mut regression_offset: u32 = 0;
    let mut last_video_output_ms = None::<u32>;
    let mut last_audio_output_ms = None::<u32>;
    let mut current_file_started = false;
    let mut create_new = false;
    let mut read_result: crate::downloader::error::Result<()> = Ok(());
    loop {
        let tag_header_bytes = match connection.read_frame(11).await {
            Ok(b) => b,
            Err(e) => {
                read_result = Err(e);
                break;
            }
        };
        if tag_header_bytes.is_empty() {
            // let mut rdr = Cursor::new(tag_header_bytes);
            // println!("{}", rdr.read_u32::<BigEndian>().unwrap());
            break;
        }

        let (_, tag_header) = match map_parse_err(tag_header(&tag_header_bytes), "tag header") {
            Ok(res) => res,
            Err(e) => {
                read_result = Err(e);
                break;
            }
        };
        // write_tag_header(&mut out, &tag_header)?;

        let bytes = match connection.read_frame(tag_header.data_size as usize).await {
            Ok(b) => b,
            Err(e) => {
                read_result = Err(e);
                break;
            }
        };
        let previous_tag_size = match connection.read_frame(4).await {
            Ok(b) => b,
            Err(e) => {
                read_result = Err(e);
                break;
            }
        };
        if let Some(sink) = preview.as_mut() {
            let tag_type = tag_header.tag_type as u8;
            sink.push(
                preview::flv::classify(tag_type, &bytes),
                preview::flv::tag_chunk_from_parts(
                    &preview::flv::tag_header(tag_type, tag_header.data_size, tag_header.timestamp),
                    &bytes,
                    &previous_tag_size,
                ),
            );
        }
        // out.write(&bytes)?;
        let (i, flv_tag_data) = match map_parse_err(
            tag_data(tag_header.tag_type, tag_header.data_size as usize)(&bytes),
            "tag data",
        ) {
            Ok(res) => res,
            Err(e) => {
                read_result = Err(e);
                break;
            }
        };
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
                        &mut last_video_output_ms,
                        &mut last_audio_output_ms,
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
                    last_video_output_ms = None;
                    last_audio_output_ms = None;
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
                            &mut last_video_output_ms,
                            &mut last_audio_output_ms,
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
                        &mut last_video_output_ms,
                        &mut last_audio_output_ms,
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
    // 连接因 EOF、读超时或解析错误结束时，缓存里还留着最后一个 GOP（都是完整读到并解析过的 tag）：
    // 先写出再返回原结果，否则每次断流、重连、下播都会丢掉最后 1–10 秒
    let drain_result: std::io::Result<()> = (|| {
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
                &mut last_video_output_ms,
                &mut last_audio_output_ms,
            );
            out.write_tag(&output_header, &flv_tag_data, &previous_tag_size_bytes)?;
            segment.increase_size((11 + tag_header.data_size + 4) as u64);
        }
        Ok(())
    })();
    if current_file_started {
        segment_ended(&out.file.file_name);
    }
    match (read_result, drain_result) {
        (Ok(()), drained) => Ok(drained?),
        (Err(e), Err(drained)) => {
            warn!("writing the cached GOP after `{e}` failed: {drained}");
            Err(e)
        }
        (Err(e), Ok(())) => Err(e),
    }
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

/// 写入前：先按 source 前跳压平 base，再 rebase 到段内时间轴，并对容差内回退做分轨单调钳位平滑。
fn rebase_media_timestamp_for_write(
    header: &TagHeader,
    is_media: bool,
    output_timestamp_base: &mut Option<u32>,
    stream_max_ms: &mut Option<u32>,
    regression_offset: &mut u32,
    last_video_output_ms: &mut Option<u32>,
    last_audio_output_ms: &mut Option<u32>,
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
        let last_track_output = match header.tag_type {
            TagType::Video => last_video_output_ms,
            TagType::Audio => last_audio_output_ms,
            _ => return out_header,
        };
        out_header.timestamp = clamp_regression_monotonic(
            out_header.timestamp,
            regression_offset,
            last_track_output,
        );
        if *regression_offset > prev_offset {
            let gap = *regression_offset - prev_offset;
            // 优化日志级别：微小容差平滑（< 100ms）使用 debug 避免刷屏，显著回退（>= 100ms）才使用 warn 提醒
            if gap >= 100 {
                warn!(
                    "输出时间戳已单调钳位平滑 tag_type={:?} raw_timestamp={} output_timestamp={} clamped_gap_ms={gap} total_offset_ms={}",
                    header.tag_type,
                    header.timestamp,
                    out_header.timestamp,
                    *regression_offset,
                );
            } else {
                debug!(
                    "输出时间戳已单调钳位平滑 tag_type={:?} raw_timestamp={} output_timestamp={} clamped_gap_ms={gap} total_offset_ms={}",
                    header.tag_type,
                    header.timestamp,
                    out_header.timestamp,
                    *regression_offset,
                );
            }
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
        let mut last_video_output_ms = None;
        let mut last_audio_output_ms = None;
        // 先写入 prev 对应的输出点
        assert_eq!(
            rebase_media_timestamp_for_write(
                &prev,
                true,
                &mut base,
                &mut stream_max_ms,
                &mut regression_offset,
                &mut last_video_output_ms,
                &mut last_audio_output_ms,
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
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
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
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
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
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );
        assert!(audio_clamped.timestamp > audio_out.timestamp);
    }

    #[test]
    fn av_interleaving_same_timestamp_does_not_trigger_clamp_offset() {
        let v1 = TagHeader {
            tag_type: TagType::Video,
            data_size: 10,
            timestamp: 1000,
            stream_id: 0,
        };
        let a1 = TagHeader {
            tag_type: TagType::Audio,
            data_size: 10,
            timestamp: 1000,
            stream_id: 0,
        };
        let v2 = TagHeader {
            tag_type: TagType::Video,
            data_size: 10,
            timestamp: 1033,
            stream_id: 0,
        };
        let a2 = TagHeader {
            tag_type: TagType::Audio,
            data_size: 10,
            timestamp: 1021, // 相比 v2(1033) 稍小，但相比自身 a1(1000) 单调递增
            stream_id: 0,
        };

        let mut base = Some(1000u32);
        let mut stream_max_ms = None;
        let mut regression_offset = 0u32;
        let mut last_video_output_ms = None;
        let mut last_audio_output_ms = None;

        let out_v1 = rebase_media_timestamp_for_write(
            &v1,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );
        let out_a1 = rebase_media_timestamp_for_write(
            &a1,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );
        let out_v2 = rebase_media_timestamp_for_write(
            &v2,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );
        let out_a2 = rebase_media_timestamp_for_write(
            &a2,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );

        assert_eq!(out_v1.timestamp, 0);
        assert_eq!(out_a1.timestamp, 0);
        assert_eq!(out_v2.timestamp, 33);
        assert_eq!(out_a2.timestamp, 21);
        // 音视频交织/同时间戳不应触发任何虚假 offset 垫高
        assert_eq!(regression_offset, 0);

        // 模拟后续真实回退：视频帧回退 200ms
        let v3_regressed = TagHeader {
            tag_type: TagType::Video,
            data_size: 10,
            timestamp: 833, // 从 1033 回退到 833 (rebase 后是 0 saturating, candidate 0 <= prev 33)
            stream_id: 0,
        };
        let out_v3 = rebase_media_timestamp_for_write(
            &v3_regressed,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );
        // 视频回退被垫高推进到 34 (33 + 1)
        assert_eq!(out_v3.timestamp, 34);
        assert_eq!(regression_offset, 34);

        // 伴随的音频也回退到了 825，但享受相同的 regression_offset 协同垫高
        let a3_regressed = TagHeader {
            tag_type: TagType::Audio,
            data_size: 10,
            timestamp: 825,
            stream_id: 0,
        };
        let out_a3 = rebase_media_timestamp_for_write(
            &a3_regressed,
            true,
            &mut base,
            &mut stream_max_ms,
            &mut regression_offset,
            &mut last_video_output_ms,
            &mut last_audio_output_ms,
        );
        // a3 协同垫高后，时间戳单调且相对音画差保持一致
        assert!(out_a3.timestamp > out_a2.timestamp);
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

    /// 一段最小的纯视频 FLV 流体（不含 9 字节文件头）：
    /// onMetaData 脚本标签 + 一个 H264 序列头关键帧，没有任何音频标签。
    fn pure_video_flv_body() -> Vec<u8> {
        let mut data: Vec<u8> = Vec::new();
        // parse_flv 起始会先读取 4 字节（上一个 tag 的大小），这里给占位。
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
        data
    }

    /// 回归测试：纯视频流（没有任何音频标签）在首次分段时不应 panic。
    ///
    /// 该流只包含一个 onMetaData 脚本标签和一个 H264 序列头关键帧，`aac_sequence_header`
    /// 全程为 `None`。修复前，分段重建逻辑会对 `aac_sequence_header` 执行
    /// `expect("aac_sequence_header does not exist")` 而 panic，导致纯视频直播录制中断。
    #[tokio::test]
    async fn pure_video_stream_segments_without_panic() -> Result<(), Box<dyn std::error::Error>> {
        use crate::downloader::util::{LifecycleFile, Segmentable};

        let http_resp = http::Response::builder()
            .status(200)
            .body(pure_video_flv_body())?;
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
            None,
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
            None,
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
            None,
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

    /// 一段带 onMetaData、AVC 序列头与 `keyframes` 个 GOP（每 GOP 一个关键帧 + 两个普通帧）
    /// 的 FLV 流体（不含 9 字节文件头，含起始的 PreviousTagSize0）。
    fn gop_flv_body(keyframes: u32) -> Vec<u8> {
        fn tag(data: &mut Vec<u8>, tag_type: u8, ts: u32, body: &[u8]) {
            data.push(tag_type);
            data.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            data.extend_from_slice(&(ts & 0xff_ffff).to_be_bytes()[1..]);
            data.push((ts >> 24) as u8);
            data.extend_from_slice(&[0, 0, 0]);
            data.extend_from_slice(body);
            data.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
        }
        let mut data = vec![0, 0, 0, 0];
        tag(
            &mut data,
            18,
            0,
            &[
                0x02, 0x00, 0x0A, b'o', b'n', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a', 0x05,
            ],
        );
        // AVC 序列头（packet_type 0）
        tag(&mut data, 9, 0, &[0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x64]);
        for i in 0..keyframes {
            let base = i * 120;
            // 关键帧 NALU（packet_type 1）+ 两个普通帧，载荷里带序号便于比对
            let mut key = vec![0x17, 0x01, 0, 0, 0];
            key.extend_from_slice(&i.to_be_bytes());
            key.resize(300, 0xaa);
            tag(&mut data, 9, base, &key);
            tag(&mut data, 9, base + 40, &[0x27, 0x01, 0, 0, 0, 0x11]);
            tag(&mut data, 9, base + 80, &[0x27, 0x01, 0, 0, 0, 0x22]);
        }
        data
    }

    fn concat(chunks: &[bytes::Bytes]) -> Vec<u8> {
        chunks.iter().flat_map(|b| b.iter().copied()).collect()
    }

    /// 预览快照 + 实时分块拼起来，恰好等于源流「文件头 + 从第一个关键帧起的全部 tag」，
    /// 即预览在解析点按 tag 实时旁路，不等写盘循环在下一个关键帧才整 GOP 落盘（那会让
    /// 预览端每个 GOP 间隔收到一次突发，缓冲刚好在下一个突发到达时耗尽）；
    /// 跨多个分段文件（rolling）时连接不断、不重发 FLV 文件头，分段本身照常切。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_follows_the_parsed_tags_across_rolling_segments()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::downloader::preview::{PreviewFormat, PreviewHub};
        use crate::downloader::util::{LifecycleFile, Segmentable};
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        // 按真实直播的节奏分块送入（每块之间隔 2 ms），而不是一次性给完：
        // 瞬时灌完会让订阅者必然掉队（Lagged），那是另一个测试覆盖的场景
        let body = gop_flv_body(120);
        let pieces: Vec<Vec<u8>> = body.chunks(512).map(|c| c.to_vec()).collect();
        let paced = futures::stream::iter(pieces).then(|piece| async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            Ok::<_, std::io::Error>(piece)
        });
        let http_resp = http::Response::builder()
            .status(200)
            .body(reqwest::Body::wrap_stream(paced))?;
        let connection = super::Connection::new(reqwest::Response::from(http_resp));

        // 文件名模板不含时间占位符时每个分段都叫同一个名字，会互相覆盖；
        // 在 rename 钩子里把每个分段改成带序号的名字留下来，并记下钩子触发时的文件大小
        let dir = tempfile::tempdir()?;
        let file_stem = dir.path().join("rolling");
        let segments: Arc<Mutex<Vec<(std::path::PathBuf, u64)>>> = Arc::default();
        let file = LifecycleFile::with_hook(file_stem.to_str().unwrap(), "flv", {
            let segments = segments.clone();
            let dir = dir.path().to_path_buf();
            move |name: &str| {
                let mut segments = segments.lock().unwrap();
                let size_at_hook = std::fs::metadata(name).unwrap().len();
                let kept = dir.join(format!("seg-{:04}.flv", segments.len()));
                std::fs::rename(name, &kept).unwrap();
                segments.push((kept, size_at_hook));
            }
        });

        let hub = PreviewHub::new(4);
        let sink = hub.attach(PreviewFormat::Flv);
        // 每个分段 2 KB 左右，120 个 GOP 会切出二十多个文件
        let segment = Segmentable::new(None, Some(2 * 1024));

        // 先把订阅请求确定地排进写入端的队列（poll 一次即入队），再开始写盘，
        // 这样快照一定在第一个关键帧被回应，预览覆盖整条流
        let mut subscribe = Box::pin({
            let hub = hub.clone();
            async move { hub.subscribe(Duration::from_secs(5)).await }
        });
        assert!(futures::poll!(subscribe.as_mut()).is_pending());
        let subscriber = tokio::spawn(async move {
            let mut sub = subscribe.await.unwrap();
            let mut live = Vec::new();
            while let Ok(chunk) = sub.rx.recv().await {
                live.extend_from_slice(&chunk);
            }
            (sub.snapshot, live)
        });

        super::parse_flv(connection, file, segment, Some(sink)).await?;

        let (snapshot, live) = subscriber.await?;
        // 快照 = 文件头 + onMetaData + AVC 序列头 + 从第一个关键帧起的 GOP
        assert_eq!(
            &snapshot[0][..],
            &crate::downloader::preview::flv::FILE_HEADER
        );
        let is_key_nalu =
            |c: &bytes::Bytes| c.len() > 12 && c[0] == 9 && c[11] == 0x17 && c[12] == 0x01;
        let first_key = snapshot
            .iter()
            .position(is_key_nalu)
            .expect("snapshot must contain a keyframe");
        let seq_headers: Vec<u8> = snapshot[1..first_key].iter().map(|c| c[0]).collect();
        assert_eq!(
            seq_headers,
            vec![18, 9],
            "onMetaData + AVC sequence header before the GOP"
        );
        let mut received = concat(&snapshot);
        received.extend_from_slice(&live);
        // 整条流里不再出现第二个文件头
        assert_eq!(
            received[13..].windows(3).filter(|w| *w == b"FLV").count(),
            0,
            "existing subscribers must never get the FLV file header again"
        );

        // 预览 = 文件头 + 序列头 + 从第一个关键帧起的全部 tag = 源流逐字节（源流体不含 9 字节
        // 文件头、含 PreviousTagSize0）
        assert_eq!(
            &received[13..],
            &body[4..],
            "preview must reproduce the source tags byte for byte"
        );

        // 落盘不受影响：多个分段，每个都以 FLV 头开始，tag 序列是源流的子序列（分段处补了序列头）
        let segments = segments.lock().unwrap();
        assert!(
            segments.len() > 5,
            "expected rolling into many segments, got {}",
            segments.len()
        );
        let mut written_tags = Vec::new();
        for (path, size_at_hook) in segments.iter() {
            let data = std::fs::read(path)?;
            assert_eq!(&data[..13], &crate::downloader::preview::flv::FILE_HEADER);
            assert_eq!(
                *size_at_hook,
                data.len() as u64,
                "the hook must see the flushed file ({})",
                path.display()
            );
            written_tags.extend_from_slice(&data[13..]);
        }
        // 关键帧 NALU 的标记：连接结束时缓存里的最后一个 GOP 也落盘，写盘与预览都是 120 个
        assert_eq!(keyframes(&written_tags), 120);
        assert_eq!(keyframes(&received), 120);
        Ok(())
    }

    fn keyframes(data: &[u8]) -> usize {
        data.windows(5)
            .filter(|w| *w == [0x17, 0x01, 0, 0, 0])
            .count()
    }

    /// 录一段不分段的流，返回 `parse_flv` 的结果与落盘文件内容。
    async fn record_unsegmented(
        body: reqwest::Body,
    ) -> (crate::downloader::error::Result<()>, Vec<u8>) {
        use crate::downloader::util::{LifecycleFile, Segmentable};

        let http_resp = http::Response::builder().status(200).body(body).unwrap();
        let connection = super::Connection::new(reqwest::Response::from(http_resp));
        let dir = tempfile::tempdir().unwrap();
        let file_stem = dir.path().join("rec");
        let file = LifecycleFile::new(file_stem.to_str().unwrap(), "flv");
        let result = super::parse_flv(connection, file, Segmentable::new(None, None), None).await;
        let data = std::fs::read(dir.path().join("rec.flv")).unwrap();
        (result, data)
    }

    /// 下播：源站在 tag 边界干净地关掉连接，最后一个 GOP 照样落盘。
    #[tokio::test]
    async fn the_last_gop_is_written_when_the_stream_ends() {
        let body = gop_flv_body(5);
        let (result, data) = record_unsegmented(body.clone().into()).await;
        result.unwrap();
        assert_eq!(keyframes(&data), 5);
        assert_eq!(&data[13..], &body[4..], "every source tag is on disk");
    }

    /// 断流：读到一半 30 秒没有新数据，读超时照样作为错误返回，但此前完整读到的 GOP 都已落盘。
    #[tokio::test(start_paused = true)]
    async fn the_last_gop_is_written_when_a_read_times_out() {
        use futures::StreamExt;

        let body = gop_flv_body(5);
        let stalled = futures::stream::iter([Ok::<_, std::io::Error>(body.clone())])
            .chain(futures::stream::pending());
        let (result, data) = record_unsegmented(reqwest::Body::wrap_stream(stalled)).await;
        assert!(
            matches!(
                result,
                Err(crate::downloader::error::Error::ElapsedError(_))
            ),
            "the read timeout must still be reported: {result:?}"
        );
        assert_eq!(keyframes(&data), 5);
        assert_eq!(&data[13..], &body[4..]);
    }

    /// 解析错误：流里混进一个坏 tag，错误照样返回，坏 tag 之前的 GOP 都已落盘，坏字节不落盘。
    #[tokio::test]
    async fn the_last_gop_is_written_when_parsing_fails() {
        let good = gop_flv_body(5);
        let mut body = good.clone();
        body.extend_from_slice(&[0xff; 32]);
        let (result, data) = record_unsegmented(body.into()).await;
        assert!(result.is_err(), "the parse error must still be reported");
        assert_eq!(keyframes(&data), 5);
        assert_eq!(&data[13..], &good[4..]);
    }

    /// 一个从不读取的订阅者在场时，录制照常结束、落盘内容与无订阅者时完全一致。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_preview_subscriber_does_not_change_what_is_written()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::downloader::preview::{PreviewFormat, PreviewHub, PreviewSink};
        use crate::downloader::util::{LifecycleFile, Segmentable};
        use std::time::Duration;

        // 足够多的 tag，让掉队的订阅者远超广播缓冲容量
        let body = gop_flv_body(PreviewFormat::Flv.broadcast_capacity() as u32 * 2);
        let run = |body: Vec<u8>, dir: &std::path::Path, sink: Option<PreviewSink>| {
            let dir = dir.to_path_buf();
            async move {
                let http_resp = http::Response::builder().status(200).body(body).unwrap();
                let connection = super::Connection::new(reqwest::Response::from(http_resp));
                let file_stem = dir.join("rec");
                let file = LifecycleFile::new(file_stem.to_str().unwrap(), "flv");
                let started = std::time::Instant::now();
                super::parse_flv(connection, file, Segmentable::new(None, None), sink)
                    .await
                    .unwrap();
                let data = std::fs::read(dir.join("rec.flv")).unwrap();
                (data, started.elapsed())
            }
        };

        let plain_dir = tempfile::tempdir()?;
        let (plain, _) = run(body.clone(), plain_dir.path(), None).await;

        let hub = PreviewHub::new(4);
        let sink = hub.attach(PreviewFormat::Flv);
        let mut subscribe = Box::pin({
            let hub = hub.clone();
            async move { hub.subscribe(Duration::from_secs(5)).await }
        });
        assert!(futures::poll!(subscribe.as_mut()).is_pending());
        let stalled = tokio::spawn(async move {
            let sub = subscribe.await.unwrap();
            // 拿到订阅后一个字节都不读，直到写入端结束
            tokio::time::sleep(Duration::from_millis(1500)).await;
            sub
        });
        let teed_dir = tempfile::tempdir()?;
        let (teed, elapsed) = run(body, teed_dir.path(), Some(sink)).await;

        assert_eq!(
            teed, plain,
            "the recording must not depend on preview subscribers"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the producer must not wait for a stalled subscriber ({elapsed:?})"
        );
        let mut sub = stalled.await?;
        assert!(matches!(
            sub.rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed)
        ));
        Ok(())
    }

    /// 写盘字节计数：只统计真正经 `write_tag` 落盘的 tag（11 字节头 + 数据 + 4 字节 previous tag size）。
    ///
    /// 这段流里 onMetaData 脚本标签在遇到关键帧时写出（14 字节数据 → 29 字节），
    /// 关键帧（5 字节数据 → 20 字节）留在缓存里，流结束时一并落盘。
    #[tokio::test]
    async fn written_bytes_are_counted_on_the_shared_counter()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::downloader::util::{ByteCounter, LifecycleFile, Segmentable};

        let http_resp = http::Response::builder()
            .status(200)
            .body(pure_video_flv_body())?;
        let connection = super::Connection::new(reqwest::Response::from(http_resp));

        let dir = tempfile::tempdir()?;
        let file_stem = dir.path().join("counted");
        let counter = ByteCounter::new();
        let file =
            LifecycleFile::new(file_stem.to_str().unwrap(), "flv").with_counter(counter.clone());

        super::parse_flv(connection, file, Segmentable::new(None, None), None).await?;
        assert_eq!(counter.total(), (11 + 14 + 4) + (11 + 5 + 4));
        Ok(())
    }

    /// 关键帧索引旁路报告的每个 tag（偏移、时间戳、长度、判定）与落盘文件逐一对应，
    /// 跨分段时每个文件从头计偏移，关段报告的长度就是文件长度。
    #[tokio::test]
    async fn index_tap_reports_the_on_disk_offset_of_every_tag()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::downloader::index_tap::{FlvTagKind, IndexEvent, IndexTap};
        use crate::downloader::util::{LifecycleFile, Segmentable};
        use std::sync::{Arc, Mutex};

        fn classify(tag_type: u8, body: &[u8]) -> FlvTagKind {
            FlvTagKind {
                media: matches!(tag_type, 8 | 9),
                sequence_header: tag_type == 9 && body.get(1) == Some(&0),
                keyframe: tag_type == 9 && body[0] >> 4 == 1,
            }
        }

        let http_resp = http::Response::builder()
            .status(200)
            .body(gop_flv_body(40))?;
        let connection = super::Connection::new(reqwest::Response::from(http_resp));

        let dir = tempfile::tempdir()?;
        let kept: Arc<Mutex<Vec<std::path::PathBuf>>> = Arc::default();
        let file = LifecycleFile::with_hook(dir.path().join("tap").to_str().unwrap(), "flv", {
            let kept = kept.clone();
            let dir = dir.path().to_path_buf();
            move |name: &str| {
                let mut kept = kept.lock().unwrap();
                let path = dir.join(format!("seg-{:04}.flv", kept.len()));
                std::fs::rename(name, &path).unwrap();
                kept.push(path);
            }
        });
        let (tap, mut rx) = IndexTap::channel(1 << 16, |tag_type, body| classify(tag_type, body));
        let file = file.with_index_tap(Some(tap));
        super::parse_flv(connection, file, Segmentable::new(None, Some(2048)), None).await?;

        type Tags = Vec<(u64, u32, u32, FlvTagKind)>;
        let mut reported: Vec<(Tags, Option<u64>)> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                IndexEvent::Opened(file) => {
                    assert!(file.path().to_string_lossy().ends_with("tap.flv.part"));
                    reported.push((Vec::new(), None));
                }
                IndexEvent::FlvTag {
                    offset,
                    timestamp,
                    data_size,
                    kind,
                    ..
                } => reported
                    .last_mut()
                    .unwrap()
                    .0
                    .push((offset, timestamp, data_size, kind)),
                IndexEvent::Closed { len, .. } => reported.last_mut().unwrap().1 = Some(len),
                other => panic!("unexpected {other:?}"),
            }
        }

        let kept = kept.lock().unwrap();
        assert!(
            kept.len() > 3,
            "expected several segments, got {}",
            kept.len()
        );
        assert_eq!(reported.len(), kept.len());
        for ((tags, closed), path) in reported.iter().zip(kept.iter()) {
            let bytes = std::fs::read(path)?;
            assert_eq!(*closed, Some(bytes.len() as u64));
            let mut on_disk = Vec::new();
            let mut offset = 13;
            while offset < bytes.len() {
                let h = &bytes[offset..offset + 11];
                let size = u32::from_be_bytes([0, h[1], h[2], h[3]]);
                let ts = u32::from_be_bytes([h[7], h[4], h[5], h[6]]);
                let body = &bytes[offset + 11..offset + 11 + size as usize];
                on_disk.push((offset as u64, ts, size, classify(h[0], body)));
                offset += 15 + size as usize;
            }
            assert_eq!(*tags, on_disk, "{}", path.display());
        }
        Ok(())
    }

}
