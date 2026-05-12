use std::{pin::Pin, sync::Arc, time::Duration};

use crate::{
  common::*,
  device_info::DeviceInfo,
  media_clock::{ClockOverlay, MediaClock},
  util::real_time_box_channel::{self, RealTimeBoxReceiver, RealTimeBoxSender},
  ring_buffer::{
    ProxyToBuffer, ProxyToSamplesBuffer, RBOutput,
  },
};
use futures::{Future, FutureExt};

use itertools::Itertools;
use tokio::{
  sync::{mpsc, Notify},
  time::interval,
};

// Lower value = lower audio playthrough latency. Must be > 0.
// The consuming side (device.rs LEAD_SAMPLES) must exceed READ_INTERVAL_MS * sample_rate / 1000
// samples to avoid gaps. E.g. at 2 ms and 48000 Hz: min LEAD = 96 samples.
// NOTE: In event-driven mode (data_notify wired) this interval is only a safety-net fallback.
// Normally samples_collector wakes within <0.5 ms of packet arrival via TransferNotifier.
const FALLBACK_INTERVAL: Duration = Duration::from_millis(1);
const CALLBACK_BLOCK_SIZE: usize = 32;
const MAX_CALLBACK_BLOCKS_PER_WAKE: usize = 8;
const BUFFER_SIZE: usize = 65536;
const SANE_CLOCK_DIFF: usize = 192000;
const ZERO_FILL_WARN_PERIODS: usize = 1000;

pub type SamplesCallback = Box<dyn FnMut(usize, &Vec<Vec<Sample>>) + Send + 'static>;

struct Channel<P: ProxyToSamplesBuffer> {
  id: usize,
  source: RBOutput<Sample, P>,
  prev_holes_count: usize,
  latency_samples: Clock,
  was_connected: bool,
  callback_cursor: Option<Clock>,
  callback_zero_fills: usize,
}

impl<P: ProxyToSamplesBuffer> Channel<P> {
  fn report_lost_samples(&self, timestamp: Clock, num_samples: usize, reason: &str) {
    error!("Lost {num_samples} samples at timestamp {timestamp} in channel id {} ({reason})", self.id);
  }
  fn read_samples_from_ringbuffer(&mut self, start_timestamp: Clock, buffer: &mut [Sample]) -> bool {
    let mut good = true;
    // report holes:
    let holes_count = self.source.holes_count();
    if holes_count != self.prev_holes_count {
      debug!("holes {} -> {}", self.prev_holes_count, holes_count);
      self.report_lost_samples(start_timestamp, buffer.len(), "reorder buffer timeout");
      self.prev_holes_count = holes_count;
      good = false;
    }

    // read samples:
    let r = self.source.read_at(start_timestamp as usize, buffer);
    if r.useful_start_index != 0 {
      if self.was_connected {
        self.report_lost_samples(
          start_timestamp,
          r.useful_start_index,
          "buffer underrun or overwritten in the meantime",
        );
        good = false;
      }
      // clear whatever junk data was contained at the beginning of buffer
      for sample in &mut buffer[0..r.useful_start_index] {
        *sample = 0;
      }
    }
    if r.useful_start_index < r.useful_end_index {
      self.was_connected = true;
    }
    if r.useful_end_index != buffer.len() {
      if self.was_connected {
        self.report_lost_samples(
          start_timestamp.wrapping_add(r.useful_end_index.try_into().unwrap()),
          buffer.len() - r.useful_end_index,
          "buffer underrun",
        );
        good = false;
      }
      for sample in &mut buffer[r.useful_end_index..] {
        *sample = 0;
      }
    }
    if !good {
      warn!(
        "wanted {start_timestamp}..{} but has ..{}",
        start_timestamp + buffer.len(),
        self.source.readable_until()
      );
    }
    good
  }

  fn read_callback_block(&mut self, block_size: usize, buffer: &mut [Sample]) {
    let buffer = &mut buffer[..block_size];
    let readable_until = self.source.readable_until();
    let Some(mut cursor) = self.callback_cursor else {
      self.callback_cursor = Some(readable_until);
      self.fill_callback_zeros(buffer, "bootstrap");
      return;
    };

    let available = wrapped_diff(readable_until, cursor);
    if available <= 0 {
      self.fill_callback_zeros(buffer, "waiting for samples");
      return;
    }

    if available > SANE_CLOCK_DIFF as ClockDiff {
      warn!(
        "callback channel id {} lagged by {available} samples; resyncing to latest block",
        self.id
      );
      cursor = readable_until.wrapping_sub(block_size as Clock);
      self.callback_cursor = Some(cursor);
      self.read_samples_from_ringbuffer(cursor, buffer);
      self.callback_cursor = Some(cursor.wrapping_add(block_size as Clock));
      self.callback_zero_fills = 0;
      return;
    }

    let available: usize = available.try_into().unwrap();

    if available < block_size {
      self.fill_callback_zeros(buffer, "partial callback block");
      return;
    }

    self.read_samples_from_ringbuffer(cursor, buffer);
    self.callback_cursor = Some(cursor.wrapping_add(block_size as Clock));
    self.callback_zero_fills = 0;
  }

