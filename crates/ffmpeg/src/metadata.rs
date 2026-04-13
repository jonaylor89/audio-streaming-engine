//! In-process metadata extraction using FFmpeg C structs.

use crate::error::FfmpegError;
use crate::io::InputContext;
use ffmpeg_sys::*;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ptr;

/// Metadata extracted from an audio file.
#[derive(Debug, Clone)]
pub struct AudioFileMetadata {
    pub format: String,
    pub duration: Option<f64>,
    pub bit_rate: Option<i64>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub codec: Option<String>,
    pub size: Option<i64>,
    pub tags: HashMap<String, String>,
}

/// Extract metadata from raw audio bytes without spawning a subprocess.
///
/// This opens the data with FFmpeg's demuxer, reads format-level and
/// stream-level information, and returns it as an [`AudioFileMetadata`].
pub fn extract_metadata(data: bytes::Bytes) -> Result<AudioFileMetadata, FfmpegError> {
    crate::init();

    let data_len = data.len();
    let input = InputContext::open(data)?;
    let format_ctx = input.format_ctx();

    // Format name from AVInputFormat
    let format = unsafe {
        let iformat = (*format_ctx).iformat;
        if !iformat.is_null() && !(*iformat).name.is_null() {
            CStr::from_ptr((*iformat).name)
                .to_string_lossy()
                .into_owned()
        } else {
            "unknown".to_string()
        }
    };

    // Duration (AV_TIME_BASE units → seconds)
    let duration = unsafe {
        let d = (*format_ctx).duration;
        // AV_NOPTS_VALUE is i64::MIN
        if d != i64::MIN {
            Some(d as f64 / AV_TIME_BASE as f64)
        } else {
            None
        }
    };

    // Bit rate
    let bit_rate = unsafe {
        let br = (*format_ctx).bit_rate;
        if br > 0 {
            Some(br)
        } else {
            None
        }
    };

    // Format-level tags
    let mut tags = unsafe { read_dict((*format_ctx).metadata) };

    // Audio stream info
    let (sample_rate, channels, codec) = if let Ok(stream_idx) = input.find_audio_stream() {
        let stream = input.stream(stream_idx);
        let codecpar = unsafe { (*stream).codecpar };

        let sample_rate = unsafe { (*codecpar).sample_rate };
        let channels = unsafe { (*codecpar).ch_layout.nb_channels };
        let codec_name = unsafe {
            let name_ptr = avcodec_get_name((*codecpar).codec_id);
            if !name_ptr.is_null() {
                Some(CStr::from_ptr(name_ptr).to_string_lossy().into_owned())
            } else {
                None
            }
        };

        // Merge stream-level tags
        let stream_tags = unsafe { read_dict((*stream).metadata) };
        for (k, v) in stream_tags {
            tags.entry(k).or_insert(v);
        }

        (
            if sample_rate > 0 {
                Some(sample_rate)
            } else {
                None
            },
            if channels > 0 { Some(channels) } else { None },
            codec_name,
        )
    } else {
        (None, None, None)
    };

    let size = Some(data_len as i64);

    Ok(AudioFileMetadata {
        format,
        duration,
        bit_rate,
        sample_rate,
        channels,
        codec,
        size,
        tags,
    })
}

/// Read all entries from an AVDictionary into a HashMap.
unsafe fn read_dict(dict: *mut AVDictionary) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if dict.is_null() {
        return map;
    }
    let mut entry: *const AVDictionaryEntry = ptr::null();
    loop {
        entry = av_dict_iterate(dict, entry);
        if entry.is_null() {
            break;
        }
        let key = CStr::from_ptr((*entry).key).to_string_lossy().into_owned();
        let value = CStr::from_ptr((*entry).value)
            .to_string_lossy()
            .into_owned();
        map.insert(key, value);
    }
    map
}
