use std::path::Path;

use ffmpeg_next::ChannelLayout;

use crate::{EncoderError, FfmpegErrorExt};

pub fn decode_channels<P: AsRef<Path>>(p: &P) -> Result<(Vec<Vec<i16>>, u16), EncoderError> {
    let mut opened = ffmpeg_next::format::input(p).context("couldn't open file", false)?;
    decode_channels_from_input(&mut opened)
}

pub fn decode_channels_from_input(
    opened: &mut ffmpeg_next::format::context::Input,
) -> Result<(Vec<Vec<i16>>, u16), EncoderError> {
    let audio_stream = opened
        .streams()
        .find(|stream| stream.parameters().medium() == ffmpeg_next::media::Type::Audio)
        .ok_or(EncoderError::FfmpegError {
            msg: "could not find audio stream",
            err: None,
            user_fault: true,
        })?;
    let audio_stream_index = audio_stream.index();
    let mut decoder = ffmpeg_next::codec::Context::from_parameters(audio_stream.parameters())
        .context("could not create decoder", true)?
        .decoder()
        .audio()
        .context("not an audio decoder", false)?;

    let mut encoder = ffmpeg_next::codec::Context::new()
        .encoder()
        .audio()
        .unwrap();
    encoder.set_time_base(decoder.time_base());
    encoder.set_format(ffmpeg_next::format::Sample::I16(
        ffmpeg_next::format::sample::Type::Planar,
    ));
    if decoder.channel_layout().is_empty() {
        decoder.set_channel_layout(ChannelLayout::STEREO);
    }
    encoder.set_channel_layout(decoder.channel_layout());
    encoder.set_rate(decoder.rate() as _);
    let mut resampler = decoder
        .resampler(encoder.format(), encoder.channel_layout(), encoder.rate())
        .context("can't create resampler", false)?;
    let mut mp3_raw_frame = ffmpeg_next::util::frame::audio::Audio::empty();
    mp3_raw_frame.set_channel_layout(decoder.channel_layout());
    mp3_raw_frame.set_rate(decoder.rate());
    mp3_raw_frame.set_format(decoder.format());
    let mut i16_raw_frame = ffmpeg_next::util::frame::audio::Audio::empty();
    i16_raw_frame.set_channel_layout(encoder.channel_layout());
    i16_raw_frame.set_rate(encoder.rate());
    i16_raw_frame.set_format(encoder.format());
    // duration is in microseconds
    let estimated_frames =
        (opened.duration() as f64 / 1_000_000f64 * decoder.rate() as f64).ceil() as usize;
    let mut out_channels = vec![Vec::with_capacity(estimated_frames); decoder.channels() as usize];
    for (_, packet) in opened.packets() {
        if packet.stream() == audio_stream_index {
            decoder
                .send_packet(&packet)
                .context("sending packet to decoder failed", false)?;
            receive_decoded_frames(
                &mut decoder,
                &mut resampler,
                &mut mp3_raw_frame,
                &mut i16_raw_frame,
                &mut out_channels,
            )?;
        }
    }
    decoder.flush();
    receive_decoded_frames(
        &mut decoder,
        &mut resampler,
        &mut mp3_raw_frame,
        &mut i16_raw_frame,
        &mut out_channels,
    )?;
    Ok((out_channels, decoder.rate() as u16))
}

// Ok(true) means continue, Ok(false) means break
fn map_receive_result(
    res: Result<(), ffmpeg_next::util::error::Error>,
) -> Result<bool, EncoderError> {
    match res {
        Ok(()) => Ok(true),
        Err(e)
            if e == ffmpeg_next::util::error::Error::Eof
                || e == (ffmpeg_next::util::error::Error::Other {
                    errno: ffmpeg_next::util::error::EAGAIN,
                }) =>
        {
            Ok(false)
        }
        Err(e) => {
            Err(EncoderError::FfmpegError {
                msg: "receive failed",
                err: Some(e),
                user_fault: false,
            })
        }
    }
}

fn receive_decoded_frames(
    decoder: &mut ffmpeg_next::decoder::Audio,
    resampler: &mut ffmpeg_next::software::resampling::Context,
    mp3_raw_frame: &mut ffmpeg_next::frame::Audio,
    i16_raw_frame: &mut ffmpeg_next::frame::Audio,
    out: &mut [Vec<i16>],
) -> Result<(), EncoderError> {
    while map_receive_result(decoder.receive_frame(mp3_raw_frame))? {
        // ???
        if mp3_raw_frame.channel_layout().is_empty() {
            mp3_raw_frame.set_channel_layout(ChannelLayout::STEREO);
        }
        let mut delay = resampler
            .run(mp3_raw_frame, i16_raw_frame)
            .context("resampler failed", false)?;
        send_to_encode(i16_raw_frame, out)?;
        while delay.is_some() {
            delay = resampler
                .flush(i16_raw_frame)
                .context("flush failed", false)?;
            send_to_encode(i16_raw_frame, out)?;
        }
    }
    Ok(())
}

fn send_to_encode(
    i16_raw_frame: &mut ffmpeg_next::frame::Audio,
    out: &mut [Vec<i16>],
) -> Result<(), EncoderError> {
    assert_eq!(out.len(), i16_raw_frame.planes());
    for (plane_idx, chn) in out.iter_mut().enumerate() {
        chn.extend_from_slice(i16_raw_frame.plane(plane_idx));
    }
    Ok(())
}
