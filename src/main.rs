use crate::stream::resolution::VBANResolution;
use crate::stream::stream_name::StreamName;
use crate::stream::try_parse_header;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleRate, SizedSample, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::env::args;
use std::error::Error;
use std::io::{Cursor, Seek};
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs, UdpSocket};
use std::num::NonZeroUsize;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod stream;

type SampleFormat = i16;
const SAMPLE_BYTE_SIZE: usize = size_of::<SampleFormat>();

fn main() {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 6980)).unwrap();
    let stream_name = StreamName::try_from("Stream1").unwrap();
    let (ip, buffer_multiplier) = match get_args() {
        Ok(v) => v,
        Err(err) => {
            let bin = args().next().unwrap();
            println!("Error: {err:?}\nUsage: {bin} <ip> [buffer_multiplier]");
            exit(-1);
        }
    };
    println!("Starting with buffer multiplier of {buffer_multiplier}");

    let _stream = {
        let addr = (ip, 6980).to_socket_addrs().unwrap().next().unwrap();

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

    let buffer_invalid = Arc::new(AtomicBool::new(false));
    let (mut producer, mut consumer) = HeapRb::new(10240).split();

    // TODO: Figure out a better way to deal with latency

    let _speaker = {
        let mut is_warming_buffer = true;
        let buffer_invalid = buffer_invalid.clone();
        setup_speaker(move |data: &mut [i16]| {
            if buffer_invalid.load(Ordering::Acquire) {
                println!("Cleared invalid buffer");
                // Avoid a screech
                data.fill(0);
                consumer.clear();
                is_warming_buffer = true;
                buffer_invalid.store(false, Ordering::Release);
            } else if is_warming_buffer
                && consumer.occupied_len() >= data.len() * buffer_multiplier.get()
            {
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
        let (len, addr) = socket.recv_from(&mut buf).unwrap();
        if addr.ip() != ip {
            continue;
        }

        let Some((frame, sample_count, buf)) = try_parse_header(&stream_name, &buf[..len]) else {
            continue;
        };

        let expected_frame = next_expected_frame.unwrap_or(frame);
        next_expected_frame = Some(frame.wrapping_add(1));

        if buffer_invalid.load(Ordering::Acquire) {
            continue;
        }

        if expected_frame != frame {
            println!("WARN: Discontinuity: expected {expected_frame}, got {frame}");
            buffer_invalid.store(true, Ordering::Release);
        }

        let chunks = buf.chunks_exact(SAMPLE_BYTE_SIZE);
        if !chunks.remainder().is_empty() {
            println!(
                "WARN: VBAN protocol violation - buffer is not a multiple of sample byte size!"
            );
        }

        let added_count =
            producer.push_iter(chunks.map(|e| SampleFormat::from_le_bytes(e.try_into().unwrap())));

        // channels = 2
        let expected_count = sample_count * 2;
        if added_count < expected_count {
            println!("WARN: Buffer overrun");
            buffer_invalid.store(true, Ordering::Release);
        } else if added_count > expected_count {
            println!("WARN: VBAN protocol violation - more data than expected!");
            buffer_invalid.store(true, Ordering::Release);
        }
    }
}

fn get_args() -> Result<(IpAddr, NonZeroUsize), Box<dyn Error>> {
    let mut args = args().skip(1);
    let ip = args
        .next()
        .ok_or_else(|| "No address provided".to_string())
        .and_then(|e| e.parse().map_err(|e| format!("Bad address: {e:?}")))?;
    let buffer_size = args
        .next()
        .map(|v| {
            v.parse()
                .map_err(|e| format!("Bad buffer multiplier: {e:?}"))
        })
        .transpose()?
        .unwrap_or(1);

    Ok((ip, buffer_size.try_into()?))
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
            // 512 is too low, have to go 1024 cus system goes crunchy
            buffer_size: BufferSize::Fixed(1024),
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
            // 512 is too low, have to go 1024 cus system goes crunchy
            buffer_size: BufferSize::Fixed(1024),
            sample_rate: SampleRate(48000),
        },
        move |data: &mut [T], _| cb(data),
        |err| panic!("{err:?}"),
        None,
    )
    .unwrap()
}
