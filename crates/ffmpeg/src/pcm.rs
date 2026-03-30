//! Decode audio bytes to raw PCM samples.

use crate::error::{check, check_again, FfmpegError};
use crate::handle::{find_decoder, CodecContext, Frame, Packet, Resampler};
use crate::io::InputContext;
use ffmpeg_sys::*;
use std::ptr;

/// Decoded PCM audio data.
pub struct PcmData {
    pub samples: Vec<f32>,
    pub sample_rate: i32,
}

/// Decode audio bytes to mono f32 PCM samples.
pub fn decode_to_pcm(data: bytes::Bytes) -> Result<PcmData, FfmpegError> {
    crate::init();

    let input = InputContext::open(data)?;
    let audio_stream_idx = input.find_audio_stream()?;
    let stream = input.stream(audio_stream_idx);

    // Set up decoder
    let codecpar = unsafe { (*stream).codecpar };
    let codec_id = unsafe { (*codecpar).codec_id };
    let decoder_codec = find_decoder(codec_id)?;

    let mut decoder = CodecContext::new(decoder_codec)?;
    decoder.set_parameters(codecpar)?;
    decoder.open(decoder_codec)?;

    let sample_rate = decoder.sample_rate();

    // Set up resampler: convert to mono f32 at original sample rate
    let mut mono_layout: AVChannelLayout = unsafe { std::mem::zeroed() };
    unsafe { av_channel_layout_default(&mut mono_layout, 1) };

    let mut resampler = Resampler::new()?;
    resampler.configure(
        decoder.ch_layout(),
        decoder.sample_fmt(),
        sample_rate,
        &mono_layout,
        AVSampleFormat::AV_SAMPLE_FMT_FLT,
        sample_rate,
    )?;

    let mut pkt = Packet::new()?;
    let mut frame = Frame::new()?;
    let mut resampled_frame = Frame::new()?;
    let mut samples = Vec::new();

    // Helper: set output frame properties so swr_convert_frame sees a
    // consistent format on every call (avoids "Output changed" errors).
    let prepare_out = |out: &mut Frame| unsafe {
        let p = out.as_mut_ptr();
        (*p).format = AVSampleFormat::AV_SAMPLE_FMT_FLT as i32;
        (*p).sample_rate = sample_rate;
        av_channel_layout_copy(&mut (*p).ch_layout, &mono_layout);
    };

    // Read and decode packets
    loop {
        let ret = unsafe { av_read_frame(input.format_ctx(), pkt.as_mut_ptr()) };
        if ret < 0 {
            if ffmpeg_sys::is_eof(ret) {
                break;
            }
            check(ret, "av_read_frame")?;
        }

        if pkt.stream_index() as usize != audio_stream_idx {
            pkt.unref();
            continue;
        }

        check(
            unsafe { avcodec_send_packet(decoder.as_mut_ptr(), pkt.as_ptr()) },
            "avcodec_send_packet",
        )?;

        loop {
            let ret = unsafe { avcodec_receive_frame(decoder.as_mut_ptr(), frame.as_mut_ptr()) };
            if !check_again(ret, "avcodec_receive_frame")? {
                break;
            }

            prepare_out(&mut resampled_frame);
            resampler.convert_frame(&mut resampled_frame, &frame)?;
            collect_f32_samples(&resampled_frame, &mut samples);
            frame.unref();
            resampled_frame.unref();
        }

        pkt.unref();
    }

    // Flush decoder
    check(
        unsafe { avcodec_send_packet(decoder.as_mut_ptr(), ptr::null()) },
        "avcodec_send_packet (flush)",
    )?;

    loop {
        let ret = unsafe { avcodec_receive_frame(decoder.as_mut_ptr(), frame.as_mut_ptr()) };
        if !check_again(ret, "avcodec_receive_frame (flush)")? {
            break;
        }

        prepare_out(&mut resampled_frame);
        resampler.convert_frame(&mut resampled_frame, &frame)?;
        collect_f32_samples(&resampled_frame, &mut samples);
        frame.unref();
        resampled_frame.unref();
    }

    // Flush resampler
    prepare_out(&mut resampled_frame);
    resampler.flush(&mut resampled_frame)?;
    if resampled_frame.nb_samples() > 0 {
        collect_f32_samples(&resampled_frame, &mut samples);
    }

    Ok(PcmData {
        samples,
        sample_rate,
    })
}

fn collect_f32_samples(frame: &Frame, samples: &mut Vec<f32>) {
    let nb_samples = frame.nb_samples();
    if nb_samples <= 0 {
        return;
    }
    unsafe {
        let data_ptr = (*frame.as_ptr()).data[0] as *const f32;
        let slice = std::slice::from_raw_parts(data_ptr, nb_samples as usize);
        samples.extend_from_slice(slice);
    }
}
