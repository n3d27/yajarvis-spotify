//! Spotify Web API: поиск, лайки, «любимое» и «волна». HTTP тонкий и блокирующий (ureq, rustls)
//! в `spawn_blocking`; чистые части (URL, разбор ответов) тестируются отдельно.

use crate::keys::{self, Keys};
use crate::pick::{Kind, Pick, pick};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ACCOUNTS: &str = "https://accounts.spotify.com";
const API: &str = "https://api.spotify.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Базовые адреса; подменяются в тестах.
#[derive(Clone)]
pub struct Base {
    pub accounts: String,
    pub api: String,
}

impl Default for Base {
    fn default() -> Self {
        Base { accounts: ACCOUNTS.into(), api: API.into() }
    }
}

static APP_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
static USER_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Сбросить кэши токенов: ключи сменились, чужой токен использовать нельзя.
pub fn reset_tokens() {
    *APP_TOKEN.lock().unwrap() = None;
    *USER_TOKEN.lock().unwrap() = None;
}

pub fn search_types(kind: Kind) -> &'static str {
    match kind {
        Kind::Any => "track,artist,album,playlist",
        Kind::Track => "track",
        Kind::Artist => "artist",
        Kind::Album => "album",
        Kind::Playlist => "playlist",
        Kind::Podcast => "show,episode",
    }
}

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub fn search_url(base: &str, query: &str, kind: Kind) -> String {
    format!("{base}/v1/search?q={}&type={}&limit=5", enc(query), enc(search_types(kind)))
}

pub fn library_url(base: &str, uri: &str) -> String {
    format!("{base}/v1/me/library?uris={}", enc(uri))
}

pub fn tracks_url(base: &str, limit: u32, offset: u64) -> String {
    format!("{base}/v1/me/tracks?limit={limit}&offset={offset}")
}

pub fn basic_auth(id: &str, secret: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let data = format!("{id}:{secret}").into_bytes();
    let mut o = String::from("Basic ");
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

pub struct TokenReply {
    pub access: String,
    pub ttl: u64,
    pub rotated: Option<String>,
}

pub fn parse_token(v: &Value) -> Result<TokenReply, String> {
    let access = v.get("access_token").and_then(Value::as_str).ok_or("spotify token: нет access_token")?.to_string();
    let ttl = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).saturating_sub(60).max(30);
    let rotated = v.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(TokenReply { access, ttl, rotated })
}

pub fn parse_me(v: &Value) -> Option<String> {
    let id = v.get("id")?.as_str().filter(|s| !s.is_empty())?;
    Some(format!("spotify:user:{id}:collection"))
}

pub fn parse_total(v: &Value) -> u64 {
    v.get("total").and_then(Value::as_u64).unwrap_or(0)
}

pub fn parse_track_id(v: &Value) -> Option<String> {
    let uri = v.pointer("/items/0/track/uri")?.as_str()?;
    let id = uri.strip_prefix("spotify:track:")?;
    (!id.is_empty()).then(|| id.to_string())
}

pub fn wave_from_id(id: &str) -> String {
    format!("spotify:station:track:{id}")
}

/// Ответ без секретов: код и тело (для JSON).
struct Reply {
    status: u16,
    body: String,
}

fn http(method: &str, url: &str, auth: &str, form: Option<&[(&str, &str)]>) -> Result<Reply, String> {
    let req = ureq::request(method, url).timeout(HTTP_TIMEOUT).set("Authorization", auth);
    let res = match form {
        Some(f) => req.send_form(f),
        None if method == "GET" => req.call(),
        None => req.set("Content-Length", "0").send_bytes(&[]),
    };
    match res {
        Ok(r) => {
            let status = r.status();
            Ok(Reply { status, body: r.into_string().unwrap_or_default() })
        }
        Err(ureq::Error::Status(status, r)) => Ok(Reply { status, body: r.into_string().unwrap_or_default() }),
        Err(_) => Err("нет связи со Spotify".into()),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|_| "внутренняя ошибка".to_string())
}

fn cached(slot: &Mutex<Option<(String, Instant)>>) -> Option<String> {
    slot.lock().unwrap().as_ref().filter(|(_, exp)| Instant::now() < *exp).map(|(t, _)| t.clone())
}

fn store(slot: &Mutex<Option<(String, Instant)>>, t: &str, ttl: u64) {
    *slot.lock().unwrap() = Some((t.to_string(), Instant::now() + Duration::from_secs(ttl)));
}

async fn app_token(keys: &Keys, base: &Base) -> Result<String, String> {
    if let Some(t) = cached(&APP_TOKEN) {
        return Ok(t);
    }
    let url = format!("{}/api/token", base.accounts);
    let auth = basic_auth(&keys.client_id, &keys.client_secret);
    let r = blocking(move || http("POST", &url, &auth, Some(&[("grant_type", "client_credentials")]))).await??;
    if !(200..300).contains(&r.status) {
        return Err(format!("spotify token: HTTP {}", r.status));
    }
    let v: Value = serde_json::from_str(&r.body).map_err(|_| "spotify token: ответ не разобрать".to_string())?;
    let t = parse_token(&v)?;
    store(&APP_TOKEN, &t.access, t.ttl);
    Ok(t.access)
}