  fn fill_callback_zeros(&mut self, buffer: &mut [Sample], reason: &str) {
    buffer.fill(0);
    self.callback_zero_fills = self.callback_zero_fills.saturating_add(1);
    if self.callback_zero_fills == ZERO_FILL_WARN_PERIODS || self.callback_zero_fills % (ZERO_FILL_WARN_PERIODS * 10) == 0 {
      warn!(
        "callback channel id {} has filled {} blocks with silence ({reason})",
        self.id, self.callback_zero_fills
      );
    }
  }
}

pub struct RealTimeSamplesReceiver<P: ProxyToSamplesBuffer> {
  channels: Vec<RealTimeBoxReceiver<Option<Channel<P>>>>,
  clock: MediaClock,
  clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
}

// MAYBE TODO move timestamp checks to separate module

impl<P: ProxyToSamplesBuffer> RealTimeSamplesReceiver<P> {
  fn get_min_max_end_timestamps(&mut self) -> Option<(Clock, Clock)> {
    get_min_max_end_timestamps(self.channels.iter_mut().map(|chrecv| {
      chrecv.update();
      chrecv.get()
    }))
  }
  pub fn get_available_num_samples(&mut self, start_timestamp: Clock) -> usize {
    self
      .get_min_max_end_timestamps()
      .map(|(end_ts, _)| {
        let diff = wrapped_diff(end_ts, start_timestamp);
        if diff > 0 {
          diff as Clock
        } else {
          0
        }
      })
      .unwrap_or(0)
      .try_into()
      .unwrap()
  }
  pub fn get_samples(
    &mut self,
    start_timestamp: Clock,
    channel_index: usize,
    buffer: &mut [Sample],
  ) -> bool {
    let chrecv = &mut self.channels[channel_index];
    chrecv.update();
    if let Some(ch) = chrecv.get_mut() {
      let start_timestamp =
        start_timestamp.wrapping_sub(ch.latency_samples).wrapping_sub(buffer.len() as Clock);
      ch.read_samples_from_ringbuffer(start_timestamp, buffer)
    } else {
      buffer.fill(0);
      true
    }
  }
  pub fn clock(&mut self) -> &MediaClock {
    if self.clock_recv.update() {
      if let Some(ovl) = self.clock_recv.get() {
        self.clock.update_overlay(*ovl);
      }
    }
    &self.clock
  }
}

enum Command<P: ProxyToSamplesBuffer> {
  NoOp,
  Shutdown,
  ConnectChannel { channel_index: usize, source: RBOutput<Sample, P>, latency_samples: usize },
  DisconnectChannel { channel_index: usize },
}

struct ToRealTime<P: ProxyToSamplesBuffer> {
  commands_receiver: mpsc::Receiver<Command<P>>,
  senders: Vec<RealTimeBoxSender<Option<Channel<P>>>>,
}

impl<P: ProxyToSamplesBuffer> ToRealTime<P> {
  async fn run(&mut self) {
    loop {
      let command_opt = self.commands_receiver.recv().await;
      if !self.handle_command(command_opt).await {
        break;
      }
      for sender in &self.senders {
        sender.collect_garbage();
      }
    }
  }
  async fn handle_command(&mut self, command_opt: Option<Command<P>>) -> bool {
    let command = command_opt.unwrap_or(Command::Shutdown);
    match command {
      Command::ConnectChannel { channel_index, source, latency_samples } => {
        debug!("connecting channel index={channel_index}");
        self.senders[channel_index].send(Box::new(Some(Channel {
          id: channel_index + 1,
          source,
          prev_holes_count: 0,
          latency_samples: latency_samples.try_into().unwrap(),
          was_connected: false,
          callback_cursor: None,
          callback_zero_fills: 0,
        })));
      }
      Command::DisconnectChannel { channel_index } => {
        debug!("disconnecting channel index={channel_index}");
        self.senders[channel_index].send(Box::new(None));
      }
      Command::Shutdown => {
        return false;
      }
      Command::NoOp => {}
    };
    return true;
  }
}

