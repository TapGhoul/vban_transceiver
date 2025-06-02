use crate::stream::resolution::VBANResolution;
use crate::stream::sample_rate::VBANSampleRate;
use crate::stream::stream_name::StreamName;
use crate::stream::sub_proto::SubProto;
use deku::prelude::*;
use std::io::{Cursor, Seek};

pub mod resolution;
pub mod sample_rate;
pub mod stream_name;
pub mod sub_proto;

#[derive(Debug, DekuRead, DekuWrite)]
#[deku(magic = b"VBAN")]
pub struct VBANHeader {
    sub_proto: SubProto,
    rate: VBANSampleRate,
    sample_count: u8,
    channel_count: u8,
    #[deku(bits = 4)]
    codec: u8,
    #[deku(pad_bits_before = "1")]
    format_bit: VBANResolution,
    stream_name: StreamName,
    frame: u32,
}

pub fn write_header<const N: usize>(
    buf: &mut Cursor<[u8; N]>,
    stream_name: StreamName,
    frame: u32,
    format_bit: VBANResolution,
    sample_count: u8,
) {
    let written = VBANHeader {
        rate: VBANSampleRate::Rate22050,
        sub_proto: SubProto::Audio,
        sample_count,
        channel_count: 0,
        codec: 0,
        format_bit,
        stream_name,
        frame,
    }
    .to_slice(buf.get_mut())
    .unwrap();

    buf.seek_relative(written as i64).unwrap();
}

pub fn try_parse_header<'a>(
    stream_name: &'a StreamName,
    buf: &'a [u8],
) -> Option<(u32, usize, &'a [u8])> {
    macro_rules! check {
        ($lhs:expr, $rhs:expr, $err:literal) => {
            if $lhs != $rhs {
                println!("WARN: Bad header: {} ({} != {})", $err, $lhs, $rhs);
                return None;
            }
        };
    }

    let ((buf, bit_offset), header) = VBANHeader::from_bytes((buf, 0)).unwrap();
    assert_eq!(bit_offset, 0, "VBAN header is not byte-aligned!");

    check!(header.sub_proto, SubProto::Audio, "subproto");
    check!(&header.stream_name, stream_name, "stream name");
    check!(header.codec, 0, "codec");
    // 2 channels
    check!(header.channel_count, 1, "channel count");
    check!(header.rate, VBANSampleRate::Rate48000, "sample rate");
    check!(header.format_bit, VBANResolution::S16, "format");

    Some((header.frame, header.sample_count as usize + 1, buf))
}

#[allow(dead_code)]
pub fn generate_sin(frame_start: u32, buf: &mut Vec<u8>) {
    buf.reserve_exact(256);
    for idx in 0..256 {
        let p = (frame_start * 256 + idx) as f64 * 440. / 44100. * std::f64::consts::TAU;
        let e = ((p.sin() + 1.) * (u8::MAX / 2) as f64) as u8;
        buf.push(e);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::empty;

    #[test]
    fn header_size() {
        let header = VBANHeader {
            sub_proto: SubProto::Audio,
            rate: VBANSampleRate::Rate48000,
            sample_count: 0,
            channel_count: 1,
            codec: 0,
            format_bit: VBANResolution::U8,
            stream_name: StreamName::try_from("Stream1").unwrap(),
            frame: 0,
        };

        let mut writer = Writer::new(empty());
        header.to_writer(&mut writer, ()).unwrap();
        writer.finalize().unwrap();
        assert_eq!(writer.bits_written, 28 * 8);
    }
}
