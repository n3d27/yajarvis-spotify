//! Инструменты `spotify_play` / `spotify_control`, быстрые фразы и настройки плагина.

use crate::keys::Keys;
use crate::pick::Kind;
use crate::soloist::{Op, Probe, Soloist, SpotState};
use crate::talk::{Talk, run_start};
use crate::web;
use jarvis_plugin_sdk::{Map, Plugin, Response, Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_SOLOIST_DIR: &str = "/data/opt/soloist";
const TOOL_BUDGET: Duration = Duration::from_secs(13);
/// Ядро убивает плагин, не уложившийся в 2 с, и не перезапускает его: держим запас.
const COMMAND_BUDGET: Duration = Duration::from_millis(1500);
/// Опрос трека для «лайка» внутри `COMMAND_BUDGET`.
const LIKE_NOW_BUDGET: Duration = Duration::from_millis(1100);
const DEFERRED: &str = " Включу после ответа.";
const NO_KEYS: &str = "Spotify не ответил: нет ключей.";
const NOTHING: &str = "Spotify сейчас ничего не играет.";
const SOLOIST_DOWN: &str = "Spotify не отвечает: Soloist не запущен или не установлен.";
const STATUS_NO_KEYS: &str = " Ключей Spotify нет — поиск и лайки недоступны.";
const STATUS_NO_WATCH: &str = " Пауза на время разговора выключена.";
const TOKEN_FILE: &str = "token.json";

const PLAY_DESC: &str = "Spotify — основной источник музыки. Любую музыку, песню, исполнителя, альбом, плейлист, подкаст сначала включай этим инструментом. play_music (YouTube) — только если этот ответил «В Spotify не нашёл» или просят видео, ролик, обзор, «на ютубе». «Моё любимое» — kind=liked; liked_shuffle — только если прямо сказали «вперемешку», «в случайном порядке», «рандом» или «перемешай»; wave — «рекомендации», «моя волна».";
const CONTROL_DESC: &str = "Управление тем, что играет в Spotify: пауза, продолжить, дальше, назад (всегда предыдущий трек), лайк, убрать из любимых, перемешать, по порядку, что играет. Если играет Spotify — используй это, а не music_control.";

pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "name": "spotify_play",
            "description": PLAY_DESC,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Что искать: название, исполнитель, альбом, плейлист, подкаст"},
                    "kind": {"type": "string", "enum": ["any", "track", "artist", "album", "playlist", "podcast", "liked", "liked_shuffle", "wave"]}
                },
                "required": ["kind"]
            }
        }),
        json!({
            "type": "function",
            "name": "spotify_control",
            "description": CONTROL_DESC,
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["pause", "resume", "next", "prev", "like", "unlike", "shuffle_on", "shuffle_off", "status"]}
                },
                "required": ["command"]
            }
        }),
    ]
}

/// Что именно играть.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Find(Kind),
    Liked,
    LikedShuffle,
    Wave,
}

fn parse_want(kind: &str) -> Want {
    match kind {
        "track" => Want::Find(Kind::Track),
        "artist" => Want::Find(Kind::Artist),
        "album" => Want::Find(Kind::Album),
        "playlist" => Want::Find(Kind::Playlist),
        "podcast" => Want::Find(Kind::Podcast),
        "liked" => Want::Liked,
        "liked_shuffle" => Want::LikedShuffle,
        "wave" => Want::Wave,
        _ => Want::Find(Kind::Any),
    }
}

/// Короткая причина для «Spotify не ответил: …».
fn short(e: &str) -> String {
    let line = e.lines().next().unwrap_or("").trim().trim_end_matches('.');
    let line: String = line.chars().take(80).collect();
    if line.is_empty() { "ошибка".into() } else { line }
}

fn fail(e: &str) -> String {
    format!("Spotify не ответил: {}.", short(e))
}

/// Всё, что зависит от настроек; при `configure` собирается заново.
struct Ctx {
    keys: Keys,
    soloist: Arc<Soloist>,
    store: PathBuf,
}

