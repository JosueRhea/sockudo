use crate::app::config::AppConfig;
use crate::app::manager::AppManager;
use crate::channel::PresenceMemberInfo;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SocketId(pub String);

// Optional but recommended - implement Display for better debugging
impl std::fmt::Display for SocketId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Optional - implement AsRef for convenient string operations
impl AsRef<str> for SocketId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Default for SocketId {
    fn default() -> Self {
        Self::new()
    }
}

impl SocketId {
    pub fn new() -> Self {
        Self(Self::generate_socket_id())
    }

    pub(crate) fn generate_socket_id() -> String {
        let mut rng = rand::thread_rng(); // Get a random number generator

        // Define min and max as u64, since Rust requires specifying the integer type
        let min: u64 = 0;
        let max: u64 = 10000000000;

        // Rust's rand crate handles generating a random number between min and max differently
        let mut random_number = |min: u64, max: u64| -> u64 { rng.gen_range(min..=max) };

        // Format the random numbers into a String with a dot separator
        format!("{}.{}", random_number(min, max), random_number(min, max))
    }
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConnectionState {
    pub socket_id: SocketId,
    pub app: Option<AppConfig>,
    pub subscribed_channels: HashSet<String>,
    pub user_id: Option<String>,
    pub last_ping: String,
    pub presence: Option<HashMap<String, PresenceMemberInfo>>,
    pub user: Option<Value>,
}

impl ConnectionState {
    pub fn new() -> Self {
        Self {
            socket_id: SocketId::new(),
            app: None,
            subscribed_channels: HashSet::new(),
            user_id: None,
            last_ping: String::new(),
            presence: None,
            user: None,
        }
    }

    pub fn is_presence(&self) -> bool {
        self.presence.is_some()
    }

    pub fn is_subscribed(&self, channel: &str) -> bool {
        self.subscribed_channels.contains(channel)
    }

    pub fn add_subscription(&mut self, channel: String) {
        self.subscribed_channels.insert(channel);
    }

    pub fn remove_subscription(&mut self, channel: &str) {
        self.subscribed_channels.remove(channel);
    }

    pub fn update_ping(&mut self) {
        self.last_ping = chrono::Utc::now().to_rfc3339();
    }

    pub fn get_app_key(&self) -> String {
        match &self.app {
            Some(app) => app.key.clone(),
            None => String::new(),
        }
    }
}
