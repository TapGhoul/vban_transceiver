#[cfg(not(target_endian = "little"))]
compile_error!("This software only supports little endian at this time.");

use crate::stream::resolution::VBANResolution;
use crate::stream::stream_name::StreamName;
use crate::stream::try_parse_header;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SizedSample, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::env::args;
use std::error::Error;
use std::io::{Cursor, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::process::exit;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod stream;

type SampleFormat = i16;
const SAMPLE_BYTE_SIZE: usize = size_of::<SampleFormat>();
const LISTEN_PORT: u16 = 6980;
const MAX_VBAN_PACKET_SIZE: usize = 1436;

// 256 is too low, have to go at least 512 cus system goes crunchy
// Testing 128 now with added audio_thread_priority feature
const CHANNEL_BUFFER_SIZE: cpal::FrameCount = 128;

fn main() {
    let (addr, buffer_multiplier) = match get_args() {
        Ok(v) => v,
        Err(err) => {
            let bin = args().next().unwrap();
            println!("Error: {err:?}\nUsage: {bin} <ip> [buffer_multiplier]");
            exit(-1);
        }
    };
    println!("Starting with buffer multiplier of {buffer_multiplier}");

    let stream_name = StreamName::try_from("Stream1").unwrap();
    let _sender_handle = start_mic(addr, &stream_name);

    let buffer_invalid = Arc::new(AtomicBool::new(false));
    let buffer_max_capacity = (MAX_VBAN_PACKET_SIZE / SAMPLE_BYTE_SIZE)
        .max(CHANNEL_BUFFER_SIZE as usize * 2 * buffer_multiplier.get())
        * 16;

    // TODO: cache-align sample sets by buffer size - crossbeam_utils::CachePadded might do the trick
    let (mut producer, consumer) = HeapRb::<SampleFormat>::new(buffer_max_capacity).split();
    let _output_handle = start_speaker(buffer_multiplier, consumer, buffer_invalid.clone());

    run_receiver(addr, &stream_name, buffer_invalid, &mut producer);
}

fn get_args() -> Result<(SocketAddr, NonZeroUsize), Box<dyn Error>> {
    let mut args = args().skip(1);

    let ip = args
        .next()
        .ok_or_else(|| "No address provided".to_string())
        .and_then(|e| IpAddr::from_str(&e).map_err(|e| format!("Bad address: {e:?}")))?;

    let buffer_size = args
        .next()
        .map(|v| NonZeroUsize::from_str(&v).map_err(|e| format!("Bad buffer multiplier: {e:?}")))
        .transpose()?
        .unwrap_or_else(|| unsafe { NonZeroUsize::new_unchecked(1) });

    Ok((SocketAddr::from((ip, LISTEN_PORT)), buffer_size))
}

fn start_mic(addr: SocketAddr, stream_name: &StreamName) -> Stream {
    let send_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    send_sock.connect(addr).unwrap();

    println!("Sending to {addr} on {stream_name}");

    let stream_name = stream_name.clone();

    create_mic({
        let mut idx: u32 = 0;
        let mut buf = [0u8; MAX_VBAN_PACKET_SIZE];
        let mut net_borked = false;

        move |samples: &[SampleFormat]| {
            if net_borked {
                let mut send_buf = Cursor::new(&mut buf[..]);
                stream::write_header(
                    &mut send_buf,
                    // TODO: Avoid a clone here
                    stream_name.clone(),
                    idx,
                    VBANResolution::S16,
                    0u8,
                );

                let curr_offset = send_buf.position() as usize;
                let final_buf = &send_buf.into_inner()[..curr_offset];

                if send_sock.send(final_buf).is_ok() {
                    // We fall through our check here - we sent 0 samples, so there won't be any buffer bloat.
                    net_borked = false;
                    println!("Mic network recovered");
                } else {
                    return;
                }
            }

            // Max bytes: 1436
            // Max samples: 256
            let chunk_size = (MAX_VBAN_PACKET_SIZE / SAMPLE_BYTE_SIZE).min(256);
            for chunk in samples.chunks(chunk_size) {
                let sample_count = chunk.len();

                idx = idx.wrapping_add(1);

                let mut send_buf = Cursor::new(&mut buf[..]);
                stream::write_header(
                    &mut send_buf,
                    // TODO: Avoid a clone here
                    stream_name.clone(),
                    idx,
                    VBANResolution::S16,
                    (sample_count - 1) as u8,
                );

                // If this is ever an issue, we could replace it with the unsafe "ptr::copy_nonoverlapping()"
                // and just do the length checking ahead of time rather than in each iteration
                send_buf.write_all(bytemuck::cast_slice(chunk)).unwrap();

                let final_buf = &send_buf.get_ref()[..send_buf.position() as _];
                if let Err(err) = send_sock.send(final_buf) {
                    println!("WARN: Failed to send data: {err:?}");
                    net_borked = true;
                    return;
                }
            }
        }
    })
}

fn create_mic<T, D>(mut cb: D) -> Stream
where
    D: FnMut(&[T]) + Send + 'static,
    T: SizedSample,
{
    let host = cpal::default_host();
    let mic = host.default_input_device().unwrap();

    mic.build_input_stream(
        &StreamConfig {
            channels: 1,
            buffer_size: BufferSize::Fixed(CHANNEL_BUFFER_SIZE),
            sample_rate: 22050,
        },
        move |data: &[T], _| cb(data),
        |err| panic!("{err:?}"),
        None,
    )
    .unwrap()
}

fn start_speaker(
    buffer_multiplier: NonZeroUsize,
    mut audio_buffer: HeapCons<SampleFormat>,
    audio_invalid_flag: Arc<AtomicBool>,
) -> Stream {
    // TODO: Figure out a better way to deal with latency
    let mut is_warming_buffer = true;
    let buffer_invalid = audio_invalid_flag.clone();

    const AUDIO_DATA_SIZE: usize = CHANNEL_BUFFER_SIZE as usize * 2;
    let min_buffer_fill = AUDIO_DATA_SIZE * buffer_multiplier.get();

    create_speaker(move |data: &mut [i16]| {
        // Relaxed ordering is fine here, as this is just a flag - the only data it's "protecting" (not really) is itself atomic.
        if buffer_invalid
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            println!("Cleared invalid buffer");
            // Avoid a screech
            data.fill(0);
            audio_buffer.clear();
            is_warming_buffer = true;
        } else if is_warming_buffer && audio_buffer.occupied_len() >= min_buffer_fill {
            println!("Buffer warmed!");
            is_warming_buffer = false;
        } else if !is_warming_buffer && audio_buffer.occupied_len() < AUDIO_DATA_SIZE {
            println!("WARN: Buffer underrun");
            // Avoid a screech
            data.fill(0);
            is_warming_buffer = true;
        }

        if is_warming_buffer {
            return;
        }

        audio_buffer.pop_slice(data);
    })
}