impl Ctx {
    fn build(settings: &Map<String, Value>) -> Ctx {
        let dir = settings
            .get("soloist_dir")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_SOLOIST_DIR);
        Ctx { keys: Keys::resolve(settings, Path::new(dir)), soloist: Arc::new(Soloist::new(dir)), store: PathBuf::from(TOKEN_FILE) }
    }

    /// Нужен ли доступ пользователя Web API для этого запроса.
    fn missing_keys(&self, want: Want) -> bool {
        match want {
            Want::Find(_) => !self.keys.can_search(),
            Want::Liked | Want::LikedShuffle => self.keys.liked_uri.is_empty() && !self.keys.has_user(),
            Want::Wave => !self.keys.has_user(),
        }
    }
}

#[derive(Clone)]
struct Shared {
    ctx: Arc<Ctx>,
    talk: Arc<Talk>,
    /// Наблюдатель за разговором запущен.
    watching: Arc<AtomicBool>,
}

impl Shared {
    /// Запуск. Во время разговора не играет сразу, а откладывает последовательность на его конец.
    async fn play(&self, query: &str, want: Want) -> String {
        let c = &self.ctx;
        if c.missing_keys(want) {
            return NO_KEYS.into();
        }
        let query = query.trim();
        let (uri, ok_text, pre) = match want {
            Want::Find(kind) => match web::search(&c.keys, query, kind).await {
                Ok(Some(p)) => (p.uri, format!("Включил в Spotify: {}.", p.label), None),
                Ok(None) => return format!("В Spotify не нашёл «{query}» — включи через play_music."),
                Err(e) => return fail(&e),
            },
            Want::Liked | Want::LikedShuffle => match web::liked_uri(&c.keys, &c.store).await {
                Ok(u) if want == Want::Liked => (u, "Включил любимое в Spotify.".to_string(), Some(false)),
                Ok(u) => (u, "Включил любимое вперемешку в Spotify.".to_string(), Some(true)),
                Err(e) => return fail(&e),
            },
            Want::Wave => match web::wave_uri(&c.keys, &c.store).await {
                Ok(u) => (u, "Включил рекомендации в Spotify.".to_string(), None),
                Err(e) => return fail(&e),
            },
        };
        let mut ops = Vec::new();
        if let Some(on) = pre {
            ops.push(Op::Shuffle(on));
        }
        ops.push(Op::Play(Some(uri)));
        if self.talk.defer_start(ops.clone(), pre) {
            return format!("{ok_text}{DEFERRED}");
        }
        match run_start(&*c.soloist, ops, pre).await {
            Ok(()) => ok_text,
            Err(e) => fail(&e),
        }
    }

    async fn like(&self, on: bool) -> String {
        let c = &self.ctx;
        if !c.keys.has_user() {
            return NO_KEYS.into();
        }
        let Some(SpotState { uri, .. }) = c.soloist.now().await else {
            return NOTHING.into();
        };
        match web::like(&c.keys, &uri, on, &c.store).await {
            Ok(()) => if on { "Добавил в любимые.".into() } else { "Убрал из любимых.".into() },
            Err(e) => fail(&e),
        }
    }

