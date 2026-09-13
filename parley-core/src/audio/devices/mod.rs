// Audio device model + PipeWire-backed discovery.

pub mod configuration;
pub mod discovery;

pub use configuration::{AudioDevice, DeviceType};
pub use discovery::{list_audio_devices, trigger_audio_permission};
