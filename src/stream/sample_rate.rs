use deku::prelude::*;
use strum::Display;

#[derive(Debug, Clone, PartialEq, DekuRead, DekuWrite, Display)]
#[deku(id_type = "u8", bits = 5)]
#[repr(u8)]
pub enum VBANSampleRate {
    Rate6000 = 0,
    Rate12000,
    Rate24000,
    Rate48000,
    Rate96000,
    Rate192000,
    Rate384000,

    Rate8000,
    Rate16000,
    Rate32000,
    Rate64000,
    Rate128000,
    Rate256000,
    Rate512000,

    Rate11025,
    Rate22050,
    Rate44100,
    Rate88200,
    Rate176400,
    Rate352800,
    Rate705600,
}