    async fn control(&self, command: &str) -> String {
        let c = &self.ctx;
        let simple = |r: Result<(), String>, ok: &str| match r {
            Ok(()) => ok.to_string(),
            Err(e) => fail(&e),
        };
        match command {
            "pause" => {
                self.talk.user_paused();
                simple(c.soloist.cmd(Op::Pause).await, "Поставил на паузу.")
            }
            "resume" => {
                // Запуск во время разговора заглушил бы ответ Джарвиса: ждём конца.
                if self.talk.defer_start(vec![Op::Resume], None) {
                    return format!("Продолжаю.{DEFERRED}");
                }
                simple(c.soloist.cmd(Op::Resume).await, "Продолжаю.")
            }
            "next" | "prev" => {
                let (op, ok) = if command == "next" { (Op::Next, "Следующий трек.") } else { (Op::Prev, "Предыдущий трек.") };
                // Со стоящей паузы Soloist может начать играть сам: во время разговора ничего
                // не делаем сразу. Смена трека и (если пауза наша) возобновление — на его конец.
                let mut ops = vec![op.clone()];
                if self.talk.paused_by_us() {
                    ops.push(Op::Resume);
                }
                if self.talk.defer_start(ops, None) {
                    return format!("{ok}{DEFERRED}");
                }
                simple(c.soloist.cmd(op).await, ok)
            }
            "shuffle_on" => simple(c.soloist.cmd(Op::Shuffle(true)).await, "Перемешал."),
            "shuffle_off" => simple(c.soloist.cmd(Op::Shuffle(false)).await, "Включил по порядку."),
            "like" => self.like(true).await,
            "unlike" => self.like(false).await,
            "status" => {
                let mut text = match c.soloist.probe().await {
                    Probe::Down => SOLOIST_DOWN.to_string(),
                    Probe::Idle => NOTHING.to_string(),
                    Probe::Active(s) => {
                        let state = if s.status == "playing" { "Играет" } else { "На паузе" };
                        if s.artists.is_empty() {
                            format!("{state} «{}».", s.title)
                        } else {
                            format!("{state} «{}» — {}.", s.title, s.artists)
                        }
                    }
                };
                if !c.keys.can_search() {
                    text.push_str(STATUS_NO_KEYS);
                }
                if !self.watching.load(Ordering::Relaxed) {
                    text.push_str(STATUS_NO_WATCH);
                }
                text
            }
            _ => "Spotify: неизвестная команда.".into(),
        }
    }

    async fn tool(&self, name: &str, args: &Value) -> String {
        let work = async {
            match name {
                "spotify_play" => {
                    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("any");
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    self.play(query, parse_want(kind)).await
                }
                "spotify_control" => self.control(args.get("command").and_then(Value::as_str).unwrap_or("")).await,
                _ => "Spotify: неизвестный инструмент.".into(),
            }
        };
        tokio::time::timeout(TOOL_BUDGET, work).await.unwrap_or_else(|_| fail("слишком долго"))
    }

    /// Быстрые фразы: отвечает сразу, работа идёт в фоне. `None` — не наша фраза.
    async fn command(&self, text: &str) -> Option<String> {
        enum Act {
            Like(bool),
            Play(Want, &'static str),
        }
        let n = normalize(text);
        let act = match n.as_str() {
            "лайк" | "нравится" | "добавь в любимые" => Act::Like(true),
            "дизлайк" | "убери из любимых" => Act::Like(false),
            "мое любимое" => Act::Play(Want::Liked, "Включил любимое в Spotify."),
            "любимое вперемешку" => Act::Play(Want::LikedShuffle, "Включил любимое вперемешку в Spotify."),
            "рекомендации" | "моя волна" => Act::Play(Want::Wave, "Включил рекомендации в Spotify."),
            _ => return None,
        };
        match act {
            Act::Like(on) => {
                if !self.ctx.keys.has_user() {
                    return None;
                }
                let has_track = tokio::time::timeout(LIKE_NOW_BUDGET, self.ctx.soloist.now()).await.ok().flatten().is_some();
                if !has_track {
                    return None;
                }
                let me = self.clone();
                tokio::spawn(async move {
                    let r = me.like(on).await;
                    if r.starts_with("Spotify") {
                        eprintln!("spotify-плагин: лайк не удался: {r}");
                    }
                });
                Some(if on { "Добавил в любимые." } else { "Убрал из любимых." }.into())
            }
            Act::Play(want, reply) => {
                if self.ctx.missing_keys(want) {
                    return None;
                }
                let me = self.clone();
                tokio::spawn(async move {
                    let r = me.play("", want).await;
                    if r.starts_with("Spotify") {
                        eprintln!("spotify-плагин: запуск не удался: {r}");
                    }
                });
                Some(reply.into())
            }
        }
    }
}

/// Как `command_text` ядра: нижний регистр, ё→е, без знаков, без «джарвис».
fn normalize(text: &str) -> String {
    let s = text.to_lowercase().replace('ё', "е");
    let cleaned: String = s.chars().map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' }).collect();
    let n = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    n.strip_prefix("джарвис ").map(str::to_string).unwrap_or(n)
}

/// Журнал ядра: `/tmp/jarvisd/jarvisd.log`, иначе `<data_dir>/jarvisd.log`
/// (рабочий каталог плагина — `<data_dir>/plugins/spotify/data`).
pub fn log_path(cwd: &Path) -> Option<PathBuf> {
    let primary = PathBuf::from("/tmp/jarvisd/jarvisd.log");
    if primary.is_file() {
        return Some(primary);
    }
    let fallback = cwd.join("../../..").join("jarvisd.log");
    fallback.is_file().then_some(fallback)
}

fn flag(settings: &Map<String, Value>, key: &str, default: bool) -> bool {
    settings.get(key).and_then(Value::as_bool).unwrap_or(default)
}

pub struct SpotifyPlugin {
    state: Mutex<Shared>,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cwd: PathBuf,
}

impl SpotifyPlugin {
    pub fn new(settings: &Map<String, Value>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let p = SpotifyPlugin {
            state: Mutex::new(Shared {
                ctx: Arc::new(Ctx::build(settings)),
                talk: Arc::new(Talk::default()),
                watching: Arc::new(AtomicBool::new(false)),
            }),
            watcher: Mutex::new(None),
            cwd,
        };
        p.apply(settings, true);
        p
    }

