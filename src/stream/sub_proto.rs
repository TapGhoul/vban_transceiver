use deku::{DekuRead, DekuWrite};
use strum::Display;

#[derive(Debug, Clone, PartialEq, DekuRead, DekuWrite, Display)]
#[deku(id_type = "u8", bits = 3)]
#[repr(u8)]
pub enum SubProto {
    Audio = 0,
    Serial,
    Txt,
    Service,
    Undefined1,
    Undefined2,
    Undefined3,
    User,
}
