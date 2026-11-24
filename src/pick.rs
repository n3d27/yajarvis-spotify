//! Сверка запроса с результатами поиска Spotify (порт из форка).

use serde_json::Value;

const ACCEPT_SCORE: f32 = 0.6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Any,
    Track,
    Artist,
    Album,
    Playlist,
    Podcast,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    pub uri: String,
    pub label: String,
}

const STOP_WORDS: &[&str] = &[
    "песню", "песня", "трек", "альбом", "группы", "группу", "исполнителя", "плейлист", "подкаст", "выпуск", "эпизод", "the",
];

fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars().flat_map(|c| c.to_lowercase()) {
        let ch = if ch == 'ё' { 'е' } else { ch };
        out.push(if ch.is_alphanumeric() { ch } else { ' ' });
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Кириллица → латиница: «Кино» в запросе должно совпасть с артистом Kino.
fn translit(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let t = match ch {
            'а' => "a", 'б' => "b", 'в' => "v", 'г' => "g", 'д' => "d", 'е' => "e", 'ж' => "zh", 'з' => "z",
            'и' => "i", 'й' => "y", 'к' => "k", 'л' => "l", 'м' => "m", 'н' => "n", 'о' => "o", 'п' => "p",
            'р' => "r", 'с' => "s", 'т' => "t", 'у' => "u", 'ф' => "f", 'х' => "h", 'ц' => "ts", 'ч' => "ch",
            'ш' => "sh", 'щ' => "sch", 'ъ' | 'ь' => "", 'ы' => "y", 'э' => "e", 'ю' => "yu", 'я' => "ya",
            _ => {
                out.push(ch);
                continue;
            }
        };
        out.push_str(t);
    }
    out
}

fn query_words(query: &str) -> Vec<String> {
    norm(query).split(' ').filter(|w| !w.is_empty() && !STOP_WORDS.contains(w)).map(str::to_string).collect()
}

fn word_hit(word: &str, hay: &str) -> bool {
    if word.chars().count() >= 3 {
        hay.contains(word)
    } else {
        hay.split(' ').any(|t| t == word)
    }
}

fn score(words: &[String], hay: &str) -> f32 {
    if words.is_empty() {
        return 0.0;
    }
    let plain = norm(hay);
    let plain_hits = words.iter().filter(|w| word_hit(w, &plain)).count();
    let tr_hay = translit(&plain);
    let tr_hits = words.iter().filter(|w| word_hit(&translit(w), &tr_hay)).count();
    plain_hits.max(tr_hits) as f32 / words.len() as f32
}

fn s<'a>(v: &'a Value, path: &str) -> &'a str {
    v.pointer(path).and_then(Value::as_str).unwrap_or("")
}

fn artist_names(v: &Value) -> String {
    v.get("artists")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

struct Cand {
    uri: String,
    label: String,
    hay: String,
    is_artist: bool,
    name: String,
}

fn candidates(kind: Kind, search: &Value) -> Vec<Cand> {
    let sections: &[&str] = match kind {
        Kind::Any => &["tracks", "artists", "albums", "playlists"],
        Kind::Track => &["tracks"],
        Kind::Artist => &["artists"],
        Kind::Album => &["albums"],
        Kind::Playlist => &["playlists"],
        Kind::Podcast => &["shows", "episodes"],
    };
    let mut out = Vec::new();
    for sec in sections {
        let Some(items) = search.pointer(&format!("/{sec}/items")).and_then(Value::as_array) else { continue };
        for it in items {
            if it.is_null() {
                continue;
            }
            let uri = s(it, "/uri");
            let name = s(it, "/name");
            if uri.is_empty() || name.is_empty() {
                continue;
            }
            let (label, hay, is_artist) = match *sec {
                "tracks" | "albums" => {
                    let a = artist_names(it);
                    let label = if a.is_empty() { name.to_string() } else { format!("{a} — {name}") };
                    (label, format!("{name} {a}"), false)
                }
                "artists" => (name.to_string(), name.to_string(), true),
                "playlists" => (name.to_string(), format!("{name} {}", s(it, "/owner/display_name")), false),
                "shows" => (name.to_string(), format!("{name} {}", s(it, "/publisher")), false),
                _ => (name.to_string(), format!("{name} {} {}", s(it, "/show/name"), s(it, "/show/publisher")), false),
            };
            out.push(Cand { uri: uri.to_string(), label, hay, is_artist, name: name.to_string() });
        }
    }
    out
}

/// Сверка: берём первый результат Spotify, в котором узнаётся запрос (≥ 0.6 слов).
pub fn pick(query: &str, kind: Kind, search: &Value) -> Option<Pick> {
    let words = query_words(query);
    if words.is_empty() {
        return None;
    }
    let cands = candidates(kind, search);
    let q_full = words.join(" ");
    if kind == Kind::Any {
        let q_tr = translit(&q_full);
        let exact = cands.iter().find(|c| {
            c.is_artist && {
                let n = norm(&c.name);
                n == q_full || translit(&n) == q_tr
            }
        });
        if let Some(c) = exact {
            return Some(Pick { uri: c.uri.clone(), label: c.label.clone() });
        }
    }
    cands
        .iter()
        .find(|c| score(&words, &c.hay) >= ACCEPT_SCORE)
        .map(|c| Pick { uri: c.uri.clone(), label: c.label.clone() })
}

#[cfg(test)]
fn fixture(name: &str) -> Value {
    let text = match name {
        "search_kino.json" => include_str!("../tests/fixtures/search_kino.json"),
        "search_obzor.json" => include_str!("../tests/fixtures/search_obzor.json"),
        "search_podcast.json" => include_str!("../tests/fixtures/search_podcast.json"),
        _ => panic!("unknown fixture {name}"),
    };
    serde_json::from_str(text).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn pick_artist_for_band_name() { let v = fixture("search_kino.json"); assert!(pick("кино", Kind::Any, &v).unwrap().uri.starts_with("spotify:artist:")); }
    #[test] fn pick_rejects_video_like_query() { let v = fixture("search_obzor.json"); assert!(pick("обзор айфона 17", Kind::Any, &v).is_none()); }
    #[test] fn pick_podcast_show() { let v = fixture("search_podcast.json"); assert!(pick("Lex Fridman Podcast", Kind::Podcast, &v).unwrap().uri.starts_with("spotify:show:")); }
    #[test] fn pick_skips_nulls() { let v = serde_json::json!({"tracks":{"items":[null]}}); assert!(pick("что угодно", Kind::Any, &v).is_none()); }
}