fn create_speaker<T, D>(mut cb: D) -> Stream
where
    D: FnMut(&mut [T]) + Send + 'static,
    T: SizedSample,
{
    let host = cpal::default_host();
    let mic = host.default_input_device().unwrap();

    mic.build_output_stream(
        &StreamConfig {
            channels: 2,
            buffer_size: BufferSize::Fixed(CHANNEL_BUFFER_SIZE * 2),
            sample_rate: 48000,
        },
        move |data: &mut [T], _| cb(data),
        |err| panic!("{err:?}"),
        None,
    )
    .unwrap()
}

fn run_receiver(
    mut addr: SocketAddr,
    stream_name: &StreamName,
    buffer_invalid: Arc<AtomicBool>,
    producer: &mut HeapProd<SampleFormat>,
) {
    let recv_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, LISTEN_PORT)).unwrap();
    addr.set_port(0);
    recv_sock.connect(addr).unwrap();

    let mut buf = [0u8; MAX_VBAN_PACKET_SIZE];
    let mut next_expected_frame = None;

    loop {
        let len = recv_sock.recv(&mut buf).unwrap();
        let mut buf = Cursor::new(&buf[..len]);

        let Some((frame, sample_count)) = try_parse_header(stream_name, &mut buf) else {
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

        let buf_offset = buf.position() as usize;
        let buf = &buf.into_inner()[buf_offset..];

        if !buf.len().is_multiple_of(SAMPLE_BYTE_SIZE) {
            // We don't actually care if the buffers aren't identical, just that the data is valid.
            // We can be lazy this way wheeeeee
            println!(
                "WARN: VBAN protocol violation - buffer is not a multiple of sample byte size!"
            );
        }

        let added_count = producer.push_slice(bytemuck::cast_slice(buf));

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
