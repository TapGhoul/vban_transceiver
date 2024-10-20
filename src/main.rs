use crate::stream::resolution::VBANResolution;
use crate::stream::stream_name::StreamName;
use crate::stream::try_parse_header;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleRate, SizedSample, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::env::args;
use std::io::{Cursor, Seek};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::process::exit;

mod stream;

type SampleFormat = i16;
const SAMPLE_BYTE_SIZE: usize = size_of::<SampleFormat>();

fn main() {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 6980)).unwrap();
    let stream_name = StreamName::try_from("Stream1").unwrap();

    let _stream = {
        let ip: IpAddr = get_ip().parse().unwrap();
        let addr = SocketAddr::from((ip, 6980));
        println!("Sending to {addr} on {stream_name}");

        let stream_name = stream_name.clone();
        let socket = socket.try_clone().unwrap();

        let mut idx: u32 = 0;
        let mut send_buf = Cursor::new([0u8; 1436]);

        setup_mic(move |samples: &[SampleFormat]| {
            // Max bytes: 1436
            // Max samples: 256
            let chunk_size = (1436 / SAMPLE_BYTE_SIZE).min(256);

            for chunk in samples.chunks(chunk_size) {
                let sample_count = chunk.len();

                idx = idx.wrapping_add(1);
                send_buf.rewind().unwrap();

                stream::write_header(
                    &mut send_buf,
                    // TODO: Avoid a clone here
                    stream_name.clone(),
                    idx,
                    VBANResolution::S16,
                    (sample_count - 1) as u8,
                );

                // Realistically, this doesn't actually need a cursor - VBAN's header size is fixed.
                // But ay, why not. I can change it later if I want.
                let curr_offset = send_buf.position() as usize;
                let packet_len = curr_offset + (sample_count * SAMPLE_BYTE_SIZE);

                let sample_dst_buf = &mut send_buf.get_mut()[curr_offset..packet_len];

                // If this is ever an issue, we could replace it with the unsafe "ptr::copy_nonoverlapping()"
                // and just do the length checking ahead of time rather than in each iteration (as per how this works currently)
                for (src, dst) in chunk
                    .into_iter()
                    .map(|e| e.to_le_bytes())
                    .zip(sample_dst_buf.chunks_mut(SAMPLE_BYTE_SIZE))
                {
                    dst.copy_from_slice(src.as_slice())
                }

                let final_buf = &send_buf.get_ref()[..packet_len];
                socket.send_to(final_buf, addr).unwrap();
            }
        })
    };

    let (mut producer, mut consumer) = HeapRb::new(48000 * 30).split();

    let _speaker = {
        let mut is_warming_buffer = true;
        setup_speaker(move |data: &mut [i16]| {
            if is_warming_buffer && consumer.occupied_len() >= data.len() {
                println!("Buffer warmed!");
                is_warming_buffer = false;
            } else if !is_warming_buffer && consumer.occupied_len() < data.len() {
                println!("WARN: Buffer underrun");
                // Avoid a screech
                data.fill(0);
                is_warming_buffer = true;
            }

            if is_warming_buffer {
                return;
            }

            consumer.pop_slice(data);
        })
    };

    let mut buf = [0u8; 1436];
    let mut next_expected_frame = None;

    loop {
        let (len, _) = socket.recv_from(&mut buf).unwrap();
        let Some((frame, sample_count, buf)) = try_parse_header(&stream_name, &buf[..len]) else {
            continue;
        };

        let expected_frame = next_expected_frame.unwrap_or(frame);
        if expected_frame != frame {
            println!("WARN: Discontinuity: expected {expected_frame}, got {frame}");
        }
        next_expected_frame = Some(frame.wrapping_add(1));

        let chunks = buf.chunks_exact(SAMPLE_BYTE_SIZE);
        if !chunks.remainder().is_empty() {
            println!(
                "WARN: VBAN protocol violation - buffer is not a multiple of sample byte size!"
            );
        }

        // TODO: If all samples are 0, don't write - might help with latency? (we don't have an issue with latency atm)

        let added_count =
            producer.push_iter(chunks.map(|e| SampleFormat::from_le_bytes(e.try_into().unwrap())));

        // channels = 2
        let expected_count = sample_count * 2;
        if added_count > expected_count {
            println!("WARN: Buffer Overrun");
        } else if added_count < expected_count {
            println!("WARN: VBAN protocol violation - not enough data!");
        }
    }
}

fn get_ip() -> String {
    let mut args = args();
    let bin = args.next().unwrap();
    let ip = args.next();
    match ip {
        Some(e) => e,
        None => {
            println!("Usage: {bin} <ip>");
            exit(-1);
        }
    }
}

fn setup_mic<T, D>(mut cb: D) -> Stream
where
    D: FnMut(&[T]) + Send + 'static,
    T: SizedSample,
{
    let host = cpal::default_host();
    let mic = host.default_input_device().unwrap();

    mic.build_input_stream(
        &StreamConfig {
            channels: 1,
            // This can probably be lower, but I haven't tested
            buffer_size: BufferSize::Fixed(512),
            sample_rate: SampleRate(22050),
        },
        move |data: &[T], _| cb(data),
        |err| panic!("{err:?}"),
        None,
    )
    .unwrap()
}

fn setup_speaker<T, D>(mut cb: D) -> Stream
where
    D: FnMut(&mut [T]) + Send + 'static,
    T: SizedSample,
{
    let host = cpal::default_host();
    let mic = host.default_input_device().unwrap();

    mic.build_output_stream(
        &StreamConfig {
            channels: 2,
            // Lowest I could get it without OS-level buffer underruns
            buffer_size: BufferSize::Fixed(512),
            sample_rate: SampleRate(48000),
        },
        move |data: &mut [T], _| cb(data),
        |err| panic!("{err:?}"),
        None,
    )
    .unwrap()
}
