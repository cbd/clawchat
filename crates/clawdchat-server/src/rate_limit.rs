use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, Duration};

/// Per-key usage tracking.
pub struct KeyUsage {
    pub agent_count: AtomicU64,
    pub msg_count: AtomicU64,
    pub msg_window_start: std::sync::Mutex<Instant>,
    pub room_count: AtomicU64,
}

impl KeyUsage {
    fn new() -> Self {
        Self {
            agent_count: AtomicU64::new(0),
            msg_count: AtomicU64::new(0),
            msg_window_start: std::sync::Mutex::new(Instant::now()),
            room_count: AtomicU64::new(0),
        }
    }
}

/// Rate limits for a tier.
#[derive(Debug, Clone)]
pub struct TierLimits {
    pub max_agents: u64,
    pub max_messages_per_minute: u64,
    pub max_rooms: u64,
}

impl TierLimits {
    pub fn free() -> Self {
        Self {
            max_agents: 20,
            max_messages_per_minute: 200,
            max_rooms: 50,
        }
    }

    pub fn pro() -> Self {
        Self {
            max_agents: 200,
            max_messages_per_minute: 2000,
            max_rooms: 500,
        }
    }

    pub fn for_tier(tier: &str) -> Self {
        match tier {
            "pro" => Self::pro(),
            _ => Self::free(),
        }
    }
}

/// Tracks per-key usage for rate limiting.
pub struct RateLimiter {
    usage: DashMap<String, KeyUsage>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            usage: DashMap::new(),
        }
    }

    fn get_or_create(&self, api_key: &str) -> dashmap::mapref::one::Ref<'_, String, KeyUsage> {
        if !self.usage.contains_key(api_key) {
            self.usage.insert(api_key.to_string(), KeyUsage::new());
        }
        self.usage.get(api_key).unwrap()
    }

    pub fn check_agent_limit(&self, api_key: &str, limits: &TierLimits) -> Result<(), ()> {
        let usage = self.get_or_create(api_key);
        if usage.agent_count.load(Ordering::Relaxed) >= limits.max_agents {
            Err(())
        } else {
            Ok(())
        }
    }

    pub fn check_message_rate(&self, api_key: &str, limits: &TierLimits) -> Result<(), ()> {
        let usage = self.get_or_create(api_key);
        let mut window = usage.msg_window_start.lock().unwrap();

        // Reset window if older than 1 minute
        if window.elapsed() > Duration::from_secs(60) {
            *window = Instant::now();
            usage.msg_count.store(0, Ordering::Relaxed);
        }

        if usage.msg_count.load(Ordering::Relaxed) >= limits.max_messages_per_minute {
            Err(())
        } else {
            Ok(())
        }
    }

    pub fn check_room_limit(&self, api_key: &str, limits: &TierLimits) -> Result<(), ()> {
        let usage = self.get_or_create(api_key);
        if usage.room_count.load(Ordering::Relaxed) >= limits.max_rooms {
            Err(())
        } else {
            Ok(())
        }
    }

    pub fn increment_message(&self, api_key: &str) {
        let usage = self.get_or_create(api_key);
        usage.msg_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_agent(&self, api_key: &str) {
        let usage = self.get_or_create(api_key);
        usage.agent_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn remove_agent(&self, api_key: &str) {
        if let Some(usage) = self.usage.get(api_key) {
            let current = usage.agent_count.load(Ordering::Relaxed);
            if current > 0 {
                usage.agent_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    pub fn add_room(&self, api_key: &str) {
        let usage = self.get_or_create(api_key);
        usage.room_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn remove_room(&self, api_key: &str) {
        if let Some(usage) = self.usage.get(api_key) {
            let current = usage.room_count.load(Ordering::Relaxed);
            if current > 0 {
                usage.room_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}