    fn shared(&self) -> Shared {
        self.state.lock().unwrap().clone()
    }

    /// Подставляет новые настройки и включает/выключает наблюдатель за разговором.
    fn apply(&self, settings: &Map<String, Value>, announce: bool) {
        let ctx = Arc::new(Ctx::build(settings));
        let (talk, watching) = {
            let mut st = self.state.lock().unwrap();
            if st.ctx.keys != ctx.keys {
                web::reset_tokens();
            }
            st.ctx = ctx.clone();
            (st.talk.clone(), st.watching.clone())
        };
        if let Some(h) = self.watcher.lock().unwrap().take() {
            h.abort();
        }
        let want_watch = flag(settings, "pause_on_talk", true);
        let log = log_path(&self.cwd);
        watching.store(false, Ordering::Relaxed);
        if want_watch {
            if let Some(log) = &log {
                let h = talk.spawn(ctx.soloist.clone(), log.clone());
                *self.watcher.lock().unwrap() = Some(h);
                watching.store(true, Ordering::Relaxed);
            }
        }
        if announce {
            let keys_ok = ctx.keys.can_search();
            let log_missing = want_watch && log.is_none();
            let soloist = ctx.soloist.clone();
            // Опрос Soloist не должен задерживать init: сообщение придёт, когда он ответит.
            tokio::spawn(async move {
                let down = soloist.probe().await == Probe::Down;
                if down {
                    eprintln!("spotify-плагин: Soloist не отвечает");
                }
                if !keys_ok {
                    eprintln!("spotify-плагин: нет ключей");
                } else if log_missing {
                    eprintln!("spotify-плагин: журнал не найден, пауза на разговор выключена");
                } else if !down {
                    eprintln!("spotify-плагин: готов");
                }
            });
        }
    }
}

impl Plugin for SpotifyPlugin {
    fn request(&self, method: &str, params: Value) -> Response<'_> {
        let method = method.to_string();
        let sh = self.shared();
        Box::pin(async move {
            match method.as_str() {
                "tool" => {
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                    Ok(Value::String(sh.tool(name, &args).await))
                }
                "command" => {
                    let text = params.as_str().unwrap_or("");
                    let r = tokio::time::timeout(COMMAND_BUDGET, sh.command(text)).await.ok().flatten();
                    Ok(r.map_or(Value::Null, Value::String))
                }
                _ => Ok(Value::Null),
            }
        })
    }

    fn tools(&self) -> Vec<Value> {
        tools()
    }

    fn configure(&self, settings: Map<String, Value>) {
        self.apply(&settings, false);
    }

    fn stop(&self) {
        if let Some(h) = self.watcher.lock().unwrap().take() {
            h.abort();
        }
    }
}
