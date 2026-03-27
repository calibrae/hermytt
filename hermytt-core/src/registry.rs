use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const EXPIRE_AFTER: Duration = Duration::from_secs(30);

fn deserialize_role_case_insensitive<'de, D>(deserializer: D) -> std::result::Result<ServiceRole, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "shell" => Ok(ServiceRole::Shell),
        "renderer" => Ok(ServiceRole::Renderer),
        "highlighter" => Ok(ServiceRole::Highlighter),
        "sandbox" => Ok(ServiceRole::Sandbox),
        "bot" => Ok(ServiceRole::Bot),
        "parser" => Ok(ServiceRole::Parser),
        _ => Ok(ServiceRole::Unknown),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceRole {
    Shell,
    Renderer,
    Highlighter,
    Sandbox,
    Bot,
    Parser,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceStatus {
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    #[serde(deserialize_with = "deserialize_role_case_insensitive")]
    pub role: ServiceRole,
    pub endpoint: String,
    pub status: ServiceStatus,
    #[serde(skip)]
    pub last_seen_instant: Option<Instant>,
    /// Epoch millis for JSON serialization.
    pub last_seen: u64,
    #[serde(default)]
    pub meta: serde_json::Value,
}

pub struct ServiceRegistry {
    services: RwLock<HashMap<String, ServiceInfo>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self {
            services: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self, mut info: ServiceInfo) {
        let now = Instant::now();
        info.status = ServiceStatus::Connected;
        info.last_seen_instant = Some(now);
        info.last_seen = epoch_millis();
        self.services.write().await.insert(info.name.clone(), info);
    }

    pub async fn unregister(&self, name: &str) -> bool {
        self.services.write().await.remove(name).is_some()
    }

    pub async fn heartbeat(&self, name: &str) -> bool {
        let mut services = self.services.write().await;
        if let Some(svc) = services.get_mut(name) {
            svc.last_seen_instant = Some(Instant::now());
            svc.last_seen = epoch_millis();
            svc.status = ServiceStatus::Connected;
            true
        } else {
            false
        }
    }

    pub async fn list(&self) -> Vec<ServiceInfo> {
        let mut services = self.services.write().await;
        expire_stale(&mut services);
        services.values().cloned().collect()
    }

    pub async fn get(&self, name: &str) -> Option<ServiceInfo> {
        let mut services = self.services.write().await;
        expire_stale(&mut services);
        services.get(name).cloned()
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn expire_stale(services: &mut HashMap<String, ServiceInfo>) {
    let now = Instant::now();
    for svc in services.values_mut() {
        if let Some(last) = svc.last_seen_instant {
            if now.duration_since(last) > EXPIRE_AFTER {
                svc.status = ServiceStatus::Disconnected;
            }
        }
    }
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(name: &str) -> ServiceInfo {
        ServiceInfo {
            name: name.to_string(),
            role: ServiceRole::Shell,
            endpoint: format!("ws://localhost:8888/{name}"),
            status: ServiceStatus::Connected,
            last_seen_instant: None,
            last_seen: 0,
            meta: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn register_and_list() {
        let reg = ServiceRegistry::new();
        reg.register(make_info("shytti")).await;
        let list = reg.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "shytti");
        assert_eq!(list[0].status, ServiceStatus::Connected);
    }

    #[tokio::test]
    async fn unregister() {
        let reg = ServiceRegistry::new();
        reg.register(make_info("shytti")).await;
        assert!(reg.unregister("shytti").await);
        assert!(!reg.unregister("shytti").await);
        assert!(reg.list().await.is_empty());
    }

    #[tokio::test]
    async fn heartbeat_updates_last_seen() {
        let reg = ServiceRegistry::new();
        reg.register(make_info("shytti")).await;

        let before = reg.get("shytti").await.unwrap().last_seen;
        // Small sleep to ensure timestamp changes.
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(reg.heartbeat("shytti").await);
        let after = reg.get("shytti").await.unwrap().last_seen;
        assert!(after >= before);
    }

    #[tokio::test]
    async fn heartbeat_unknown_returns_false() {
        let reg = ServiceRegistry::new();
        assert!(!reg.heartbeat("nonexistent").await);
    }

    #[tokio::test]
    async fn expire_marks_disconnected() {
        let reg = ServiceRegistry::new();
        reg.register(make_info("shytti")).await;

        // Manually backdate the last_seen_instant.
        {
            let mut services = reg.services.write().await;
            let svc = services.get_mut("shytti").unwrap();
            svc.last_seen_instant = Some(Instant::now() - Duration::from_secs(60));
        }

        let list = reg.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, ServiceStatus::Disconnected);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown() {
        let reg = ServiceRegistry::new();
        assert!(reg.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn register_overwrites() {
        let reg = ServiceRegistry::new();
        let mut info = make_info("shytti");
        info.endpoint = "ws://old".to_string();
        reg.register(info).await;

        let mut info2 = make_info("shytti");
        info2.endpoint = "ws://new".to_string();
        reg.register(info2).await;

        let svc = reg.get("shytti").await.unwrap();
        assert_eq!(svc.endpoint, "ws://new");
        assert_eq!(reg.list().await.len(), 1);
    }
}