struct PeriodicSamplesCollector<P: ProxyToSamplesBuffer> {
  commands_receiver: mpsc::Receiver<Command<P>>,
  channels: Vec<Option<Channel<P>>>,
  callback: SamplesCallback,
  data_notify: Arc<Notify>,
}

fn get_min_max_end_timestamps<'a, P: ProxyToSamplesBuffer + 'a>(
  channels: impl IntoIterator<Item = &'a Option<Channel<P>>>,
) -> Option<(Clock, Clock)> {
  let clocks = channels
    .into_iter()
    .filter_map(|opt| opt.as_ref())
    .map(|ch| ch.source.readable_until())
    .collect_vec();
  Some((
    clocks.iter().min_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?.to_owned().try_into().unwrap(),
    clocks.iter().max_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?.to_owned().try_into().unwrap(),
  ))
}

impl<P: ProxyToSamplesBuffer> PeriodicSamplesCollector<P> {
  fn get_min_max_end_timestamps(&self) -> Option<(Clock, Clock)> {
    get_min_max_end_timestamps(&self.channels)
  }
  async fn run(&mut self) {
    let mut fallback_interval = interval(FALLBACK_INTERVAL);
    fallback_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut channels_buffers = (0..self.channels.len()).map(|_| vec![0; BUFFER_SIZE]).collect_vec();
    let block_size = CALLBACK_BLOCK_SIZE.min(BUFFER_SIZE);
    'outer: loop {
      tokio::select! {
        biased;
        command_opt = self.commands_receiver.recv() => {
          if !self.handle_command(command_opt).await {
            break 'outer;
          }
          continue 'outer;
        }
        _ = self.data_notify.notified() => {},
        _ = fallback_interval.tick() => {},
      };
      if !self.has_callback_block_ready(block_size) {
        continue;
      }

      for _ in 0..MAX_CALLBACK_BLOCKS_PER_WAKE {
        if !self.has_callback_block_ready(block_size) {
          break;
        }
        for chi in 0..self.channels.len() {
          let buffer = channels_buffers[chi].as_mut_slice();
          if let Some(ch) = &mut self.channels[chi] {
            ch.read_callback_block(block_size, buffer);
            if let Some(cursor) = ch.callback_cursor {
              ch.source.read_done(cursor as usize); // TODO force buffer sizes to be power of 2
            }
          } else {
            buffer[0..block_size].fill(0);
          }
        }
        (self.callback)(block_size, &channels_buffers);
      }
    }
  }

  fn has_callback_block_ready(&mut self, block_size: usize) -> bool {
    for ch in self.channels.iter_mut().filter_map(|ch| ch.as_mut()) {
      let readable_until = ch.source.readable_until();
      let Some(cursor) = ch.callback_cursor else {
        ch.callback_cursor = Some(readable_until);
        continue;
      };
      let available = wrapped_diff(readable_until, cursor);
      if available > 0 && available as usize >= block_size {
        return true;
      } else if available > SANE_CLOCK_DIFF as isize {
        return true;
      }
    }
    false
  }

  async fn handle_command(&mut self, command_opt: Option<Command<P>>) -> bool {
    let command = command_opt.unwrap_or(Command::Shutdown);
    match command {
      Command::ConnectChannel { channel_index, source, latency_samples } => {
        debug!("connecting channel index={channel_index}");
        self.channels[channel_index] = Some(Channel {
          id: channel_index + 1,
          source,
          prev_holes_count: 0,
          latency_samples: latency_samples.try_into().unwrap(),
          was_connected: false,
          callback_cursor: None,
          callback_zero_fills: 0,
        });
      }
      Command::DisconnectChannel { channel_index } => {
        debug!("disconnecting channel index={channel_index}");
        self.channels[channel_index] = None;
      }
      Command::Shutdown => {
        return false;
      }
      Command::NoOp => {}
    };
    return true;
  }
}

pub struct SamplesCollector<P: ProxyToSamplesBuffer> {
  commands_sender: mpsc::Sender<Command<P>>,
}

