use crate::downloader::error::{Error, Result};
use crate::downloader::util::{LifecycleFile, Segmentable};
use m3u8_rs::{MediaPlaylist, Playlist};

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use url::Url;

use crate::client::StatelessClient;

pub type SegmentBoundaryHook<'a> = Box<dyn FnMut(&str) + Send + Sync + 'a>;
const MIN_PLAYLIST_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn parse_media_playlist(bytes: &[u8]) -> Result<MediaPlaylist> {
    m3u8_rs::parse_media_playlist(bytes)
        .map(|(_, playlist)| playlist)
        .map_err(|error| Error::Custom(format!("Unable to parse media playlist content: {error}")))
}

fn playlist_poll_interval(playlist: &MediaPlaylist) -> Duration {
    Duration::from_secs(playlist.target_duration).max(MIN_PLAYLIST_POLL_INTERVAL)
}

fn playlist_should_refresh(playlist: &MediaPlaylist) -> bool {
    !playlist.end_list
}

pub async fn download(
    url: &str,
    client: &StatelessClient,
    file: LifecycleFile<'_>,
    splitting: Segmentable,
) -> Result<()> {
    download_with_boundaries(
        url,
        client,
        file,
        splitting,
        Box::new(|_| {}),
        Box::new(|_| {}),
    )
    .await
}

pub async fn download_with_boundaries(
    url: &str,
    client: &StatelessClient,
    file: LifecycleFile<'_>,
    mut splitting: Segmentable,
    mut segment_started: SegmentBoundaryHook<'_>,
    mut segment_ended: SegmentBoundaryHook<'_>,
) -> Result<()> {
    info!("Downloading {}...", url);
    let resp = client.retryable(url).await?;
    info!("{}", resp.status());
    // let mut resp = resp.bytes_stream();
    let bytes = resp.bytes().await?;
    let mut ts_file = TsFile::new(file)?;

    let mut media_url = Url::parse(url)?;
    let mut pl = match m3u8_rs::parse_playlist(&bytes) {
        Ok((_i, Playlist::MasterPlaylist(pl))) => {
            info!("Master playlist:\n{:#?}", pl);
            // Pick the highest-bandwidth playable variant. The first variant is not
            // necessarily the best quality (e.g. Twitch orders transcodes ahead of the
            // source), so prefer the highest-bandwidth stream that carries a resolution.
            // Skip I-frame (trick-play) streams, which are not full playable renditions.
            // Fall back to the highest-bandwidth non-I-frame variant, then the first one.
            let best = pl
                .variants
                .iter()
                .filter(|v| !v.is_i_frame && v.resolution.is_some())
                .max_by_key(|v| v.bandwidth)
                .or_else(|| {
                    pl.variants
                        .iter()
                        .filter(|v| !v.is_i_frame)
                        .max_by_key(|v| v.bandwidth)
                })
                .unwrap_or(&pl.variants[0]);
            info!(
                "Selected variant: bandwidth={}, resolution={:?}, video={:?}",
                best.bandwidth, best.resolution, best.video
            );
            media_url = media_url.join(&best.uri)?;
            info!("media url: {media_url}");
            let resp = client.retryable(media_url.as_str()).await?;
            let bs = resp.bytes().await?;
            parse_media_playlist(&bs)?
        }
        Ok((_i, Playlist::MediaPlaylist(pl))) => {
            info!("Media playlist:\n{:#?}", pl);
            info!("index {}", pl.media_sequence);
            pl
        }
        Err(e) => return Err(Error::Custom(format!("Parsing playlist error: {e}"))),
    };
    let mut last_sequence = None::<u64>;
    let mut current_file_started = false;
    let mut last_playlist_load = Instant::now();
    let result = async {
        loop {
        if pl.segments.is_empty() {
            debug!("Segments array is empty - waiting for playlist update");
        }
        let mut seq = pl.media_sequence;
        for segment in &pl.segments {
            if should_download_sequence(last_sequence, seq) {
                let sequence_gap = has_sequence_gap(last_sequence, seq);
                if sequence_gap {
                    warn!(last = ?last_sequence, current = seq, "HLS media sequence gap");
                }
                debug!("Yield segment");
                let anomaly_boundary =
                    splitting.timestamp_anomaly_threshold_ms() > 0 && sequence_gap;
                if anomaly_boundary || segment.discontinuity {
                    if current_file_started {
                        segment_ended(&ts_file.file.file_name);
                    }
                    warn!("#EXT-X-DISCONTINUITY");
                    ts_file.create_new()?;
                    splitting.reset();
                    current_file_started = false;
                }
                let file_name = ts_file.file.file_name.clone();
                let length = download_to_file(
                    media_url.join(&segment.uri)?,
                    client,
                    &mut ts_file.buf_writer,
                    || {
                        if !current_file_started {
                            segment_started(&file_name);
                            current_file_started = true;
                        }
                    },
                )
                .await?;
                splitting.increase_size(length);
                splitting.increase_time(Duration::from_secs_f64(segment.duration as f64));
                if splitting.needed() {
                    if current_file_started {
                        segment_ended(&ts_file.file.file_name);
                    }
                    ts_file.create_new()?;
                    splitting.reset();
                    current_file_started = false;
                }
                last_sequence = Some(seq);
            }
            seq += 1;
        }

        if !playlist_should_refresh(&pl) {
            info!("#EXT-X-ENDLIST received - stream finished");
            break;
        }

        let poll_interval = playlist_poll_interval(&pl);
        let refresh_delay = poll_interval.saturating_sub(last_playlist_load.elapsed());
        if !refresh_delay.is_zero() {
            debug!("Waiting {refresh_delay:?} before refreshing media playlist");
            tokio::time::sleep(refresh_delay).await;
        }

        let resp = client.retryable(media_url.as_str()).await?;
        let bs = resp.bytes().await?;
        let playlist = parse_media_playlist(&bs)?;
        if splitting.timestamp_anomaly_threshold_ms() > 0
            && is_sequence_regression(last_sequence, playlist_last_sequence(&playlist))
        {
            warn!(
                "检测到 HLS media sequence 回退，准备切分文件 previous_last={:?} new_start={} new_last={:?}",
                last_sequence,
                playlist.media_sequence,
                playlist_last_sequence(&playlist)
            );
            if current_file_started {
                segment_ended(&ts_file.file.file_name);
            }
            ts_file.create_new()?;
            splitting.reset();
            current_file_started = false;
            last_sequence = None;
        }
        pl = playlist;
        last_playlist_load = Instant::now();
        }
        Ok(())
    }
    .await;

    if current_file_started {
        segment_ended(&ts_file.file.file_name);
    }
    drop(ts_file);
    info!("Done...");
    result
}

