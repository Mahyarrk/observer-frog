//! Notifications: event rendering, coalescing, rate limiting, and the ntfy sink.
//!
//! Invariants:
//!   I7  ntfy publishes with headers (X-Title/X-Priority/X-Tags) and a plain-text body.
//!   I8  no sink goes through the monitored proxy unless it opts in (`use_proxy`).
//!   Rate limit — a burst of simultaneous failures becomes one message, never N.

use crate::config::Sink;
use crate::state::Change;

pub const PRIORITY_OUTAGE: i32 = 4;
pub const PRIORITY_RECOVERY: i32 = 2;
pub const RATE_WINDOW_S: f64 = 60.0;
pub const MAX_MESSAGES_PER_WINDOW: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub title: String,
    pub message: String,
    pub priority: i32,
    pub tags: Vec<String>,
    pub click: String,
}

/// Turn state transitions into human messages. First sightings never page.
pub fn events_from_changes(changes: &[Change], dashboard_url: &str) -> Vec<Event> {
    let mut events = Vec::new();
    for c in changes {
        if c.first_sighting {
            continue;
        }
        if c.is_outage() {
            let suffix = if c.ignored { " (ignored)" } else { "" };
            events.push(Event {
                title: format!("{} is down{}", c.name, suffix),
                message: format!(
                    "{}\nnode {} · {}",
                    if c.detail.is_empty() { "no detail" } else { &c.detail },
                    c.node_id,
                    c.group
                ),
                priority: PRIORITY_OUTAGE,
                tags: vec!["rotating_light".into(), c.service_id.clone()],
                click: dashboard_url.to_string(),
            });
        } else if c.is_recovery() {
            events.push(Event {
                title: format!("{} recovered", c.name),
                message: format!(
                    "{}\nnode {} · {}",
                    if c.detail.is_empty() { "healthy again" } else { &c.detail },
                    c.node_id,
                    c.group
                ),
                priority: PRIORITY_RECOVERY,
                tags: vec!["white_check_mark".into(), c.service_id.clone()],
                click: dashboard_url.to_string(),
            });
        }
    }
    events
}

/// Buffers events for a short window so a mass outage is one page instead of thirty.
#[derive(Default)]
pub struct Digest {
    pub window_s: f64,
    pub max_items: usize,
    pub pending: Vec<Event>,
}

impl Digest {
    pub fn new() -> Self {
        Self { window_s: 15.0, max_items: 12, pending: Vec::new() }
    }

    pub fn add(&mut self, event: Event) {
        self.pending.push(event);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn drain(&mut self) -> Vec<Event> {
        if self.pending.is_empty() {
            return vec![];
        }
        let items: Vec<Event> = self.pending.iter().take(self.max_items).cloned().collect();
        self.pending.drain(0..items.len().min(self.pending.len()));
        if items.len() == 1 {
            return items;
        }
        let outages: Vec<&Event> = items.iter().filter(|e| e.priority >= PRIORITY_OUTAGE).collect();
        let recoveries = items.len() - outages.len();
        if outages.is_empty() {
            let names = items
                .iter()
                .map(|e| e.title.replace(" recovered", ""))
                .collect::<Vec<_>>()
                .join(", ");
            return vec![Event {
                title: format!("{} services recovered", items.len()),
                message: names,
                priority: PRIORITY_RECOVERY,
                tags: vec!["white_check_mark".into()],
                click: String::new(),
            }];
        }
        let joined = outages
            .iter()
            .map(|e| e.title.replace(" is down", ""))
            .collect::<Vec<_>>()
            .join(", ");
        let detail = if joined.chars().count() <= 220 {
            joined
        } else {
            format!("{}…", joined.chars().take(217).collect::<String>())
        };
        let mut message = format!("{detail}\n\n{} service(s) down", outages.len());
        if recoveries > 0 {
            message.push_str(&format!(", {recoveries} recovered"));
        }
        vec![Event {
            title: format!("{} services down", outages.len()),
            message,
            priority: PRIORITY_OUTAGE,
            tags: vec!["rotating_light".into()],
            click: String::new(),
        }]
    }
}

/// Hard ceiling on messages per window, so a flapping service cannot silence your phone.
pub struct RateLimiter {
    pub window_s: f64,
    pub limit: usize,
    sent: Vec<f64>,
    pub suppressed: usize,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(RATE_WINDOW_S, MAX_MESSAGES_PER_WINDOW)
    }
}

impl RateLimiter {
    pub fn new(window_s: f64, limit: usize) -> Self {
        Self { window_s, limit, sent: Vec::new(), suppressed: 0 }
    }

    pub fn allow(&mut self, now: f64) -> bool {
        let cutoff = now - self.window_s;
        self.sent.retain(|t| *t >= cutoff);
        if self.sent.len() >= self.limit {
            self.suppressed += 1;
            return false;
        }
        self.sent.push(now);
        true
    }
}

/// I8: bypass the proxy unless the sink explicitly needs it (Telegram from a censored net).
fn agent_for(use_proxy: bool) -> ureq::Agent {
    let builder = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(6));
    if use_proxy {
        builder.build()
    } else {
        builder.try_proxy_from_env(false).build()
    }
}

pub fn sink_ntfy_base(sink: &Sink) -> String {
    let base = if sink.url.is_empty() { "https://ntfy.sh" } else { &sink.url };
    base.trim_end_matches('/').to_string()
}