/// Какие refresh-токены пробовать: сначала действующий (возможно ротированный), затем токен из настроек.
pub fn refresh_candidates(configured: &str, effective: &str) -> Vec<String> {
    let mut v = vec![effective.to_string()];
    if effective != configured {
        v.push(configured.to_string());
    }
    v
}

/// `invalid_grant` в теле ответа токен-эндпойнта: токен отозван или устарел.
pub fn is_invalid_grant(status: u16, body: &str) -> bool {
    status == 400 && serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("error").and_then(Value::as_str).map(|e| e == "invalid_grant")).unwrap_or(false)
}

static USER_REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn user_token(keys: &Keys, base: &Base, store_path: &Path) -> Result<String, String> {
    if !keys.has_user() {
        return Err("нет refresh-токена".into());
    }
    if let Some(t) = cached(&USER_TOKEN) {
        return Ok(t);
    }
    // Один обмен за раз; второй вызов найдёт токен в кэше.
    let _guard = USER_REFRESH.lock().await;
    if let Some(t) = cached(&USER_TOKEN) {
        return Ok(t);
    }
    let auth = basic_auth(&keys.client_id, &keys.client_secret);
    let effective = keys::effective_refresh(&keys.refresh_token, store_path);
    let mut last = String::new();
    for rt in refresh_candidates(&keys.refresh_token, &effective) {
        let (url, auth) = (format!("{}/api/token", base.accounts), auth.clone());
        let r = blocking(move || {
            http("POST", &url, &auth, Some(&[("grant_type", "refresh_token"), ("refresh_token", &rt)]))
        })
        .await??;
        if (200..300).contains(&r.status) {
            let v: Value = serde_json::from_str(&r.body).map_err(|_| "spotify token: ответ не разобрать".to_string())?;
            let t = parse_token(&v)?;
            if let Some(new) = &t.rotated {
                keys::save_rotated(&keys.refresh_token, new, store_path);
            }
            store(&USER_TOKEN, &t.access, t.ttl);
            return Ok(t.access);
        }
        last = format!("spotify token: HTTP {}", r.status);
        if !is_invalid_grant(r.status, &r.body) {
            break;
        }
    }
    Err(last)
}

pub async fn search(keys: &Keys, query: &str, kind: Kind) -> Result<Option<Pick>, String> {
    search_at(&Base::default(), keys, query, kind).await
}

pub async fn search_at(base: &Base, keys: &Keys, query: &str, kind: Kind) -> Result<Option<Pick>, String> {
    if !keys.can_search() {
        return Err("нет ключей".into());
    }
    let url = search_url(&base.api, query, kind);
    for attempt in 0..2 {
        let tok = app_token(keys, base).await?;
        let (u, auth) = (url.clone(), format!("Bearer {tok}"));
        let r = blocking(move || http("GET", &u, &auth, None)).await??;
        if r.status == 401 && attempt == 0 {
            *APP_TOKEN.lock().unwrap() = None;
            continue;
        }
        if !(200..300).contains(&r.status) {
            return Err(format!("spotify search: HTTP {}", r.status));
        }
        let v: Value = serde_json::from_str(&r.body).map_err(|_| "spotify search: ответ не разобрать".to_string())?;
        return Ok(pick(query, kind, &v));
    }
    Err("spotify search: HTTP 401".into())
}

/// Запрос с пользовательским токеном и одним повтором на 401.
async fn user_call(keys: &Keys, base: &Base, store_path: &Path, method: &'static str, url: String) -> Result<Reply, String> {
    for attempt in 0..2 {
        let tok = user_token(keys, base, store_path).await?;
        let (u, auth) = (url.clone(), format!("Bearer {tok}"));
        let r = blocking(move || http(method, &u, &auth, None)).await??;
        if r.status == 401 && attempt == 0 {
            *USER_TOKEN.lock().unwrap() = None;
            continue;
        }
        return Ok(r);
    }
    Err("spotify: HTTP 401".into())
}

pub async fn like(keys: &Keys, uri: &str, on: bool, store_path: &Path) -> Result<(), String> {
    let base = Base::default();
    let r = user_call(keys, &base, store_path, if on { "PUT" } else { "DELETE" }, library_url(&base.api, uri)).await?;
    if (200..300).contains(&r.status) { Ok(()) } else { Err(format!("spotify library: HTTP {}", r.status)) }
}

