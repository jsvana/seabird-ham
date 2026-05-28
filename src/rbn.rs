use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

pub const DEFAULT_DB_PATH: &str = "./seabird-ham-rbn.db";

const QSY_THRESHOLD_HZ: i64 = 500;
const GAP_DURATION_MINUTES: i64 = 30;
const RECONNECT_MIN_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(300);

pub fn default_servers() -> Vec<(String, u16)> {
    vec![
        ("telnet.reversebeacon.net".to_string(), 7000),
        ("telnet.reversebeacon.net".to_string(), 7001),
    ]
}

#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub channel_id: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LastSpot {
    pub frequency_hz: i64,
    pub mode: String,
    pub spotted_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Spot {
    pub spotter: String,
    pub callsign: String,
    pub frequency_hz: i64,
    pub mode: String,
    pub spotted_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct RbnDb {
    conn: Arc<Mutex<Connection>>,
}

impl RbnDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS monitored_callsigns (
                channel_id TEXT NOT NULL,
                callsign TEXT NOT NULL,
                added_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (channel_id, callsign)
            );
            CREATE TABLE IF NOT EXISTS channel_spot_state (
                channel_id TEXT NOT NULL,
                callsign TEXT NOT NULL,
                last_frequency_hz INTEGER NOT NULL,
                last_mode TEXT NOT NULL,
                last_spotted_at TEXT NOT NULL,
                PRIMARY KEY (channel_id, callsign)
            );
            CREATE INDEX IF NOT EXISTS idx_monitored_callsign
                ON monitored_callsigns (callsign);
            "#,
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| anyhow!("db lock poisoned"))
    }

    pub fn add_monitored(&self, channel_id: &str, callsign: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO monitored_callsigns(channel_id, callsign) VALUES (?1, ?2)",
            params![channel_id, callsign],
        )?;
        Ok(n > 0)
    }

    pub fn remove_monitored(&self, channel_id: &str, callsign: &str) -> Result<bool> {
        let conn = self.lock()?;
        let n = conn.execute(
            "DELETE FROM monitored_callsigns WHERE channel_id = ?1 AND callsign = ?2",
            params![channel_id, callsign],
        )?;
        conn.execute(
            "DELETE FROM channel_spot_state WHERE channel_id = ?1 AND callsign = ?2",
            params![channel_id, callsign],
        )?;
        Ok(n > 0)
    }

    pub fn list_monitored(&self, channel_id: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT callsign FROM monitored_callsigns WHERE channel_id = ?1 ORDER BY callsign",
        )?;
        let rows = stmt.query_map(params![channel_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn channels_monitoring(&self, callsign: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt =
            conn.prepare("SELECT channel_id FROM monitored_callsigns WHERE callsign = ?1")?;
        let rows = stmt.query_map(params![callsign], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_last_spot(&self, channel_id: &str, callsign: &str) -> Result<Option<LastSpot>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT last_frequency_hz, last_mode, last_spotted_at
                   FROM channel_spot_state
                  WHERE channel_id = ?1 AND callsign = ?2",
                params![channel_id, callsign],
                |r| {
                    let freq: i64 = r.get(0)?;
                    let mode: String = r.get(1)?;
                    let when: String = r.get(2)?;
                    Ok((freq, mode, when))
                },
            )
            .optional()?;
        match row {
            Some((freq, mode, when)) => {
                let spotted_at = DateTime::parse_from_rfc3339(&when)
                    .map_err(|e| anyhow!("invalid spotted_at in db: {e}"))?
                    .with_timezone(&Utc);
                Ok(Some(LastSpot {
                    frequency_hz: freq,
                    mode,
                    spotted_at,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn upsert_last_spot(&self, channel_id: &str, callsign: &str, spot: &Spot) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO channel_spot_state(channel_id, callsign, last_frequency_hz, last_mode, last_spotted_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(channel_id, callsign) DO UPDATE SET
                last_frequency_hz = excluded.last_frequency_hz,
                last_mode = excluded.last_mode,
                last_spotted_at = excluded.last_spotted_at",
            params![
                channel_id,
                callsign,
                spot.frequency_hz,
                spot.mode,
                spot.spotted_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }
}

fn spot_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Example RBN line:
        // DX de SK3W-#:    14045.0  N1AW         CW    27 dB  31 WPM  CQ      0335Z
        Regex::new(r"^DX de\s+(\S+?):\s+(\d+(?:\.\d+)?)\s+(\S+)\s+(\S+)\s+").unwrap()
    })
}

pub fn parse_spot(line: &str) -> Option<Spot> {
    let caps = spot_regex().captures(line)?;
    let spotter = caps.get(1)?.as_str().to_string();
    let freq_khz: f64 = caps.get(2)?.as_str().parse().ok()?;
    let callsign = caps.get(3)?.as_str().to_uppercase();
    let mode = caps.get(4)?.as_str().to_uppercase();
    Some(Spot {
        spotter,
        callsign,
        frequency_hz: (freq_khz * 1000.0).round() as i64,
        mode,
        spotted_at: Utc::now(),
    })
}

fn freq_display(hz: i64) -> String {
    let mhz = hz / 1_000_000;
    let khz = (hz % 1_000_000) / 1_000;
    let sub = (hz % 1_000) / 100;
    if sub > 0 {
        format!("{mhz}.{khz:03}.{sub}")
    } else {
        format!("{mhz}.{khz:03}")
    }
}

fn format_alert(reason: AlertReason, spot: &Spot, prev: Option<&LastSpot>) -> String {
    let f = freq_display(spot.frequency_hz);
    match (reason, prev) {
        (AlertReason::New, _) => format!(
            "[rbn:new] {} {} MHz {}, de {}",
            spot.callsign, f, spot.mode, spot.spotter
        ),
        (AlertReason::Qsy, Some(p)) => format!(
            "[rbn:qsy] {} {} MHz {} (from {} MHz {}), de {}",
            spot.callsign,
            f,
            spot.mode,
            freq_display(p.frequency_hz),
            p.mode,
            spot.spotter
        ),
        (AlertReason::Mode, Some(p)) => format!(
            "[rbn:mode] {} {} MHz {} (was {}), de {}",
            spot.callsign, f, spot.mode, p.mode, spot.spotter
        ),
        (AlertReason::Active, Some(p)) => format!(
            "[rbn:active] {} {} MHz {} (back after {}m), de {}",
            spot.callsign,
            f,
            spot.mode,
            (spot.spotted_at - p.spotted_at).num_minutes(),
            spot.spotter
        ),
        (reason, None) => format!(
            "[rbn:{reason:?}] {} {} MHz {}, de {}",
            spot.callsign, f, spot.mode, spot.spotter
        ),
    }
}

#[derive(Debug, Clone, Copy)]
enum AlertReason {
    New,
    Qsy,
    Mode,
    Active,
}

fn classify(spot: &Spot, prev: Option<&LastSpot>) -> Option<AlertReason> {
    match prev {
        None => Some(AlertReason::New),
        Some(p) => {
            if (p.frequency_hz - spot.frequency_hz).abs() > QSY_THRESHOLD_HZ {
                Some(AlertReason::Qsy)
            } else if p.mode != spot.mode {
                Some(AlertReason::Mode)
            } else if (spot.spotted_at - p.spotted_at).num_minutes() > GAP_DURATION_MINUTES {
                Some(AlertReason::Active)
            } else {
                None
            }
        }
    }
}

pub fn process_spot(
    spot: &Spot,
    db: &RbnDb,
    outbound: &UnboundedSender<OutboundMessage>,
) -> Result<()> {
    let channels = db.channels_monitoring(&spot.callsign)?;
    if channels.is_empty() {
        return Ok(());
    }
    for channel in &channels {
        let prev = db.get_last_spot(channel, &spot.callsign)?;
        if let Some(reason) = classify(spot, prev.as_ref()) {
            let text = format_alert(reason, spot, prev.as_ref());
            let _ = outbound.send(OutboundMessage {
                channel_id: channel.clone(),
                text,
            });
        }
        db.upsert_last_spot(channel, &spot.callsign, spot)?;
    }
    Ok(())
}

pub fn spawn_connections(
    servers: Vec<(String, u16)>,
    callsign: String,
    db: RbnDb,
    outbound: UnboundedSender<OutboundMessage>,
) {
    for (host, port) in servers {
        let cs = callsign.clone();
        let dbc = db.clone();
        let out = outbound.clone();
        tokio::spawn(async move {
            connection_loop(host, port, cs, dbc, out).await;
        });
    }
}

async fn connection_loop(
    host: String,
    port: u16,
    callsign: String,
    db: RbnDb,
    outbound: UnboundedSender<OutboundMessage>,
) {
    let mut delay = RECONNECT_MIN_DELAY;
    loop {
        info!("RBN: connecting to {host}:{port}");
        match run_connection(&host, port, &callsign, &db, &outbound).await {
            Ok(()) => warn!("RBN: {host}:{port} closed cleanly"),
            Err(e) => error!("RBN: {host}:{port} error: {e:?}"),
        }
        warn!("RBN: reconnecting to {host}:{port} in {delay:?}");
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, RECONNECT_MAX_DELAY);
    }
}

async fn run_connection(
    host: &str,
    port: u16,
    callsign: &str,
    db: &RbnDb,
    outbound: &UnboundedSender<OutboundMessage>,
) -> Result<()> {
    let addr = format!("{host}:{port}");
    let stream = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("connect timeout {addr}"))??;
    info!("RBN: connected to {addr}");

    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(format!("{callsign}\r\n").as_bytes())
        .await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half).lines();
    loop {
        let next = tokio::time::timeout(READ_TIMEOUT, reader.next_line()).await;
        match next {
            Err(_) => return Err(anyhow!("read timeout {addr}")),
            Ok(Ok(Some(line))) => {
                if let Some(spot) = parse_spot(&line) {
                    if let Err(e) = process_spot(&spot, db, outbound) {
                        warn!("RBN: error processing spot: {e:?}");
                    }
                } else {
                    debug!("RBN: ignored line: {line}");
                }
            }
            Ok(Ok(None)) => return Err(anyhow!("EOF from {addr}")),
            Ok(Err(e)) => return Err(anyhow!("read error from {addr}: {e}")),
        }
    }
}

#[derive(Debug, Deserialize)]
struct VailResponse {
    spots: Vec<VailSpot>,
}

#[derive(Debug, Deserialize)]
pub struct VailSpot {
    pub callsign: String,
    pub frequency: f64,
    pub mode: String,
    pub timestamp: String,
}

pub async fn lookup_latest_spot(callsign: &str) -> Result<Option<VailSpot>> {
    let url = format!("https://vailrerbn.com/api/v1/spots/{callsign}?hours=24");
    let resp = tokio::time::timeout(Duration::from_secs(10), async {
        reqwest::get(&url).await?.json::<VailResponse>().await
    })
    .await
    .map_err(|_| anyhow!("timeout looking up {callsign}"))??;
    Ok(resp.spots.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cw_spot() {
        let line = "DX de SK3W-#:    14045.0  N1AW         CW    27 dB  31 WPM  CQ      0335Z";
        let spot = parse_spot(line).expect("parse");
        assert_eq!(spot.spotter, "SK3W-#");
        assert_eq!(spot.callsign, "N1AW");
        assert_eq!(spot.mode, "CW");
        assert_eq!(spot.frequency_hz, 14_045_000);
    }

    #[test]
    fn parse_ft8_spot() {
        let line = "DX de SK3W-#:    14074.0  K1ABC        FT8    -8 dB  1647 Hz  CQ      0335Z";
        let spot = parse_spot(line).expect("parse");
        assert_eq!(spot.callsign, "K1ABC");
        assert_eq!(spot.mode, "FT8");
        assert_eq!(spot.frequency_hz, 14_074_000);
    }

    #[test]
    fn ignore_non_dx_line() {
        assert!(parse_spot("Welcome to the RBN").is_none());
        assert!(parse_spot("").is_none());
    }

    #[test]
    fn classify_new() {
        let spot = Spot {
            spotter: "X".into(),
            callsign: "K1ABC".into(),
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: Utc::now(),
        };
        assert!(matches!(classify(&spot, None), Some(AlertReason::New)));
    }

    #[test]
    fn classify_suppressed_when_same() {
        let now = Utc::now();
        let spot = Spot {
            spotter: "X".into(),
            callsign: "K1ABC".into(),
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: now,
        };
        let prev = LastSpot {
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: now - chrono::Duration::seconds(30),
        };
        assert!(classify(&spot, Some(&prev)).is_none());
    }

    #[test]
    fn classify_qsy() {
        let now = Utc::now();
        let spot = Spot {
            spotter: "X".into(),
            callsign: "K1ABC".into(),
            frequency_hz: 14_050_000,
            mode: "CW".into(),
            spotted_at: now,
        };
        let prev = LastSpot {
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: now - chrono::Duration::seconds(30),
        };
        assert!(matches!(classify(&spot, Some(&prev)), Some(AlertReason::Qsy)));
    }

    #[test]
    fn classify_mode_change() {
        let now = Utc::now();
        let spot = Spot {
            spotter: "X".into(),
            callsign: "K1ABC".into(),
            frequency_hz: 14_045_000,
            mode: "RTTY".into(),
            spotted_at: now,
        };
        let prev = LastSpot {
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: now - chrono::Duration::seconds(30),
        };
        assert!(matches!(classify(&spot, Some(&prev)), Some(AlertReason::Mode)));
    }

    #[test]
    fn classify_active_after_gap() {
        let now = Utc::now();
        let spot = Spot {
            spotter: "X".into(),
            callsign: "K1ABC".into(),
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: now,
        };
        let prev = LastSpot {
            frequency_hz: 14_045_000,
            mode: "CW".into(),
            spotted_at: now - chrono::Duration::minutes(45),
        };
        assert!(matches!(
            classify(&spot, Some(&prev)),
            Some(AlertReason::Active)
        ));
    }
}
