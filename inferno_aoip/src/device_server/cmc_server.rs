use crate::common::*;
use crate::protocol::req_resp::CODE_OK;
use std::sync::Arc;

use crate::device_info::DeviceInfo;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::req_resp;
use crate::protocol::proto_cmc::*;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;

pub async fn run_server(self_info: Arc<DeviceInfo>, shutdown: BroadcastReceiver<()>) {
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), self_info.cmc_port, shutdown).await;
  let mut conn = req_resp::Connection::new(server);
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  while conn.should_work() {
    let request = match conn.recv(&mut recv_buff).await {
      Some(v) => v,
      None => continue,
    };

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        REQUEST_DEVICE_ADVERTISEMENT => {
          let adv = DeviceAdvertisement {
            process_id: self_info.process_id,
            factory_device_id: self_info.factory_device_id,
            unknown1_1: 1,
            unknown2_0: 0,
            ip_address: self_info.ip_address.octets(),
            info_request_port: self_info.info_request_port,
            unknown3_0: 0,
          };
          // DC sends CMC requests with start code 0x1000; devices must respond with 0x1200.
          // DC validates this and only sends the 0x3010 controller-registration keepalive
          // (which gates management button availability) to devices that reply correctly.
          conn.respond_with_struct_start(RESPONSE_START_CODE, CODE_OK, adv).await;
        }
        REGISTER_CONTROLLER => {
          // Echo the 0x3010 registration packet back verbatim — this is what real Dante devices do.
          // DC sends this every ~3.25s; the bidirectional exchange enables Device Config buttons.
          let content = request.content().to_vec();
          conn.respond_with_code_start(RESPONSE_START_CODE, CODE_OK, &content).await;
        }
        other => {
          error!("received unknown opcode1 {other:#04x}, content {}", hex::encode(request.content()));
          error!("whole packet: {:?}", hex::encode(request.into_storage()));
        }
      }
    } else {
      error!(
        "received unknown opcode2 {:#04x}, content {}",
        request.opcode2().read(),
        hex::encode(request.content())
      );
      error!("whole packet: {:?}", hex::encode(request.into_storage()));
    }
  }
}
