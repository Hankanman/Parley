use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub enum DeviceType {
    Input,
    Output,
}

/// A selected audio device.
///
/// `name` is the stable PipeWire `node.name` (e.g.
/// `alsa_input.usb-RODE_Microphones_RODE_NT-USB-00.pro-input-0`) or the
/// literal `"default"` for the system default source/sink. Which stream
/// a device drives (microphone vs system) is decided by the *parameter*
/// it is passed as — never parsed out of the string.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Debug)]
pub struct AudioDevice {
    pub name: String,
    pub device_type: DeviceType,
}

impl AudioDevice {
    pub fn new(name: String, device_type: DeviceType) -> Self {
        AudioDevice { name, device_type }
    }
}

impl fmt::Display for AudioDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} ({})",
            self.name,
            match self.device_type {
                DeviceType::Input => "input",
                DeviceType::Output => "output",
            }
        )
    }
}
