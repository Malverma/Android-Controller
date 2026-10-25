//! scrcpy 4.x client side: server launch, video demuxer and control
//! protocol (spec §4). The device-side agent is upstream `scrcpy-server`.

pub mod control;
pub mod demux;
pub mod device_msg;
pub mod launch;
pub mod server;

pub use control::{ControlMsg, KeyAction, MotionAction, Position};
pub use demux::{DemuxError, DemuxItem, Demuxer, VideoCodec};
pub use device_msg::DeviceMsg;
pub use launch::{Encoder, LaunchError, Launched, ServerProcess, Stage, launch, list_encoders, parse_encoders};
pub use server::{DEVICE_JAR, ServerParams, detect_version, is_compatible, parse_version};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("packet size {0} out of range")]
    Oversize(usize),
    #[error("invalid session size {0}x{1}")]
    BadSession(u32, u32),
    #[error("unknown codec id {0:#010x}")]
    UnknownCodec(u32),
    #[error("unknown device message type {0}")]
    UnknownDeviceMsg(u8),
}