fn playlist_last_sequence(playlist: &m3u8_rs::MediaPlaylist) -> Option<u64> {
    (!playlist.segments.is_empty()).then(|| {
        playlist
            .media_sequence
            .saturating_add(playlist.segments.len() as u64 - 1)
    })
}

const HLS_SEQUENCE_REGRESSION_TOLERANCE: u64 = 5;

fn is_sequence_regression(previous: Option<u64>, current: Option<u64>) -> bool {
    matches!((previous, current), (Some(previous), Some(current)) if current.saturating_add(HLS_SEQUENCE_REGRESSION_TOLERANCE) < previous)
}

fn should_download_sequence(previous: Option<u64>, current: u64) -> bool {
    previous.is_none_or(|previous| current > previous)
}

fn has_sequence_gap(previous: Option<u64>, current: u64) -> bool {
    previous.is_some_and(|previous| current > previous.saturating_add(1))
}

async fn download_to_file<F>(
    url: Url,
    client: &StatelessClient,
    out: &mut impl Write,
    mut on_first_chunk: F,
) -> Result<u64>
where
    F: FnMut(),
{
    debug!("url: {url}");
    let mut response = client.retryable(url.as_str()).await?;
    let mut length: u64 = 0;
    let mut started = false;
    while let Some(chunk) = response.chunk().await? {
        if !started {
            on_first_chunk();
            started = true;
        }
        length += chunk.len() as u64;
        out.write_all(&chunk)?;
    }
    // let mut out = File::options()
    //     .append(true)
    //     .open(format!("{file_name}.ts"))?;
    // let length = response.copy_to(out)?;
    Ok(length)
}

