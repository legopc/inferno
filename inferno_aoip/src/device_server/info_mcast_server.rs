use crate::byte_utils::*;
use super::channels_subscriber::ChannelsSubscriber;
use crate::common::*;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::mcast::{make_packet, MulticastMessage};
use crate::media_clock::MediaClock;
use crate::{byte_utils::write_str_to_buffer, device_info::DeviceInfo};
use bytebuffer::ByteBuffer;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::RwLock;
use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::Arc,
  time::Duration,
};
use tokio::sync::watch;
use tokio::time::interval;
use tokio::{
  select,
  sync::{broadcast::Receiver as BroadcastReceiver, mpsc},
  time::MissedTickBehavior,
};

const SEND_BUFFER_SIZE: usize = 1500;
const DST_PORT_HEARTBEAT: u16 = 8708;
const DST_PORT_DEVICE_INFO: u16 = 8702;

pub type PeaksCallback = Box<dyn FnMut() -> (Vec<u8>, Vec<u8>) + Send + Sync>;

struct Multicaster<'s> {
  self_info: &'s DeviceInfo,
  pub server: UdpSocketWrapper,
  seqnum: u16,
  vendor: [u8; 8],
  firmware_version_bytes: [u8; 4],
  product_version_bytes: [u8; 4],
  device_info_destination: SocketAddr,
  heartbeat_destination: SocketAddr,
  send_buffer: [u8; SEND_BUFFER_SIZE],
  clock: Arc<RwLock<MediaClock>>,
  channels_subscriber: Option<Arc<ChannelsSubscriber>>,
  get_peaks: PeaksCallback,
  // Each block group has its own seqnum counter, matching the Shure MXWANI8 pattern.
  util_block_seqnum: u16,    // for 0x8000+0x8001 packets
  latency_block_seqnum: u16, // for 0x8003+0x8004 packets
  tx_bytes: Arc<AtomicU64>,
  rx_bytes: Arc<AtomicU64>,
  tx_errors: Arc<AtomicU32>,
  rx_errors: Arc<AtomicU32>,
}

