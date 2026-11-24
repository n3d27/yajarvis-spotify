use jarvis_plugin_sdk::{json, process::{read_frame, write_frame}};
use std::process::Stdio;
use tokio::process::Command;

#[tokio::test(flavor = "current_thread")]
async fn init_then_command_then_shutdown() {
    let dir = std::env::temp_dir().join(format!("spotify-plugin-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_spotify-plugin"))
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    write_frame(&mut stdin, &json!({"method":"init","params":{"api":1,"info":{},"settings":{}}})).await.unwrap();
    let reply = read_frame(&mut stdout).await.unwrap();
    assert_eq!(reply["result"]["api"], 1);
    assert!(reply["result"]["tools"].is_array());

    write_frame(&mut stdin, &json!({"method":"command","params":"привет"})).await.unwrap();
    assert_eq!(read_frame(&mut stdout).await.unwrap(), json!({"result":null}));

    write_frame(&mut stdin, &json!({"method":"shutdown","params":null})).await.unwrap();
    let status = child.wait().await.unwrap();
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

mod harness {
    use super::*;
    use jarvis_plugin_sdk::Value;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    pub struct Plug {
        pub root: PathBuf,
        pub solo: PathBuf,
        child: tokio::process::Child,
        stdin: tokio::process::ChildStdin,
        stdout: tokio::process::ChildStdout,
        pub tools: Value,
    }

    /// Заглушка Soloist: control.sh пишет аргументы в `log`, `soloist ctl … now --json` печатает `now.json`.
    fn stub(solo: &Path) {
        std::fs::create_dir_all(solo.join("scripts")).unwrap();
        std::fs::create_dir_all(solo.join("bin")).unwrap();
        let dir = solo.display();
        std::fs::write(solo.join("now.json"), "{}").unwrap();
        std::fs::write(solo.join("scripts/control.sh"), format!("echo \"$*\" >> '{dir}/log'\n")).unwrap();
        std::fs::write(solo.join("bin/soloist"), format!("#!/bin/sh\ncat '{dir}/now.json'\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(solo.join("bin/soloist"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub async fn start(tag: &str, soloist_dir: Option<&str>) -> Plug {
        start_with(tag, soloist_dir, json!({})).await
    }

    /// `extra` дополняет настройки; `pause_on_talk: true` включает наблюдатель за журналом
    /// (`<root>/jarvisd.log`, путь запасного варианта ядра).
    pub async fn start_with(tag: &str, soloist_dir: Option<&str>, extra: Value) -> Plug {
        let root = std::env::temp_dir().join(format!("spotify-plugin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("plugins/spotify/data");
        std::fs::create_dir_all(&data).unwrap();
        let solo = root.join("solo");
        stub(&solo);
        let sdir = soloist_dir.map(str::to_string).unwrap_or_else(|| solo.display().to_string());
        let mut child = Command::new(env!("CARGO_BIN_EXE_spotify-plugin"))
            .current_dir(&data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        std::fs::write(root.join("jarvisd.log"), "").unwrap();
        let mut settings = json!({"soloist_dir": sdir, "pause_on_talk": false, "liked_uri": "spotify:user:fake:collection"});
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        write_frame(&mut stdin, &json!({"method":"init","params":{"api":1,"info":{},"settings":settings}})).await.unwrap();
        let reply = read_frame(&mut stdout).await.unwrap();
        let tools = reply["result"]["tools"].clone();
        Plug { root, solo, child, stdin, stdout, tools }
    }

    impl Plug {
        pub async fn call(&mut self, method: &str, params: Value) -> Value {
            write_frame(&mut self.stdin, &json!({"method":method,"params":params})).await.unwrap();
            read_frame(&mut self.stdout).await.unwrap()["result"].clone()
        }
        pub async fn tool(&mut self, name: &str, args: Value) -> String {
            self.call("tool", json!({"name":name,"arguments":args})).await.as_str().unwrap().to_string()
        }
        /// Дописать строку в журнал ядра с временем, как делает ядро.
        pub fn core_log(&self, msg: &str) {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(self.root.join("jarvisd.log")).unwrap();
            writeln!(f, "2026/11/24 20:15:03 {msg}").unwrap();
        }
        pub fn log(&self) -> Vec<String> {
            std::fs::read_to_string(self.solo.join("log")).unwrap_or_default().lines().map(String::from).collect()
        }
        pub async fn wait_log(&self, n: usize) -> Vec<String> {
            let t = Instant::now();
            while self.log().len() < n && t.elapsed().as_secs() < 5 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            self.log()
        }
        pub async fn finish(mut self) {
            write_frame(&mut self.stdin, &json!({"method":"shutdown","params":null})).await.unwrap();
            let _ = self.child.wait().await;
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn tools_are_listed_with_exact_names() {
    let p = harness::start("tools", None).await;
    let names: Vec<&str> = p.tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["spotify_play", "spotify_control"]);
    for t in p.tools.as_array().unwrap() {
        assert_eq!(t["type"], "function");
        assert!(t["parameters"].is_object());
    }
    assert!(p.tools[0]["description"].as_str().unwrap().starts_with("Spotify — основной источник музыки."));
    assert!(p.tools[1]["description"].as_str().unwrap().contains("всегда предыдущий трек"));
    assert_eq!(p.tools[0]["parameters"]["required"], json!(["kind"]));
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn command_unknown_is_null_fast() {
    let mut p = harness::start("cmdnull", None).await;
    let t = std::time::Instant::now();
    assert_eq!(p.call("command", json!("какая погода")).await, json!(null));
    assert!(t.elapsed().as_millis() < 200);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn command_like_needs_track() {
    let mut p = harness::start("cmdlike", None).await;
    assert_eq!(p.call("command", json!("лайк")).await, json!(null));
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn command_liked_answers_at_once_and_plays_in_background() {
    let mut p = harness::start("cmdliked", None).await;
    let t = std::time::Instant::now();
    assert_eq!(p.call("command", json!("мое любимое")).await, json!("Включил любимое в Spotify."));
    assert!(t.elapsed().as_millis() < 1500);
    let log = p.wait_log(2).await;
    assert_eq!(log, ["shuffle off", "play spotify:user:fake:collection"]);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn play_liked_turns_shuffle_off() {
    let mut p = harness::start("liked", None).await;
    let r = p.tool("spotify_play", json!({"kind":"liked"})).await;
    assert_eq!(r, "Включил любимое в Spotify.");
    assert_eq!(p.log(), ["shuffle off", "play spotify:user:fake:collection"]);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn play_liked_shuffle_turns_shuffle_on() {
    let mut p = harness::start("likedsh", None).await;
    let r = p.tool("spotify_play", json!({"kind":"liked_shuffle"})).await;
    assert_eq!(r, "Включил любимое вперемешку в Spotify.");
    assert_eq!(p.log(), ["shuffle on", "play spotify:user:fake:collection"]);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn play_without_keys_says_so() {
    let mut p = harness::start("nokeys", None).await;
    let r = p.tool("spotify_play", json!({"kind":"track","query":"что угодно"})).await;
    assert_eq!(r, "Spotify не ответил: нет ключей.");
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn control_prev_reaches_previous() {
    let mut p = harness::start("prev", None).await;
    p.tool("spotify_control", json!({"command":"prev"})).await;
    assert_eq!(p.log(), ["prev"]);
    p.finish().await;
}

const NO_KEYS_TAIL: &str = " Ключей Spotify нет — поиск и лайки недоступны.";
const NO_WATCH_TAIL: &str = " Пауза на время разговора выключена.";

#[tokio::test(flavor = "current_thread")]
async fn control_status_reads_now() {
    let mut p = harness::start("status", None).await;
    std::fs::write(p.solo.join("now.json"), include_str!("fixtures/now_playing.json")).unwrap();
    assert_eq!(
        p.tool("spotify_control", json!({"command":"status"})).await,
        format!("На паузе «ONLY POSITIVE» — Endspiel, TumaniYO.{NO_KEYS_TAIL}{NO_WATCH_TAIL}")
    );
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn status_healthy_has_no_suffixes() {
    let mut p = harness::start_with("statusok", None, json!({"client_id":"a","client_secret":"b","pause_on_talk":true})).await;
    std::fs::write(p.solo.join("now.json"), include_str!("fixtures/now_playing.json")).unwrap();
    assert_eq!(p.tool("spotify_control", json!({"command":"status"})).await, "На паузе «ONLY POSITIVE» — Endspiel, TumaniYO.");
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn status_nothing_playing() {
    let mut p = harness::start_with("statusidle", None, json!({"client_id":"a","client_secret":"b","pause_on_talk":true})).await;
    std::fs::write(p.solo.join("now.json"), "{}").unwrap();
    assert_eq!(p.tool("spotify_control", json!({"command":"status"})).await, "Spotify сейчас ничего не играет.");
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn status_soloist_down() {
    let mut p = harness::start_with("statusdown", Some("/nonexistent"), json!({"client_id":"a","client_secret":"b","pause_on_talk":true})).await;
    assert_eq!(
        p.tool("spotify_control", json!({"command":"status"})).await,
        "Spotify не отвечает: Soloist не запущен или не установлен."
    );
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn status_reports_missing_keys_and_watcher_off() {
    let mut p = harness::start_with("statusflags", None, json!({"client_id":"a","client_secret":"b"})).await;
    std::fs::write(p.solo.join("now.json"), "{}").unwrap();
    assert_eq!(p.tool("spotify_control", json!({"command":"status"})).await, format!("Spotify сейчас ничего не играет.{NO_WATCH_TAIL}"));
    p.finish().await;
    let mut p = harness::start_with("statusflags2", None, json!({"pause_on_talk":true})).await;
    std::fs::write(p.solo.join("now.json"), "{}").unwrap();
    assert_eq!(p.tool("spotify_control", json!({"command":"status"})).await, format!("Spotify сейчас ничего не играет.{NO_KEYS_TAIL}"));
    p.finish().await;
}

fn playing_now() -> String {
    include_str!("fixtures/now_playing.json").replacen("\"status\":\"paused\"", "\"status\":\"playing\"", 1)
}

#[tokio::test(flavor = "current_thread")]
async fn no_soloist_answers_error_within_timeout() {
    let mut p = harness::start("nosolo", Some("/nonexistent")).await;
    let t = std::time::Instant::now();
    let r = p.tool("spotify_control", json!({"command":"status"})).await;
    assert!(r.starts_with("Spotify не отвечает"), "{r}");
    let r2 = p.tool("spotify_control", json!({"command":"pause"})).await;
    assert!(r2.starts_with("Spotify"), "{r2}");
    assert!(t.elapsed().as_secs() < 9);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn resume_during_talk_waits_for_the_end() {
    let mut p = harness::start_with("resume", None, json!({"pause_on_talk": true})).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    p.core_log("Джарвис: активация (слово, уверенность 1.00)");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let r = p.tool("spotify_control", json!({"command":"resume"})).await;
    assert_eq!(r, "Продолжаю. Включу после ответа.");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(p.log().is_empty(), "{:?}", p.log());
    p.core_log("Джарвис: ложное срабатывание (никто ничего не сказал) — молчу");
    assert_eq!(p.wait_log(1).await, ["play"]);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn next_during_talk_makes_no_sound_before_the_end() {
    let mut p = harness::start_with("nexttalk", None, json!({"pause_on_talk": true})).await;
    std::fs::write(p.solo.join("now.json"), playing_now()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    p.core_log("Джарвис: активация (слово, уверенность 1.00)");
    assert_eq!(p.wait_log(1).await, ["pause"]);
    let r = p.tool("spotify_control", json!({"command":"next"})).await;
    assert_eq!(r, "Следующий трек. Включу после ответа.");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(p.log(), ["pause"]);
    p.core_log("Джарвис: «включи свет» → Алисе (голос, 0.30 с)");
    assert_eq!(p.wait_log(3).await, ["pause", "next", "play"]);
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_fast_command_plays_after_route_end() {
    let mut p = harness::start_with("deferfast", None, json!({"pause_on_talk": true})).await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    p.core_log("Джарвис: активация (слово, уверенность 1.00)");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(p.call("command", json!("мое любимое")).await, json!("Включил любимое в Spotify."));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(p.log().is_empty(), "{:?}", p.log());
    p.core_log("Джарвис: «мое любимое» → плагин (голос)");
    p.core_log("Джарвис: ответ «Включил любимое в Spotify.»");
    assert_eq!(p.wait_log(2).await, ["shuffle off", "play spotify:user:fake:collection"]);
    p.finish().await;
}

const LOGGED_OUT: &str = "Spotify на колонке не в сети: откройте Spotify на телефоне и выберите эту колонку в списке устройств — один раз, дальше колонка войдёт сама.";

/// Заглушка вышедшего из Spotify Soloist: `Error: not logged in`, код 3.
fn logged_out(solo: &std::path::Path) {
    std::fs::write(solo.join("bin/soloist"), "#!/bin/sh\necho 'Error: not logged in' >&2\nexit 3\n").unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn logged_out_soloist_is_reported_not_played() {
    let mut p = harness::start("loggedout", None).await;
    logged_out(&p.solo);
    assert_eq!(p.tool("spotify_play", json!({"kind":"liked"})).await, LOGGED_OUT);
    assert_eq!(p.tool("spotify_control", json!({"command":"status"})).await, format!("{LOGGED_OUT}{NO_KEYS_TAIL}{NO_WATCH_TAIL}"));
    assert_eq!(p.tool("spotify_control", json!({"command":"pause"})).await, LOGGED_OUT);
    assert_eq!(p.call("command", json!("мое любимое")).await, json!(LOGGED_OUT));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(p.log().is_empty(), "{:?}", p.log());
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn logged_out_names_the_device_from_settings() {
    let mut p = harness::start("loggedoutname", None).await;
    logged_out(&p.solo);
    std::fs::create_dir_all(p.solo.join("data")).unwrap();
    std::fs::write(p.solo.join("data/settings"), "{\"device_name\":\"Кухня\"}").unwrap();
    let r = p.tool("spotify_play", json!({"kind":"liked"})).await;
    assert!(r.contains("выберите устройство «Кухня» — один раз"), "{r}");
    p.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn logged_out_midtalk_is_not_deferred() {
    let mut p = harness::start_with("loggedouttalk", None, json!({"pause_on_talk": true})).await;
    logged_out(&p.solo);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    p.core_log("Джарвис: активация (слово, уверенность 1.00)");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(p.tool("spotify_play", json!({"kind":"liked"})).await, LOGGED_OUT);
    p.core_log("Джарвис: ложное срабатывание (никто ничего не сказал) — молчу");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(p.log().is_empty(), "{:?}", p.log());
    p.finish().await;
}

#[test]
fn settings_json_is_valid_manifest() {
    let m: serde_json::Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
    for (k, v) in [("id", "spotify"), ("name", "Spotify"), ("kind", "rust"), ("arch", "arm64"), ("category", "media"),
        ("icon", "music"), ("main", "spotify-plugin"), ("min_core", "0.10.16"), ("homepage", "https://github.com/n3d27/yajarvis-spotify")] {
        assert_eq!(m[k], v, "{k}");
    }
    assert_eq!(m["api"], 1);
    assert_eq!(m["provides"], serde_json::json!(["tools", "commands"]));
    let keys: Vec<&str> = m["settings"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["client_id", "client_secret", "refresh_token", "liked_uri", "soloist_dir", "pause_on_talk"]);
    let types: Vec<&str> = m["settings"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["text", "password", "password", "text", "text", "bool"]);
    assert_eq!(m["settings"][4]["default"], "/data/opt/soloist");
    assert_eq!(m["settings"][5]["default"], true);
}