pub struct TsFile<'a> {
    pub buf_writer: BufWriter<File>,
    pub file: LifecycleFile<'a>,
}

impl<'a> TsFile<'a> {
    pub fn new(mut file: LifecycleFile<'a>) -> std::io::Result<Self> {
        let path = file.create()?;
        Ok(Self {
            buf_writer: Self::create(path)?,
            file,
        })
    }

    pub fn create_new(&mut self) -> std::io::Result<()> {
        self.buf_writer.flush()?;
        self.file.rename();
        let path = self.file.create()?;
        self.buf_writer = Self::create(path)?;
        Ok(())
    }

    fn create<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<BufWriter<File>> {
        let path = path.as_ref();
        let out = match File::create(path) {
            Ok(o) => o,
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("Unable to create file {}", path.display()),
                ));
            }
        };
        info!("create file {}", path.display());
        Ok(BufWriter::new(out))
    }
}

impl Drop for TsFile<'_> {
    fn drop(&mut self) {
        let _ = self.buf_writer.flush();
        self.file.rename()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        has_sequence_gap, is_sequence_regression, parse_media_playlist, playlist_poll_interval,
        playlist_should_refresh, should_download_sequence,
    };
    use m3u8_rs::MediaPlaylist;
    use reqwest::Url;
    use std::time::Duration;

    #[test]
    fn test_url() -> Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("h://host.path/to/remote/resource.m3u8")?;
        let scheme = url.scheme();
        let new_url = url.join("http://path.host/remote/resource.ts")?;
        println!("{url}, {scheme}");
        println!("{new_url}, {scheme}");
        Ok(())
    }

    #[test]
    fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        // download(
        //     "test.ts")?;
        Ok(())
    }

    #[test]
    fn media_sequence_zero_is_a_valid_first_segment() {
        assert!(should_download_sequence(None, 0));
        assert!(!should_download_sequence(Some(0), 0));
        assert!(should_download_sequence(Some(0), 1));
    }

    #[test]
    fn overlapping_playlist_window_does_not_regress_or_create_a_gap() {
        // Previous playlist ended at 102; the next normal sliding window is
        // 101, 102, 103. Only 103 is new.
        assert!(!should_download_sequence(Some(102), 101));
        assert!(!should_download_sequence(Some(102), 102));
        assert!(should_download_sequence(Some(102), 103));
        assert!(!has_sequence_gap(Some(102), 103));
        assert!(!is_sequence_regression(Some(102), Some(103)));
    }

    #[test]
    fn detects_large_regression_and_forward_gap() {
        assert!(is_sequence_regression(Some(102), Some(2)));
        assert!(!is_sequence_regression(Some(102), Some(101)));
        assert!(!is_sequence_regression(Some(102), Some(98)));
        assert!(has_sequence_gap(Some(102), 104));
        assert!(!has_sequence_gap(Some(102), 103));
    }

    #[test]
    fn playlist_poll_interval_uses_target_duration() {
        let playlist = MediaPlaylist {
            target_duration: 6,
            ..MediaPlaylist::default()
        };

        assert_eq!(playlist_poll_interval(&playlist), Duration::from_secs(6));
    }

    #[test]
    fn playlist_poll_interval_has_one_second_minimum() {
        assert_eq!(
            playlist_poll_interval(&MediaPlaylist::default()),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn parse_media_playlist_preserves_end_list() {
        let playlist = parse_media_playlist(
            b"#EXTM3U\n\
              #EXT-X-TARGETDURATION:6\n\
              #EXT-X-MEDIA-SEQUENCE:7\n\
              #EXTINF:6.0,\n\
              7.ts\n\
              #EXT-X-ENDLIST\n",
        )
        .expect("valid media playlist should parse");

        assert!(playlist.end_list);
        assert!(!playlist_should_refresh(&playlist));
        assert_eq!(playlist.segments.len(), 1);
    }

    #[test]
    fn parse_media_playlist_returns_error_for_invalid_content() {
        let error = parse_media_playlist(b"not a media playlist")
            .expect_err("invalid media playlist should return an error");

        assert!(error.to_string().contains("Unable to parse media playlist"));
    }
}