/// POST one event with headers and a plain-text body (I7).
pub fn publish_ntfy(sink: &Sink, topic: &str, event: &Event) -> Result<i32, String> {
    if topic.is_empty() {
        return Err("no topic configured".into());
    }
    let token = if sink.token_env.is_empty() {
        String::new()
    } else {
        std::env::var(&sink.token_env).unwrap_or_default()
    };

    let ascii_title: String = event.title.chars().filter(|c| c.is_ascii()).collect();
    let title: String = ascii_title.chars().take(120).collect();

    let url = format!("{}/{}", sink_ntfy_base(sink), topic);
    let body: String = event.message.chars().take(500).collect();

    let mut req = agent_for(sink.use_proxy)
        .post(&url)
        .set("Content-Type", "text/plain; charset=utf-8")
        .set("X-Title", &title)
        .set("X-Priority", &event.priority.to_string())
        .set("X-Tags", &event.tags.join(","))
        .set("User-Agent", "observer-frog/0.1");
    if !event.click.is_empty() {
        req = req.set("X-Click", &event.click);
    }
    if !token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    match req.send_string(&body) {
        Ok(resp) => Ok(resp.status() as i32),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Owns the digest, the rate limit and the sink list.
pub struct Notifier {
    pub sinks: Vec<Sink>,
    pub topics: std::collections::HashMap<String, String>,
    pub dashboard_url: String,
    pub digest: Digest,
    pub limiter: RateLimiter,
    pub failures: Vec<String>,
}

impl Notifier {
    pub fn new(sinks: Vec<Sink>, topics: std::collections::HashMap<String, String>, dashboard_url: &str) -> Self {
        Self {
            sinks,
            topics,
            dashboard_url: dashboard_url.to_string(),
            digest: Digest::new(),
            limiter: RateLimiter::default(),
            failures: Vec::new(),
        }
    }

    pub fn submit(&mut self, changes: &[Change]) {
        for e in events_from_changes(changes, &self.dashboard_url) {
            self.digest.add(e);
        }
    }

    /// Send everything buffered. Returns the events actually delivered.
    pub fn flush(&mut self, now: Option<f64>) -> Vec<Event> {
        let now = now.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        });
        let mut delivered = Vec::new();
        for event in self.digest.drain() {
            if !self.limiter.allow(now) {
                eprintln!(
                    "rate limit: dropping {:?} (suppressed={})",
                    event.title, self.limiter.suppressed
                );
                continue;
            }
            if self.deliver(&event) {
                delivered.push(event);
            }
        }
        delivered
    }

    fn deliver(&mut self, event: &Event) -> bool {
        let mut sent = false;
        // Clone what we need so the borrow of self.sinks does not outlive the failure push.
        let sinks = self.sinks.clone();
        let topics = self.topics.clone();
        for sink in &sinks {
            if sink.kind == "ntfy" {
                let topic = topics.get("ntfy").cloned().unwrap_or_else(|| sink.topic.clone());
                match publish_ntfy(sink, &topic, event) {
                    Ok(_) => sent = true,
                    Err(info) => {
                        self.failures.push(format!("ntfy {}: {info}", sink_ntfy_base(sink)));
                        eprintln!("ntfy publish failed: {info}");
                    }
                }
            } else {
                eprintln!("unknown sink type {:?} skipped", sink.kind);
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(id: &str, from: &str, to: &str) -> Change {
        Change {
            node_id: "local".into(),
            service_id: id.into(),
            name: id.to_uppercase(),
            group: "G".into(),
            from_state: from.into(),
            to_state: to.into(),
            detail: "detail".into(),
            ts: 0,
            ignored: false,
            first_sighting: false,
        }
    }

    #[test]
    fn first_sighting_never_pages() {
        let mut c = change("a", "new", "down");
        c.first_sighting = true;
        assert!(events_from_changes(&[c], "").is_empty());
    }

    #[test]
    fn outage_and_recovery_render() {
        let ev = events_from_changes(&[change("a", "up", "down")], "http://d/");
        assert_eq!(ev[0].title, "A is down");
        assert_eq!(ev[0].priority, PRIORITY_OUTAGE);
        let ev2 = events_from_changes(&[change("a", "down", "up")], "http://d/");
        assert_eq!(ev2[0].title, "A recovered");
        assert_eq!(ev2[0].priority, PRIORITY_RECOVERY);
    }

    #[test]
    fn burst_coalesces_to_one_message() {
        let mut d = Digest::new();
        for i in 0..8 {
            d.add(Event {
                title: format!("S{i} is down"),
                message: "x".into(),
                priority: PRIORITY_OUTAGE,
                tags: vec![],
                click: String::new(),
            });
        }
        let out = d.drain();
        assert_eq!(out.len(), 1, "a burst must become one page");
        assert!(out[0].title.contains("8 services down"));
    }

    #[test]
    fn single_event_passes_through_unchanged() {
        let mut d = Digest::new();
        d.add(Event {
            title: "Only is down".into(),
            message: "b".into(),
            priority: PRIORITY_OUTAGE,
            tags: vec![],
            click: String::new(),
        });
        let out = d.drain();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Only is down");
    }

    #[test]
    fn rate_limiter_caps_a_flapping_service() {
        let mut rl = RateLimiter::new(60.0, 3);
        assert!(rl.allow(0.0));
        assert!(rl.allow(1.0));
        assert!(rl.allow(2.0));
        assert!(!rl.allow(3.0), "fourth message in the window should be suppressed");
        assert_eq!(rl.suppressed, 1);
        assert!(rl.allow(61.0), "the window should have rolled over");
    }
}