impl<'s> Multicaster<'s> {
  pub fn new(
    self_info: &'s DeviceInfo,
    server: UdpSocketWrapper,
    clock: Arc<RwLock<MediaClock>>,
    get_peaks: PeaksCallback,
    tx_bytes: Arc<AtomicU64>,
    rx_bytes: Arc<AtomicU64>,
    tx_errors: Arc<AtomicU32>,
    rx_errors: Arc<AtomicU32>,
  ) -> Multicaster {
    let _patch_version = env!("CARGO_PKG_VERSION_PATCH").parse::<u16>().unwrap();
    let mut r = Multicaster {
      self_info,
      server,
      seqnum: 1,
      vendor: [32; 8],
      firmware_version_bytes: self_info.firmware_version_bytes.unwrap_or([4, 1, 0, 6]),
      product_version_bytes: self_info.product_version_bytes.unwrap_or([4, 1, 0, 6]),
      device_info_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 231)),
        DST_PORT_DEVICE_INFO,
      ),
      heartbeat_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 233)),
        DST_PORT_HEARTBEAT,
      ),
      send_buffer: [0; SEND_BUFFER_SIZE],
      clock,
      channels_subscriber: None,
      get_peaks,
      util_block_seqnum: 1,
      latency_block_seqnum: 1,
      tx_bytes,
      rx_bytes,
      tx_errors,
      rx_errors,
    };
    write_str_to_buffer(&mut r.vendor, 0, 8, &self_info.vendor_string);
    return r;
  }

  pub fn should_work(&self) -> bool {
    return self.server.should_work();
  }

  async fn send(&mut self, dst: SocketAddr, start_code: u16, opcode: [u8; 8], content: &[u8]) {
    let pkt = make_packet(
      &mut self.send_buffer,
      start_code,
      self.seqnum,
      self.self_info.process_id,
      self.self_info.factory_device_id,
      self.vendor,
      opcode,
      content,
    );
    self.seqnum = self.seqnum.wrapping_add(1);
    self.server.send(&dst, pkt).await;
  }

  async fn send_board_info(&mut self) {
    let mut content = [0u8; 200];
    // Firmware version:
    content[0..4].copy_from_slice(&self.firmware_version_bytes); // T1.5: use struct field instead of hardcoded literal
    content[0x23] = 2;
    // Hardware version:
    content[4..8].copy_from_slice(&[4, 1, 0, 3]);
    content[0x27] = 1;
    // Boot version:
    content[0x28..0x2c].copy_from_slice(&[1, 0, 0, 0]);

    // flags of supported features:
    // 0x14: AES67, Device Lock
    //       0x04 - supports AES67
    //       0x08 - is lockable
    // 0x15: unknown capability flags (Shure MXWANI8 = 0x7c).
    //       Setting 0x7c greys out the Clear Config button in DC — exact semantics unknown.
    //       Setting 0x00 causes DC to show Clear Config as clickable (unwanted).
    // 0x16:
    //       0x10 - has Manufacturer name
    //       0x40 - Network is configurable (supports static addressing) — NOT set:
    //              omitting this keeps Addresses and Switch Config greyed in DC.
    // 0x17: feature flags:
    //   0x01, 0x02 = unknown; required alongside 0x40 for Reboot button to activate in DC
    //   0x08 = Identify device (LED blink)
    //   0x40 = Reboot supported
    //   0x80 = Factory reset supported (intentionally NOT set — we don't support this;
    //          DC greys the Factory Reset button regardless due to CMC 0x3010 gating)
    // Note: primary gate for DC management buttons is the CMC 0x3010 keepalive exchange.
    // Board_info flags provide secondary per-button control on top of that gate.
    content[0x14] = 0;    // no AES67, not lockable
    content[0x15] = 0x7c; // required to keep Clear Config greyed in DC (exact bits unknown)
    content[0x16] = 0x10; // has Manufacturer name only — no 0x40 (keeps Addresses/Switch Config greyed)
    content[0x17] = 0x4b; // Identify (0x08) + Reboot (0x40) + required companions (0x01|0x02)

    content[0xbb] = 0x1f; // if 0, device is flooded with info multicast requests around 1 per second
    content[0xbf] = 5;   // T1.6: limit DC polling rate for board-info sub-types (matches reference capture)
    content[0xc3] = 3;   // T1.6: rate limit sub-type 3
    content[0xc7] = 3;   // T1.6: rate limit sub-type 7
    write_str_to_buffer(&mut content, 12, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x38, 16, &self.self_info.board_name);

    self.send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x60, 0, 0, 0, 0], &content).await;
  }

  async fn send_product_info(&mut self) {
    let mut content = [0; 336];
    write_str_to_buffer(&mut content, 0, 8, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 8, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x2c, 16, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 0xac, 16, &self.self_info.model_name);
    // product version:
    content[0x12c..0x130].copy_from_slice(&self.product_version_bytes); // T1.7: populate product version field (same as firmware at 0x1c)

    // firmware version:
    content[0x1c..0x20].copy_from_slice(&self.product_version_bytes);

    // 0x18..0x1b - software version
    // 0x24..0x26 - software patch version, u32
    // 0x28..0x2b - firmware patch version, u32

    self.send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0xc0, 0, 0, 0, 0], &content).await;
  }

  fn get_freq_offset_ppb(&self) -> Option<i32> {
    self
      .clock
      .read()
      .unwrap()
      .get_overlay()
      .as_ref()
      .map(|clkovl| {
        let freq_offset_f = (clkovl.freq_scale_including_hw() * 1_000_000_000f64).round();
        if i32::MIN as f64 <= freq_offset_f && freq_offset_f <= i32::MAX as f64 {
          Some(freq_offset_f as i32)
        } else {
          None
        }
      })
      .flatten()
  }

  async fn send_heartbeat(&mut self) {
    let freq_offset_opt = self.get_freq_offset_ppb();

    if let Some(freq_offset) = freq_offset_opt {
      // Packet 1: [0x8000 (utilization) + 0x8001 (clock offset)]
      // Packet 2: [0x8003 (rx latency) + 0x8004 (missed packets)]
      // Shure MXWANI8 sends both every second as two separate UDP datagrams.
      // Each group has its own block seqnum counter (independent of the packet seqnum).
      {
        // T2.1: get audio peaks before building the buffer (requires &mut self)
        let (tx_peaks, rx_peaks) = (self.get_peaks)();

        let ctr = self.util_block_seqnum;
        let mut bytes = ByteBuffer::new();
        bytes.set_endian(bytebuffer::Endian::BigEndian);

        bytes.write_u16(36);
        bytes.write_u16(0x8000);
        bytes.write_u16(4);
        bytes.write_u16(4);
        bytes.write_u16(ctr);
        bytes.write_u16(0);
        bytes.write_u16(0x0010); // constant sub-header from Shure capture
        bytes.write_u16(0x0000);
        bytes.write_u16(0x0001);
        bytes.write_u16(0x0010);
        bytes.write_u32(self.tx_bytes.swap(0, Ordering::Relaxed).min(u32::MAX as u64) as u32); // TX bytes/sec
        bytes.write_u32(self.rx_bytes.swap(0, Ordering::Relaxed).min(u32::MAX as u64) as u32); // RX bytes/sec
        bytes.write_u32(self.tx_errors.swap(0, Ordering::Relaxed)); // TX errors
        bytes.write_u32(self.rx_errors.swap(0, Ordering::Relaxed)); // RX errors

        bytes.write_u16(16);
        bytes.write_u16(0x8001);
        bytes.write_u16(4);
        bytes.write_u16(4);
        bytes.write_u16(ctr);
        bytes.write_u16(0);
        bytes.write_i32(freq_offset);

        // T2.1: 0x8002 audio peak levels per channel
        // Format (from Dante device capture): [num_tx u16][num_rx u16][reserved u32]
        //   [bits_per_sample u16][reserved u16][tx peaks u8...][rx peaks u8...][pad if odd]
        {
          let num_tx = tx_peaks.len() as u16;
          let num_rx = rx_peaks.len() as u16;
          let peak_bytes = tx_peaks.len() + rx_peaks.len();
          let padded_peak_bytes = (peak_bytes + 1) & !1; // round up to even boundary
          let data_len = (12 + padded_peak_bytes) as u16;
          let block_len = 12 + data_len;
          bytes.write_u16(block_len);
          bytes.write_u16(0x8002);
          bytes.write_u16(4);
          bytes.write_u16(data_len);
          bytes.write_u16(ctr);
          bytes.write_u16(0);
          bytes.write_u16(num_tx);
          bytes.write_u16(num_rx);
          bytes.write_u32(0); // reserved
          bytes.write_u16(self.self_info.bits_per_sample as u16);
          bytes.write_u16(0); // reserved
          for &p in tx_peaks.iter().chain(rx_peaks.iter()) {
            bytes.write_u8(p);
          }
          if peak_bytes & 1 != 0 {
            bytes.write_u8(0); // alignment padding
          }
        }

        self.util_block_seqnum = self.util_block_seqnum.wrapping_add(1);
        let content = bytes.as_bytes().to_vec();
        self.send(self.heartbeat_destination, 0xfffe, [0, 8, 0, 1, 0x10, 0, 0, 0], &content).await;
      }

      if let Some(chsub) = self.channels_subscriber.as_ref() {
        let ctr = self.latency_block_seqnum;

        let content = {
          let mut bytes = ByteBuffer::new();
          bytes.set_endian(bytebuffer::Endian::BigEndian);

          let flows_info = chsub.flows_info();
          let flows_info = flows_info.read().unwrap();
          let flows_count = flows_info.len() as u16;

          bytes.write_u16(24 + flows_count * 4);
          bytes.write_u16(0x8003);
          bytes.write_u16(4);
          bytes.write_u16(12 + flows_count * 4);
          bytes.write_u16(ctr);
          bytes.write_u16(0);
          bytes.write_u16(flows_count);
          bytes.write_u16(0);
          bytes.write_u16(24);
          bytes.write_u16(0);
          bytes.write_u32(self.self_info.sample_rate);
          for opt in flows_info.iter() {
            let latency =
              opt.as_ref().map(|fi| fi.actual_latency_samples.swap(0, Ordering::Relaxed)).unwrap_or(0);
            bytes.write_u32(latency.clamp(0, i32::MAX) as u32);
          }

          bytes.write_u16(20 + flows_count * 4);
          bytes.write_u16(0x8004);
          bytes.write_u16(4);
          bytes.write_u16(8 + flows_count * 4);
          bytes.write_u16(ctr);
          bytes.write_u16(0);
          bytes.write_u16(flows_count);
          bytes.write_u16(0);
          bytes.write_u16(20);
          bytes.write_u16(0);
          for opt in flows_info.iter() {
            let missed = opt
              .as_ref()
              .map(|fi| fi.missed_packets.swap(0, Ordering::Relaxed))
              .unwrap_or(0);
            bytes.write_u32(missed);
          }
          // lock guard dropped here, before the await
          bytes.as_bytes().to_vec()
        };

        self.latency_block_seqnum = self.latency_block_seqnum.wrapping_add(1);
        self.send(self.heartbeat_destination, 0xfffe, [0, 8, 0, 1, 0x10, 0, 0, 0], &content).await;
      }
    } else {
      debug!("no clock available");
    }

    // this is probably response to 0738008100000064
    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00],
      &[0x00, 0x18, 0x00, 0x04, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x02, 0x00, 0x00,
      // supported sample rates:
      0x00, 0x00, /* 44100: */ 0xac, 0x44, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x01, 0x58, 0x88, 0x00, 0x01, 0x77, 0x00]
    ).await; */

    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x10, 0x07, 0, 0, 0, 0],
      &[0, 0, 0, 0]
    ).await; */

    // this is probably response to 0738008300000064
    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x82, 0x00, 0x00, 0x00, 0x00],
    &[
      0x00, 0x18, 0x00, 0x03, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x18, 0x00, 0x02, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x20
    ]).await; */

    /* self.send(
    self.device_info_destination, 0xffff, [0x07, 0x2a, 0x10, 0x09, 0x00, 0x00, 0x00, 0x00],
    &[
      0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x00, 0x08, 0x00, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
      /* clock source: */0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x11, 0x33,
      /* transmitting to us??? : */ 0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x66, 0x33,
    ]).await; */
  }

  async fn send_clock_stats(&mut self) {
    let freq_offset = if let Some(f) = self.get_freq_offset_ppb() {
      f
    } else {
      return;
    };
    let required_prefix = format!("clock-stats.{}0000", hex::encode(self.self_info.mac_address.octets()));
    let mut master_clock = None;
    if let Ok(readdir) = std::fs::read_dir("/tmp") {
      for entry in readdir {
        if let Ok(entry) = entry {
          if entry.file_name().to_string_lossy().starts_with(&required_prefix) {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
              let content = content.trim_ascii();
              if content.len() >= 12 {
                if let Ok(master_id) = hex::decode(&content[0..16]) {
                  master_clock = Some(master_id);
                  break;
                }
              }
            }
          }
        }
      }
    }
    if let Some(mc) = master_clock {
      assert_eq!(mc.len(), 8);
      let mut bytes = ByteBuffer::new();
      bytes.set_endian(bytebuffer::Endian::BigEndian);
      bytes.write_bytes(&[
        0x00, 0x03, 0x00, 0x03, /* 0x01 = PLL not locked */
        0x00, 0x00, 0x00, 0x9f, /* was 0xff */
      ]);
      bytes.write_i32(freq_offset);
      bytes.write_bytes(&self.self_info.mac_address.octets());
      bytes.write_u16(0);
      bytes.write_bytes(&mc);
      bytes.write_bytes(&mc);
      bytes.write_bytes(&[0u8; 76]);
      self
        .send(
          self.device_info_destination,
          0xffff,
          [0x07, 0x2a, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00],
          bytes.as_bytes(),
        )
        .await;
    }
  }

  async fn send_network_info(&mut self) {
    let mut bytes = ByteBuffer::new();
    bytes.set_endian(bytebuffer::Endian::BigEndian);
    bytes.write_bytes(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
    bytes.write_u16(self.self_info.link_speed);
    bytes.write_u16(1);
    bytes.write_bytes(&self.self_info.mac_address.octets());
    bytes.write_bytes(&self.self_info.ip_address.octets());
    bytes.write_bytes(&self.self_info.netmask.octets());
    bytes.write_bytes(&self.self_info.gateway.octets());
    bytes.write_bytes(&self.self_info.gateway.octets()); // DNS? doesn't really matter.
    bytes.write_bytes(&[
      0x00, 0x18, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);

    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00],
        bytes.as_bytes(),
      )
      .await;
  }

  async fn send_sample_rate(&mut self) {
    // Advertise the configured sample rate as the single supported rate so DC shows it read-only.
    let sr = (self.self_info.sample_rate as u16).to_be_bytes(); // T1.3: use actual rate, not hardcoded 48000
    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00],
        &[
          // Header matches Shure MXWANI8 exactly: 0x0018=length, 0x0001=type.
          // Using 0x0004 as type makes DC treat the rate as editable.
          0x00, 0x18, 0x00, 0x01,
          0x00, 0x00, sr[0], sr[1], // current sample rate: from self_info
          0x00, 0x00, sr[0], sr[1], // default sample rate: from self_info
          0x00, 0x01, 0x00, 0x00,   // 1 supported rate
          0x00, 0x00, sr[0], sr[1], // single supported rate: from self_info
        ],
      )
      .await;
  }

  async fn send_encoding(&mut self) {
    // Advertise the configured encoding as the single supported encoding so DC shows it read-only.
    let bps = (self.self_info.bits_per_sample as u32).to_be_bytes(); // T1.4: use actual bits_per_sample, not hardcoded 0x18
    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x82, 0x00, 0x00, 0x00, 0x00],
        &[
          // Use 0x0018/0x0003 to match the non-editable pattern (Shure uses 0x0018/0x0001 for 0x80).
          0x00, 0x18, 0x00, 0x03,
          bps[0], bps[1], bps[2], bps[3], // current encoding: bits_per_sample
          bps[0], bps[1], bps[2], bps[3], // default encoding: bits_per_sample
          0x00, 0x01, 0x00, 0x00,          // 1 supported encoding
          bps[0], bps[1], bps[2], bps[3], // single supported encoding: bits_per_sample
        ],
      )
      .await;
  }
}

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  mut rx: mpsc::Receiver<MulticastMessage>,
  clock: Arc<RwLock<MediaClock>>,
  mut channels_sub_rx: watch::Receiver<Option<Arc<ChannelsSubscriber>>>,
  get_peaks: PeaksCallback,
  shutdown: BroadcastReceiver<()>,
  tx_bytes: Arc<AtomicU64>,
  rx_bytes: Arc<AtomicU64>,
  tx_errors: Arc<AtomicU32>,
  rx_errors: Arc<AtomicU32>,
) {
  let server =
    UdpSocketWrapper::new(Some(self_info.ip_address), self_info.info_request_port, shutdown).await;
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  let mut mcaster = Multicaster::new(self_info.as_ref(), server, clock, get_peaks, tx_bytes, rx_bytes, tx_errors, rx_errors);
  mcaster.send_board_info().await;
  mcaster.send_product_info().await;
  let mut heartbeat_interval = interval(Duration::from_secs(1));
  heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
  while mcaster.should_work() {
    select! {
      r = mcaster.server.recv(&mut recv_buff) => {
        let (_src, request_buf) = match r {
          Some(v) => v,
          None => continue
        };
        if request_buf.len() < 32 {
          error!("too short packet received: {}", hex::encode(request_buf));
          continue;
        }
        let opcode = &request_buf[24..32];
        match opcode {
          [0x07, _, 0, 0x61, 0, 0, 0, 0] => {
            mcaster.send_board_info().await;
          },
          [0x07, _, 0, 0xc1, 0, 0, 0, 0] => {
            mcaster.send_product_info().await;
          },
          [0x07, _ /* was 0x38 */, 0, 0x21, 0, 0, 0, _ /* was 0x64 */] => {
            mcaster.send_clock_stats().await;
          },
          [0x07, _, 0, 0x13, 0, 0, 0, _] => {
            mcaster.send_network_info().await;
          }
          [0x07, _, 0, 0x77, 0, 0, 0, _] => {
            // T3.9 REVERTED: DC sends 0x0077 automatically on reconnect (not only user-initiated).
            // Exiting here causes an infinite restart loop. Keep as a no-op until we can
            // distinguish automatic DC registration sends from deliberate user Clear Config.
            // TODO T3.9: needs capture analysis to find a distinguishing field.
            trace!("Clear Config 0x0077 received — no-op (see T3.9)");
          }
          [0x07, _, 0, 0x81, 0, 0, 0, _] => {
            mcaster.send_sample_rate().await;
          }
          [0x07, _, 0, 0x83, 0, 0, 0, _] => {
            mcaster.send_encoding().await;
          }
          [0x07, _, 0, 0x90, 0, 0, 0, _] => {
            // Reboot command from Dante Controller (conmon message_type=0x0090).
            // Ack with 0x0092 to 224.0.0.231:8702 so DC knows the command was received,
            // then exit — systemd Restart=on-failure will re-launch the service.
            warn!("Reboot requested by Dante Controller — restarting");
            mcaster.send(
              mcaster.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x92, 0, 0, 0, 0],
              &[]
            ).await;
            std::process::exit(0);
          }
          [0x07, _, 0x10, 0x08, 0, 0, 0, _] => {
            // DC periodic heartbeat query — normal traffic, no response needed
            trace!("DC heartbeat query received");
          }
          [0x07, _, 0, 0x91, 0, 0, 0, _] => {
            // Factory reset command from DC — deliberately ignored. Inferno has no persistent
            // state to clear. The button is greyed out in DC (DC does not enable it via CMC
            // 0x3010 for devices that don't advertise factory reset support), but handle it
            // defensively in case another controller sends it.
            warn!("Factory reset requested by DC — ignored (not implemented)");
          }
          _ => {
            warn!("unknown request to multicast port: opcode: {}", hex::encode(opcode));
            warn!("raw udp payload: {}", hex::encode(request_buf));
          }
        };
      },
      m = rx.recv() => {
        // TODO we could also make seqnum atomic and simply share socket with anyone that wants it
        if let Some(msg) = m {
          mcaster.send(mcaster.device_info_destination, msg.start_code, msg.opcode, &msg.content).await;
        } else {
          break;
        }
      },
      _ = heartbeat_interval.tick() => {
        mcaster.send_heartbeat().await;
      },
      _ = channels_sub_rx.changed() => {
        mcaster.channels_subscriber = channels_sub_rx.borrow_and_update().clone();
      }
      // TODO receive shutdown properly, currently Ctrl-C doesn't work if there is error binding to socket
    };
  }
}
