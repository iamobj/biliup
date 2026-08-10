use crate::downloader::flv_parser::{
    AACPacketType, AVCPacketType, CodecId, FrameType, SoundFormat, TagData, TagHeader,
    aac_audio_packet_header, avc_video_packet_header, script_data, tag_data, tag_header,
};
use crate::downloader::flv_writer::{FlvFile, FlvTag, TagDataHeader};
use crate::downloader::util::{
    LifecycleFile, Segmentable, absorb_forward_timestamp_jump, is_timestamp_anomaly,
    retimestamp_tag_header,
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
    let mut prev_timestamp = 0;
    let mut output_timestamp_base = None::<u32>;
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
                        ..
                    },
                ..
            } => {
                let timestamp = flv_tag.header.timestamp as u64;
                if prev_timestamp == 0 && timestamp != 0 {
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

                    if !discard_rest_of_cache
                        && is_media_for_ts
                        && segment.split_on_timestamp_anomaly()
                        && is_timestamp_anomaly(prev_timestamp, tag_header.timestamp)
                    {
                        warn!(
                            "关键帧刷新前检测到时间戳异常，准备切分文件 previous={prev_timestamp} current={} delta_ms={}",
                            tag_header.timestamp,
                            tag_header.timestamp as i64 - prev_timestamp as i64
                        );
                        create_new = true;
                        discard_rest_of_cache = true;
                    } else if !discard_rest_of_cache
                        && is_media_for_ts
                        && prev_timestamp > 0
                        && tag_header.timestamp < prev_timestamp
                    {
                        warn!(
                            "输出流 DTS 非单调 previous={prev_timestamp} current={} delta_ms={}",
                            tag_header.timestamp,
                            tag_header.timestamp as i64 - prev_timestamp as i64
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
                        prev_timestamp,
                    );
                    out.write_tag(&output_header, &flv_tag_data, &previous_tag_size_bytes)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    if is_media_for_ts {
                        prev_timestamp = tag_header.timestamp;
                    }
                }
                if dropped_after_anomaly > 0 {
                    warn!("时间戳异常后丢弃 {dropped_after_anomaly} 个缓存 tag，避免写入损坏帧");
                }

                // 当前关键帧本身也参与检测；它一定是媒体帧。
                let keyframe_anomaly = segment.split_on_timestamp_anomaly()
                    && is_timestamp_anomaly(prev_timestamp, flv_tag.header.timestamp);
                if keyframe_anomaly {
                    warn!(
                        "关键帧处检测到时间戳异常，准备切分文件 previous={prev_timestamp} current={} delta_ms={}",
                        flv_tag.header.timestamp,
                        flv_tag.header.timestamp as i64 - prev_timestamp as i64
                    );
                    create_new = true;
                } else if segment.split_on_timestamp_anomaly()
                    && prev_timestamp > 0
                    && flv_tag.header.timestamp < prev_timestamp
                {
                    // 小幅回退：保留在当前文件，避免直播抖动导致碎切
                    warn!(
                        "关键帧处 DTS 小幅回退，忽略切分 previous={prev_timestamp} current={} delta_ms={}",
                        flv_tag.header.timestamp,
                        flv_tag.header.timestamp as i64 - prev_timestamp as i64
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
                    prev_timestamp = 0;
                    output_timestamp_base = None;
                    current_file_started = false;

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
                            prev_timestamp,
                        );
                        out.write_tag(&output_header, &cached_data, &cached_previous_size)?;
                        segment.increase_size((11 + cached_header.data_size + 4) as u64);
                        if is_media {
                            prev_timestamp = cached_header.timestamp;
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
                        prev_timestamp,
                    );
                    out.write_tag(&output_header, &bytes, &previous_tag_size)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    prev_timestamp = tag_header.timestamp;
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
            prev_timestamp,
        );
        out.write_tag(&output_header, &flv_tag_data, &previous_tag_size_bytes)?;
        if is_media {
            prev_timestamp = tag_header.timestamp;
        }
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

/// 写入前：先按 source 前跳压平 base，再 rebase 到段内时间轴。
fn rebase_media_timestamp_for_write(
    header: &TagHeader,
    is_media: bool,
    output_timestamp_base: &mut Option<u32>,
    prev_source_ms: u32,
) -> TagHeader {
    if is_media {
        if let Some(absorbed) =
            absorb_forward_timestamp_jump(prev_source_ms, header.timestamp, output_timestamp_base)
        {
            warn!(
                "检测到时间戳前跳，已压平输出时间轴 previous={prev_source_ms} current={} absorbed_ms={absorbed} delta_ms={}",
                header.timestamp,
                header.timestamp as i64 - prev_source_ms as i64
            );
        }
    }
    rebase_media_timestamp(header, is_media, output_timestamp_base)
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
        // 先写入 prev 对应的输出点
        assert_eq!(
            rebase_media_timestamp_for_write(&prev, true, &mut base, 0).timestamp,
            8_141_000
        );
        let out = rebase_media_timestamp_for_write(&jumped, true, &mut base, prev.timestamp);
        // 48s 空洞被吸收，仅保留 1ms 递增
        assert_eq!(out.timestamp, 8_141_001);
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
}
