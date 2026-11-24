//! Ключи Spotify: настройки плагина, затем `spotify_api.conf` Soloist; ротация refresh-токена.
//! Значения ключей нигде не печатаются.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Default, PartialEq)]
pub struct Keys {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub liked_uri: String,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(..)")
    }
}

/// Разбор `KEY=value`; значения могут быть в кавычках.
pub fn parse_conf(text: &str) -> Keys {
    let mut c = Keys::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let v = if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
        {
            &v[1..v.len() - 1]
        } else {
            v
        };
        match k.trim() {
            "SPOTIFY_CLIENT_ID" => c.client_id = v.to_string(),
            "SPOTIFY_CLIENT_SECRET" => c.client_secret = v.to_string(),
            "SPOTIFY_REFRESH_TOKEN" => c.refresh_token = v.to_string(),
            "SPOTIFY_LIKED_URI" => c.liked_uri = v.to_string(),
            _ => {}
        }
    }
    c
}

impl Keys {
    /// Настройки плагина важнее конфига; каждый ключ подставляется независимо.
    pub fn resolve(settings: &Map<String, Value>, soloist_dir: &Path) -> Keys {
        let conf = std::fs::read_to_string(soloist_dir.join("spotify_api.conf"))
            .map(|t| parse_conf(&t))
            .unwrap_or_default();
        let pick = |name: &str, fallback: String| -> String {
            match settings.get(name).and_then(Value::as_str).map(str::trim) {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => fallback,
            }
        };
        Keys {
            client_id: pick("client_id", conf.client_id),
            client_secret: pick("client_secret", conf.client_secret),
            refresh_token: pick("refresh_token", conf.refresh_token),
            liked_uri: pick("liked_uri", conf.liked_uri),
        }
    }

    pub fn can_search(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }

    pub fn has_user(&self) -> bool {
        self.can_search() && !self.refresh_token.is_empty()
    }
}

fn digest(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Токен для работы: ротированный, пока токен в настройках не менялся; иначе из настроек.
pub fn effective_refresh(settings_token: &str, store: &Path) -> String {
    let rotated = std::fs::read_to_string(store).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| {
        let from = v.get("from_settings")?.as_str()?;
        let rt = v.get("refresh_token")?.as_str()?;
        (from == digest(settings_token) && !rt.is_empty()).then(|| rt.to_string())
    });
    rotated.unwrap_or_else(|| settings_token.to_string())
}

/// Сохраняет новый refresh-токен рядом с хешем токена из настроек (файл 0600).
pub fn save_rotated(settings_token: &str, new: &str, store: &Path) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let body = serde_json::json!({"from_settings": digest(settings_token), "refresh_token": new}).to_string();
    if let Some(dir) = store.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = store.with_extension("tmp");
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, store));
    if ok.is_err() {
        eprintln!("spotify-плагин: не удалось сохранить токен");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("spk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn keys_settings_win_conf_fills_gaps() {
        let d = tmp("gaps");
        std::fs::write(
            d.join("spotify_api.conf"),
            "SPOTIFY_CLIENT_ID=conf-id\nSPOTIFY_CLIENT_SECRET=\"conf-secret\"\nSPOTIFY_REFRESH_TOKEN='conf-rt'\nSPOTIFY_LIKED_URI=spotify:user:testuser:collection\n",
        )
        .unwrap();
        let mut s = Map::new();
        s.insert("client_id".into(), "set-id".into());
        s.insert("client_secret".into(), "".into());
        let k = Keys::resolve(&s, &d);
        assert_eq!(k.client_id, "set-id");
        assert_eq!(k.client_secret, "conf-secret");
        assert_eq!(k.refresh_token, "conf-rt");
        assert_eq!(k.liked_uri, "spotify:user:testuser:collection");
        assert!(k.can_search() && k.has_user());
    }

    #[test]
    fn keys_none_anywhere() {
        let d = tmp("none");
        let k = Keys::resolve(&Map::new(), &d);
        assert!(!k.can_search());
        assert!(!k.has_user());
    }

    #[test]
    fn rotation_used_until_settings_change() {
        let d = tmp("rot");
        let store = d.join("token.json");
        assert_eq!(effective_refresh("A", &store), "A");
        save_rotated("A", "B", &store);
        assert_eq!(effective_refresh("A", &store), "B");
        assert_eq!(effective_refresh("C", &store), "C");
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(!text.contains("\"A\""));
    }
}
