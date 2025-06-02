use deku::prelude::*;
use strum::Display;

#[derive(Debug, Clone, PartialEq, DekuRead, DekuWrite, Display)]
#[deku(id_type = "u8", bits = 3)]
#[repr(u8)]
pub enum VBANResolution {
    U8 = 0,
    S16,
    S24,
    S32,
    F32,
    F64,
    S12,
    S10,
}