impl<P: ProxyToSamplesBuffer + Sync + Send + 'static> SamplesCollector<P> {
  pub fn new_with_callback(
    self_info: Arc<DeviceInfo>,
    callback: SamplesCallback,
    data_notify: Arc<Notify>,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
    let (tx, rx) = mpsc::channel(100);
    let mut internal = PeriodicSamplesCollector {
      commands_receiver: rx,
      channels: (0..self_info.rx_channels.len()).map(|_| None).collect(),
      callback,
      data_notify,
    };
    return (Self { commands_sender: tx }, async move { internal.run().await }.boxed());
  }

  pub fn new_realtime(
    self_info: Arc<DeviceInfo>,
    clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>, RealTimeSamplesReceiver<P>) {
    let (tx, rx) = mpsc::channel(100);
    let (senders, receivers) =
      (0..self_info.rx_channels.len()).map(|chi| real_time_box_channel::channel(Box::new(None))).unzip();

    let mut internal = ToRealTime { commands_receiver: rx, senders };

    (
      Self { commands_sender: tx },
      async move { internal.run().await }.boxed(),
      RealTimeSamplesReceiver { channels: receivers, clock: MediaClock::new(false /* TODO */), clock_recv },
    )
  }

  /* pub fn new_external(self_info: Arc<DeviceInfo>, external_channels: impl IntoIterator<Item = ExternalBufferParameters<Sample>>) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {

  } */

  pub async fn connect_channel(
    &self,
    channel_index: usize,
    source: RBOutput<Sample, P>,
    latency_samples: usize,
  ) {
    self
      .commands_sender
      .send(Command::ConnectChannel { channel_index, source, latency_samples })
      .await
      .log_and_forget();
  }
  pub async fn disconnect_channel(&self, channel_index: usize) {
    self.commands_sender.send(Command::DisconnectChannel { channel_index }).await.log_and_forget();
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ring_buffer::new_owned;

  fn channel_at(start_time: Clock) -> (crate::ring_buffer::RBInput<Sample, crate::ring_buffer::OwnedBuffer<atomic::Atomic<Sample>>>, Channel<crate::ring_buffer::OwnedBuffer<atomic::Atomic<Sample>>>) {
    let (input, output) = new_owned(256, start_time, 4);
    (
      input,
      Channel {
        id: 1,
        source: output,
        prev_holes_count: 0,
        latency_samples: 0,
        was_connected: false,
        callback_cursor: None,
        callback_zero_fills: 0,
      },
    )
  }

  #[test]
  fn callback_channels_read_independent_timestamp_epochs() {
    let (mut input_a, mut channel_a) = channel_at(1_000);
    let (mut input_b, mut channel_b) = channel_at(1_000_000);
    let mut out_a = vec![0; CALLBACK_BLOCK_SIZE];
    let mut out_b = vec![0; CALLBACK_BLOCK_SIZE];

    input_a.write_from_at(1_000, (0..CALLBACK_BLOCK_SIZE).map(|i| 100 + i as Sample));
    input_b.write_from_at(1_000_000, (0..CALLBACK_BLOCK_SIZE).map(|i| 200 + i as Sample));

    channel_a.callback_cursor = Some(1_000);
    channel_b.callback_cursor = Some(1_000_000);

    channel_a.read_callback_block(CALLBACK_BLOCK_SIZE, &mut out_a);
    channel_b.read_callback_block(CALLBACK_BLOCK_SIZE, &mut out_b);

    assert_eq!(out_a, (0..CALLBACK_BLOCK_SIZE).map(|i| 100 + i as Sample).collect::<Vec<_>>());
    assert_eq!(out_b, (0..CALLBACK_BLOCK_SIZE).map(|i| 200 + i as Sample).collect::<Vec<_>>());
    assert_eq!(channel_a.callback_cursor, Some(1_000 + CALLBACK_BLOCK_SIZE as Clock));
    assert_eq!(channel_b.callback_cursor, Some(1_000_000 + CALLBACK_BLOCK_SIZE as Clock));
  }

  #[test]
  fn callback_channel_resyncs_when_cursor_falls_too_far_behind() {
    let start_time = 1_000;
    let mut values = vec![0; CALLBACK_BLOCK_SIZE];
    let (mut input, mut channel) = channel_at(start_time);
    let latest_block_start = start_time + SANE_CLOCK_DIFF as Clock + 64;

    input.write_from_at(latest_block_start, (0..CALLBACK_BLOCK_SIZE).map(|i| 300 + i as Sample));
    channel.callback_cursor = Some(start_time);

    channel.read_callback_block(CALLBACK_BLOCK_SIZE, &mut values);

    assert_eq!(values, (0..CALLBACK_BLOCK_SIZE).map(|i| 300 + i as Sample).collect::<Vec<_>>());
    assert_eq!(channel.callback_cursor, Some(latest_block_start + CALLBACK_BLOCK_SIZE as Clock));
  }
}
