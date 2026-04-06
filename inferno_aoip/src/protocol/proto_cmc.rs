use crate::device_info::DeviceId;

pub const PORT: u16 = 8800;

// CMC packets from DC (controller) use start code 0x1000.
// CMC response packets from devices (including Shure hardware) use 0x1200.
// DC validates this — echoing the request's 0x1000 back causes DC to reject the response
// and never send the CMC 0x3010 controller-registration keepalive.
pub const RESPONSE_START_CODE: u16 = 0x1200;

pub const REQUEST_DEVICE_ADVERTISEMENT: u16 = 0x1001;
// Sent by DC to devices it considers fully manageable; device echoes it back.
// This bidirectional exchange is what enables management buttons in DC Device Config.
pub const REGISTER_CONTROLLER: u16 = 0x3010;

#[derive(Debug, binary_serde::BinarySerde, Default)]
pub struct DeviceAdvertisement {
  pub process_id: u16,
  pub factory_device_id: DeviceId,
  pub unknown1_1: u16,
  pub unknown2_0: u16,
  pub ip_address: [u8; 4],
  pub info_request_port: u16,
  pub unknown3_0: u16,
}
