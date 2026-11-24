//! Управление локальным движком Soloist и разбор `soloist ctl now --json`.

use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

const WS_ADDR: &str = "127.0.0.1:9090";
const NOW_TIMEOUT: Duration = Duration::from_secs(4);
const CALL_TIMEOUT: Duration = Duration::from_secs(8);
const ACTIVATE_PAUSE: Duration = Duration::from_millis(300);
const PREV_PAUSE: Duration = Duration::from_millis(300);
/// Spotify «prev» после 3 с проигрывания лишь перематывает трек в начало.
const TERM_GRACE: Duration = Duration::from_millis(300);
const PREV_REWIND_MS: u64 = 3000;

#[derive(Clone, Default, Debug, PartialEq, Serialize)]
pub struct SpotState {
    pub status: String,
    pub uri: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub cover: String,
    pub pos_ms: u64,
    pub dur_ms: u64,
    pub context: String,
    pub shuffle: bool,
}

/// Разбор `soloist ctl now --json`. Мусор и состояние без трека — `None`.
pub fn parse_now(json: &str) -> Option<SpotState> {
    let v: Value = serde_json::from_str(json.trim()).ok()?;
    let item = v.get("item")?;
    let uri = item.get("uri")?.as_str()?.to_string();
    if uri.is_empty() {
        return None;
    }
    let deco = item.get("decorations");
    let name_of = |n: Option<&Value>| -> String {
        n.and_then(|d| d.pointer("/identity/name")).and_then(Value::as_str).unwrap_or("").to_string()
    };
    let artists = deco
        .and_then(|d| d.get("creators"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|c| name_of(c.pointer("/entity/decorations")))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let covers = deco.and_then(|d| d.pointer("/visual_identity/cover")).and_then(Value::as_array);
    let cover = covers
        .and_then(|a| {
            a.iter()
                .find(|c| c.get("size").and_then(Value::as_str) == Some("large"))
                .or_else(|| a.last())
        })
        .and_then(|c| c.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let status = match v.get("status").and_then(Value::as_str) {
        Some(s @ ("playing" | "paused")) => s.to_string(),
        _ => String::new(),
    };
    Some(SpotState {
        status,
        uri,
        title: name_of(deco),
        artists,
        album: name_of(deco.and_then(|d| d.pointer("/parent/entity/decorations"))),
        cover,
        pos_ms: v.pointer("/position/position_ms").and_then(Value::as_u64).unwrap_or(0),
        dur_ms: deco.and_then(|d| d.pointer("/playback/duration_ms")).and_then(Value::as_u64).unwrap_or(0),
        context: v.pointer("/context/uri").and_then(Value::as_str).unwrap_or("").to_string(),
        shuffle: v.pointer("/options/shuffle").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Сколько трек уже сыграл: позиция + время с момента снимка, если скорость не нулевая
/// (как в request_state.sh). Без валидной позиции — `None`.
fn played_ms(json: &str, now_ms: u64) -> Option<u64> {
    let v: Value = serde_json::from_str(json.trim()).ok()?;
    let pos = v.pointer("/position/position_ms")?.as_u64()?;
    let ts = v.pointer("/position/timestamp_ms").and_then(Value::as_u64).unwrap_or(0);
    let speed = v.pointer("/position/speed").and_then(Value::as_f64).unwrap_or(0.0);
    if ts < 1000 {
        return Some(pos);
    }
    Some(if speed != 0.0 { pos + now_ms.saturating_sub(ts) } else { pos })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Play(Option<String>),
    Pause,
    Resume,
    Next,
    Prev,
    Shuffle(bool),
    Seek(u64),
}

/// Ответ Soloist на опрос состояния.
#[derive(Debug, PartialEq)]
pub enum Probe {
    /// Не запустился, завис или вернул ошибку.
    Down,
    /// Отвечает, но трека нет.
    Idle,
    Active(SpotState),
}

pub struct Soloist {
    dir: PathBuf,
}

impl Soloist {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Soloist { dir: dir.into() }
    }

    fn control_sh(&self) -> PathBuf {
        self.dir.join("scripts/control.sh")
    }

    /// Запускает команду в своей группе процессов; по таймауту убивает всю группу
    /// (скрипт и его потомков) и дожидается завершения. `None` — не запустилась или не уложилась.
    async fn run(&self, mut cmd: tokio::process::Command, timeout: Duration) -> Option<(bool, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        cmd.current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().ok()?;
        let pid = child.id();
        let mut stdout = child.stdout.take()?;
        let done = tokio::time::timeout(timeout, async {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).await.ok()?;
            let st = child.wait().await.ok()?;
            Some((st.success(), buf))
        })
        .await;
        match done {
            Ok(r) => r,
            Err(_) => {
                if let Some(pid) = pid {
                    // Группа = pid ведущего процесса (process_group(0)). Сначала TERM: control.sh
                    // чистит свои временные файлы в trap, SIGKILL оставил бы selection.pending.
                    // SAFETY: обычный системный вызов, память не затрагивается.
                    unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
                    tokio::time::sleep(TERM_GRACE).await;
                    // SAFETY: то же; child ещё не прибран, pid не переиспользован.
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let _ = child.kill().await;
                None
            }
        }
    }

    fn ctl_cmd(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(self.dir.join("bin/soloist"));
        cmd.args(["ctl", "-w", WS_ADDR]).args(args).env("LD_LIBRARY_PATH", self.dir.join("lib64"));
        cmd
    }

    async fn ctl(&self, args: &[&str], timeout: Duration) -> Result<Vec<u8>, String> {
        let verb = args.first().copied().unwrap_or("");
        match self.run(self.ctl_cmd(args), timeout).await {
            Some((true, out)) => Ok(out),
            Some((false, _)) => Err(format!("Soloist {verb}: команда не удалась")),
            None => Err(format!("Soloist {verb}: не ответил")),
        }
    }

    async fn control(&self, args: &[&str]) -> Result<(), String> {
        let verb = args.first().copied().unwrap_or("");
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg(self.control_sh()).args(args);
        match self.run(cmd, CALL_TIMEOUT).await {
            Some((true, _)) => Ok(()),
            Some((false, _)) => Err(format!("control.sh {verb}: команда не удалась")),
            None => Err(format!("control.sh {verb}: не ответил")),
        }
    }

    /// Текущее состояние; Soloist не запущен, завис или вернул мусор — `None`.
    pub async fn now(&self) -> Option<SpotState> {
        parse_now(&self.now_raw().await?)
    }

    /// Как `now`, но отличает «Soloist не отвечает» от «ничего не играет».
    pub async fn probe(&self) -> Probe {
        match self.now_raw().await {
            None => Probe::Down,
            Some(raw) => match parse_now(&raw).filter(|s| !s.status.is_empty()) {
                Some(s) => Probe::Active(s),
                None => Probe::Idle,
            },
        }
    }

    async fn now_raw(&self) -> Option<String> {
        let (ok, out) = self.run(self.ctl_cmd(&["now", "--json"]), NOW_TIMEOUT).await?;
        ok.then(|| String::from_utf8_lossy(&out).into_owned())
    }

    pub async fn cmd(&self, op: Op) -> Result<(), String> {
        if let Op::Seek(ms) = op {
            return self.ctl(&["seek", &ms.to_string()], CALL_TIMEOUT).await.map(|_| ());
        }
        if self.control_sh().is_file() {
            return match &op {
                Op::Play(Some(uri)) => self.control(&["play", uri]).await,
                Op::Play(None) | Op::Resume => self.control(&["play"]).await,
                Op::Pause => self.control(&["pause"]).await,
                Op::Next => self.control(&["next"]).await,
                Op::Prev => self.control(&["prev"]).await,
                Op::Shuffle(on) => self.control(&["shuffle", if *on { "on" } else { "off" }]).await,
                Op::Seek(_) => unreachable!(),
            };
        }
        match &op {
            Op::Play(uri) => {
                self.ctl(&["activate"], CALL_TIMEOUT).await?;
                tokio::time::sleep(ACTIVATE_PAUSE).await;
                match uri {
                    Some(u) => self.ctl(&["play", u], CALL_TIMEOUT).await,
                    None => self.ctl(&["play"], CALL_TIMEOUT).await,
                }
            }
            Op::Resume => {
                self.ctl(&["activate"], CALL_TIMEOUT).await?;
                tokio::time::sleep(ACTIVATE_PAUSE).await;
                self.ctl(&["play"], CALL_TIMEOUT).await
            }
            Op::Pause => self.ctl(&["pause"], CALL_TIMEOUT).await,
            Op::Next => self.ctl(&["next"], CALL_TIMEOUT).await,
            Op::Prev => {
                // «Назад» всегда предыдущий трек: после 3 с первый prev лишь перематывает в начало.
                let raw = self.now_raw().await.unwrap_or_default();
                if played_ms(&raw, now_epoch_ms()).is_some_and(|p| p >= PREV_REWIND_MS) {
                    self.ctl(&["prev"], CALL_TIMEOUT).await?;
                    tokio::time::sleep(PREV_PAUSE).await;
                }
                self.ctl(&["prev"], CALL_TIMEOUT).await
            }
            Op::Shuffle(on) => self.ctl(&["shuffle", if *on { "on" } else { "off" }], CALL_TIMEOUT).await,
            Op::Seek(_) => unreachable!(),
        }
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_now_reads_track() {
        let s = parse_now(include_str!("../tests/fixtures/now_playing.json")).unwrap();
        assert_eq!(s.status, "paused");
        assert_eq!(s.title, "ONLY POSITIVE");
        assert_eq!(s.artists, "Endspiel, TumaniYO");
        assert_eq!(s.album, "CHAMPION SOUND");
        assert_eq!(s.cover, "https://i.scdn.co/image/large");
        assert_eq!(s.pos_ms, 62762);
        assert_eq!(s.dur_ms, 178614);
        assert!(s.shuffle);
    }
    #[test]
    fn played_ms_accounts_for_staleness() {
        let j = |pos: u64, ts: u64, sp: u32| format!("{{\"position\":{{\"position_ms\":{pos},\"timestamp_ms\":{ts},\"speed\":{sp}}}}}");
        assert_eq!(played_ms(&j(1000, 1_700_000_000_000, 1), 1_700_000_010_000), Some(11_000));
        assert_eq!(played_ms(&j(1000, 1_700_000_000_000, 0), 1_700_000_010_000), Some(1000));
        assert_eq!(played_ms("garbage", 0), None);
    }
    #[test]
    fn parse_now_rejects_garbage() {
        assert!(parse_now("").is_none());
        assert!(parse_now("error: connection refused").is_none());
        assert!(parse_now("{\"type\":\"playback_state\"}").map_or(true, |s| s.uri.is_empty()));
    }
}