pub async fn liked_uri(keys: &Keys, store_path: &Path) -> Result<String, String> {
    if !keys.liked_uri.is_empty() {
        return Ok(keys.liked_uri.clone());
    }
    let base = Base::default();
    let r = user_call(keys, &base, store_path, "GET", format!("{}/v1/me", base.api)).await?;
    if !(200..300).contains(&r.status) {
        return Err(format!("spotify me: HTTP {}", r.status));
    }
    let v: Value = serde_json::from_str(&r.body).map_err(|_| "spotify me: ответ не разобрать".to_string())?;
    parse_me(&v).ok_or_else(|| "spotify me: нет id".to_string())
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    u64::from_le_bytes(b) ^ std::time::SystemTime::now().elapsed().map_or(0, |d| d.as_nanos() as u64)
}

pub async fn wave_uri(keys: &Keys, store_path: &Path) -> Result<String, String> {
    let base = Base::default();
    let r = user_call(keys, &base, store_path, "GET", tracks_url(&base.api, 1, 0)).await?;
    if !(200..300).contains(&r.status) {
        return Err(format!("spotify tracks: HTTP {}", r.status));
    }
    let v: Value = serde_json::from_str(&r.body).map_err(|_| "spotify tracks: ответ не разобрать".to_string())?;
    let total = parse_total(&v);
    if total == 0 {
        return Err("в любимом пусто".into());
    }
    let off = random_u64() % total;
    let r = user_call(keys, &base, store_path, "GET", tracks_url(&base.api, 1, off)).await?;
    if !(200..300).contains(&r.status) {
        return Err(format!("spotify tracks: HTTP {}", r.status));
    }
    let v: Value = serde_json::from_str(&r.body).map_err(|_| "spotify tracks: ответ не разобрать".to_string())?;
    parse_track_id(&v).map(|id| wave_from_id(&id)).ok_or_else(|| "spotify tracks: нет трека".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reset_tokens_clears_both_caches() {
        store(&APP_TOKEN, "app", 3600);
        store(&USER_TOKEN, "user", 3600);
        assert!(cached(&APP_TOKEN).is_some() && cached(&USER_TOKEN).is_some());
        reset_tokens();
        assert!(cached(&APP_TOKEN).is_none() && cached(&USER_TOKEN).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_without_keys_errors() {
        assert_eq!(search(&Keys::default(), "кино", Kind::Any).await, Err("нет ключей".to_string()));
    }

    #[test]
    fn urls_are_built() {
        assert_eq!(
            search_url("https://x", "a b&c", Kind::Podcast),
            "https://x/v1/search?q=a%20b%26c&type=show%2Cepisode&limit=5"
        );
        assert_eq!(
            library_url("https://x", "spotify:track:abc"),
            "https://x/v1/me/library?uris=spotify%3Atrack%3Aabc"
        );
        assert_eq!(tracks_url("https://x", 50, 7), "https://x/v1/me/tracks?limit=50&offset=7");
        assert!(!search_url("https://x", "q", Kind::Any).contains("market"));
    }

    #[test]
    fn basic_auth_encodes() {
        assert_eq!(basic_auth("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(basic_auth("a", "b"), "Basic YTpi");
    }

    #[test]
    fn token_parsed_with_rotation() {
        let t = parse_token(&json!({"access_token":"at","expires_in":3600,"refresh_token":"new"})).unwrap();
        assert_eq!((t.access.as_str(), t.ttl, t.rotated.as_deref()), ("at", 3540, Some("new")));
        let t = parse_token(&json!({"access_token":"at"})).unwrap();
        assert!(t.rotated.is_none());
        assert!(parse_token(&json!({"error":"invalid_grant"})).is_err());
    }

    #[test]
    fn refresh_falls_back_to_configured_token() {
        assert_eq!(refresh_candidates("A", "B"), vec!["B", "A"]);
        assert_eq!(refresh_candidates("A", "A"), vec!["A"]);
        assert!(is_invalid_grant(400, r#"{"error":"invalid_grant"}"#));
        assert!(!is_invalid_grant(400, r#"{"error":"invalid_client"}"#));
        assert!(!is_invalid_grant(500, ""));
    }

    #[test]
    fn me_and_wave_parsed() {
        assert_eq!(parse_me(&json!({"id":"testuser"})).unwrap(), "spotify:user:testuser:collection");
        assert!(parse_me(&json!({})).is_none());
        assert_eq!(parse_total(&json!({"total":12})), 12);
        let v = json!({"items":[{"track":{"uri":"spotify:track:T1"}}]});
        assert_eq!(wave_from_id(&parse_track_id(&v).unwrap()), "spotify:station:track:T1");
        assert!(parse_track_id(&json!({"items":[]})).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn liked_uri_from_keys_needs_no_network() {
        let k = Keys { liked_uri: "spotify:user:testuser:collection".into(), ..Keys::default() };
        assert_eq!(liked_uri(&k, Path::new("/nonexistent/token.json")).await.unwrap(), "spotify:user:testuser:collection");
    }
}
