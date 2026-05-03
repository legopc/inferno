use tokio::sync::broadcast;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum InterfaceEvent {
    Up(String),
    Down(String),
}

pub struct InterfaceMonitor {
    iface: String,
    last_state: bool,
}

impl InterfaceMonitor {
    pub fn new(iface: &str) -> Self {
        Self {
            iface: iface.to_string(),
            last_state: false,
        }
    }

    fn is_interface_up(iface: &str) -> bool {
        std::fs::read_to_string(format!("/sys/class/net/{}/operstate", iface))
            .map(|s| {
                let s = s.trim();
                s == "up" || s == "unknown" || s == "lowerup"
            })
            .unwrap_or(false)
    }

    pub async fn run(&mut self, sender: broadcast::Sender<InterfaceEvent>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await;
            let current_up = Self::is_interface_up(&self.iface);
            if current_up != self.last_state {
                self.last_state = current_up;
                let event = if current_up {
                    InterfaceEvent::Up(self.iface.clone())
                } else {
                    InterfaceEvent::Down(self.iface.clone())
                };
                let _ = sender.send(event);
            }
        }
    }
}