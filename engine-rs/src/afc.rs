//! The real iPhone over USB. (placeholder until the protocol client is written)

use crate::device::{Device, DeviceInfo, RemoteFile};

pub struct AfcDevice;

impl AfcDevice {
    pub fn new() -> AfcDevice {
        AfcDevice
    }
}

const MSG: &str = "NoDeviceConnectedError: the iPhone link is not available yet";

impl Device for AfcDevice {
    fn list_files(&mut self, _r: &[String], _i: &[String], _e: &mut dyn FnMut(RemoteFile) -> bool) -> Result<(), String> {
        Err(MSG.into())
    }
    fn read_range(&mut self, _p: &str, _o: u64, _n: usize) -> Result<Vec<u8>, String> {
        Err(MSG.into())
    }
    fn reconnect(&mut self) -> Result<(), String> {
        Err(MSG.into())
    }
    fn info(&mut self) -> Result<DeviceInfo, String> {
        Err(MSG.into())
    }
}
