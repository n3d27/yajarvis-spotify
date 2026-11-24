use spotify_core::soloist::{Op, SpotState};
use spotify_core::talk::{LogTail, Mark, Player, Talk, classify, is_activity};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn classify_markers() {
    let c = classify;
    assert_eq!(c("2026/11/22 23:33:19 Джарвис: активация (слово, уверенность 1.00)"), Some(Mark::Start));
    assert_eq!(c("2026/11/22 23:47:21 разговор: тишина 8 с — окончен"), Some(Mark::End));
    assert_eq!(c("2026/11/22 23:47:26 Джарвис: разговор прерван (позвали Алису)"), Some(Mark::End));
    assert_eq!(c("2026/11/22 23:35:32 разговор не открылся: gemini: …"), Some(Mark::End));
    assert_eq!(c("spotify-плагин: готов"), None);
    assert_eq!(c("Gemini: разговор готов за 1.05 с"), None);
}

const TS: &str = "2026/11/24 20:15:03 ";

fn line(msg: &str) -> String {
    format!("{TS}{msg}")
}

#[test]
fn classify_immediate_end_markers() {
    for m in [
        "Джарвис: ложное срабатывание (никто ничего не сказал) — молчу",
        "Джарвис: остановлен до начала разговора (позвали Алису)",
        "Джарвис: «стоп» — разговор закрыт, музыка не тронута (встроенный распознаватель)",
        "Джарвис: «пауза» — стоп (встроенный распознаватель)",
    ] {
        assert_eq!(classify(&line(m)), Some(Mark::End), "{m}");
    }
}

#[test]
fn classify_route_markers_end_after_answer() {
    for m in [
        "Джарвис: «лайк» → плагин (голос)",
        "Джарвис: «включи свет» → Алисе (голос, 0.27 с)",
        "Джарвис: «включи группу кино» → Алисе (модель, 0.27 с)",
        "Джарвис: «выключи лампу» → Smart Life (0.31 с, голос)",
        "Джарвис: быстрая команда «сколько времени» → Готово. (Яндекс 0.80 с, голос)",
    ] {
        assert_eq!(classify(&line(m)), Some(Mark::EndAfterAnswer), "{m}");
    }
}

#[test]
fn classify_youtube_autoclose_forgets_pause() {
    assert_eq!(classify(&line("разговор: музыка включена — закрываю разговор")), Some(Mark::EndForget));
}

#[test]
fn classify_does_not_match_look_alikes() {
    for m in [
        "инструмент: «включи группу кино» → Алисе (модель)",
        "инструмент плагина spotify_play → Включил в Spotify: Kino. Включу после ответа.",
        "Джарвис: ответ «ложное срабатывание — молчу»",
        "Джарвис: ответ «разговор: тишина 8 с — окончен»",
        "Gemini: «лайк» → плагин",
        "Джарвис: распознаватель услышал «хватит» — разговор закрыт",
        "Джарвис: «громкость» — громкость 50% (встроенный распознаватель)",
        "Джарвис: перебили словом — замолкаю",
    ] {
        assert_eq!(classify(&line(m)), None, "{m}");
        assert_eq!(classify(m), None, "{m}");
    }
    // строки плагина (без времени): ни одна не метка
    for m in [
        "spotify-плагин: готов",
        "spotify-плагин: Soloist не отвечает",
        "spotify-плагин: не увидел конец разговора",
        "spotify-плагин: нет ключей",
        "spotify-плагин: журнал не найден, пауза на разговор выключена",
        "spotify-плагин: состояние Spotify не получено, паузу не ставлю",
        "spotify-плагин: пауза на время разговора не удалась",
    ] {
        assert_eq!(classify(m), None, "{m}");
    }
    // подстрока посреди строки не считается
    assert_eq!(classify("spotify: Джарвис: ложное срабатывание"), None);
    assert_eq!(classify(&line("эхо Джарвис: активация")), None);
}

#[test]
fn only_conversation_lines_are_activity() {
    assert!(is_activity(&line("Gemini: разговор готов за 1.05 с")));
    assert!(is_activity(&line("Джарвис: ответ «привет»")));
    assert!(is_activity(&line("разговор: тишина 3 с")));
    assert!(is_activity(&line("инструмент плагина spotify_play → ok")));
    assert!(is_activity("Джарвис: активация"));
    assert!(!is_activity("spotify-плагин: готов"));
    assert!(!is_activity(&line("spotify: что-то")));
    assert!(!is_activity(&line("шум 1")));
}

#[test]
fn tail_survives_truncate_and_recreate() {
    let p = std::env::temp_dir().join(format!("talk-tail-{}.log", std::process::id()));
    std::fs::write(&p, "").unwrap();
    let mut t = LogTail::open(&p);
    std::fs::write(&p, "one\ntwo\n").unwrap();
    assert_eq!(t.poll(), vec!["one", "two"]);
    assert!(t.poll().is_empty());
    std::fs::write(&p, "three\n").unwrap(); // усечение
    assert_eq!(t.poll(), vec!["three"]);
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, "four\n").unwrap();
    assert_eq!(t.poll(), vec!["four"]);
    let _ = std::fs::remove_file(&p);
}

struct Stub {
    state: Mutex<SpotState>,
    ops: Mutex<Vec<Op>>,
}

impl Stub {
    fn new(status: &str) -> Arc<Stub> {
        Arc::new(Stub {
            state: Mutex::new(SpotState { status: status.into(), uri: "u1".into(), ..Default::default() }),
            ops: Mutex::new(vec![]),
        })
    }
    fn ops(&self) -> Vec<Op> {
        self.ops.lock().unwrap().clone()
    }
}

impl Player for Stub {
    fn now(&self) -> Pin<Box<dyn Future<Output = Option<SpotState>> + Send + '_>> {
        Box::pin(async move { Some(self.state.lock().unwrap().clone()) })
    }
    fn cmd(&self, op: Op) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            match op {
                Op::Pause => self.state.lock().unwrap().status = "paused".into(),
                Op::Resume => self.state.lock().unwrap().status = "playing".into(),
                _ => {}
            }
            self.ops.lock().unwrap().push(op);
            Ok(())
        })
    }
}

async fn until(f: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn pause_then_resume() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, s.clone());
    assert!(t.in_talk());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    t.handle(Mark::Start, s.clone()); // второй Start не перезаписывает паузу
    settle().await;
    t.handle(Mark::End, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause, Op::Resume]).await);
    assert!(!t.in_talk());
}

#[tokio::test]
async fn failsafe_ends_talk() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::new(Duration::from_millis(50)));
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause, Op::Resume]).await);
    assert!(!t.in_talk());
}

#[tokio::test]
async fn user_pause_mid_talk_blocks_resume() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    t.user_paused();
    t.handle(Mark::End, s.clone());
    settle().await;
    assert_eq!(s.ops(), vec![Op::Pause]);
}

#[tokio::test]
async fn deferred_play_runs_at_end() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    assert!(t.defer(Op::Play(Some("u2".into()))));
    t.handle(Mark::End, s.clone());
    assert!(until(|| s.ops().len() == 2).await);
    settle().await;
    assert_eq!(s.ops(), vec![Op::Pause, Op::Play(Some("u2".into()))]);
}

#[tokio::test]
async fn start_when_idle_still_defers() {
    let s = Stub::new("paused");
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, s.clone());
    settle().await;
    assert!(s.ops().is_empty());
    assert!(t.defer(Op::Play(None)));
    t.handle(Mark::End, s.clone());
    assert!(until(|| s.ops() == vec![Op::Play(None)]).await);
}

#[test]
fn tail_detects_larger_recreated_file() {
    let p = std::env::temp_dir().join(format!("talk-tail-big-{}.log", std::process::id()));
    std::fs::write(&p, "").unwrap();
    let mut t = LogTail::open(&p);
    std::fs::write(&p, "old one\n").unwrap();
    assert_eq!(t.poll(), vec!["old one"]);
    // пересоздан и уже длиннее прежней позиции; inode мог совпасть
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, "new aaaa\nnew bbbb\nnew cccc\n").unwrap();
    assert_eq!(t.poll(), vec!["new aaaa", "new bbbb", "new cccc"]);
    // усечён и снова вырос за один интервал опроса
    std::fs::write(&p, "fresh 1\nfresh 2\nfresh 3\nfresh 4\n").unwrap();
    assert_eq!(t.poll(), vec!["fresh 1", "fresh 2", "fresh 3", "fresh 4"]);
    // недописанная строка склеивается
    std::fs::write(&p, "fresh 1\nfresh 2\nfresh 3\nfresh 4\npart").unwrap();
    assert!(t.poll().is_empty());
    std::fs::write(&p, "fresh 1\nfresh 2\nfresh 3\nfresh 4\npartial\n").unwrap();
    assert_eq!(t.poll(), vec!["partial"]);
    let _ = std::fs::remove_file(&p);
}

struct FailFirstNow {
    inner: Arc<Stub>,
    fails: Mutex<u32>,
}

impl Player for FailFirstNow {
    fn now(&self) -> Pin<Box<dyn Future<Output = Option<SpotState>> + Send + '_>> {
        Box::pin(async move {
            {
                let mut f = self.fails.lock().unwrap();
                if *f > 0 {
                    *f -= 1;
                    return None;
                }
            }
            self.inner.now().await
        })
    }
    fn cmd(&self, op: Op) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.cmd(op)
    }
}

#[tokio::test]
async fn pause_retries_failed_now() {
    let s = Stub::new("playing");
    let p = Arc::new(FailFirstNow { inner: s.clone(), fails: Mutex::new(1) });
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, p);
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
}

#[tokio::test]
async fn failsafe_counts_from_last_log_line() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::new(Duration::from_millis(400)));
    let log = std::env::temp_dir().join(format!("talk-act-{}.log", std::process::id()));
    std::fs::write(&log, "").unwrap();
    t.clone().spawn_with(s.clone(), log.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut body = String::from("2026/11/22 23:33:19 Джарвис: активация (слово)\n");
    std::fs::write(&log, &body).unwrap();
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    // строки идут дольше страховки: возобновления нет
    for i in 0..60 {
        body.push_str(&format!("Gemini: шум {i}\n"));
        std::fs::write(&log, &body).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(s.ops(), vec![Op::Pause]);
    assert!(t.in_talk());
    // тишина в логе: страховка срабатывает
    assert!(until(|| s.ops() == vec![Op::Pause, Op::Resume]).await);
    let _ = std::fs::remove_file(&log);
}

/// Заглушка: после Play показывает перемешивание `after_play`; Shuffle(on) его выставляет.
struct ShuffleStub {
    inner: Arc<Stub>,
    after_play: bool,
}
impl Player for ShuffleStub {
    fn now(&self) -> Pin<Box<dyn Future<Output = Option<SpotState>> + Send + '_>> {
        self.inner.now()
    }
    fn cmd(&self, op: Op) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        match &op {
            Op::Play(_) => self.inner.state.lock().unwrap().shuffle = self.after_play,
            Op::Shuffle(on) => self.inner.state.lock().unwrap().shuffle = *on,
            _ => {}
        }
        self.inner.cmd(op)
    }
}

async fn deferred_with_shuffle(ops: Vec<Op>, want: bool, sticky: bool) -> Vec<Op> {
    let s = Stub::new("playing");
    let p = Arc::new(ShuffleStub { inner: s.clone(), after_play: sticky });
    let t = Arc::new(Talk::new(Duration::from_secs(60)));
    t.handle(Mark::Start, p.clone());
    assert!(t.in_talk());
    assert!(t.defer_start(ops, Some(want)));
    t.handle(Mark::End, p);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    s.ops()
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_liked_rechecks_shuffle_off() {
    let ops = deferred_with_shuffle(vec![Op::Shuffle(false), Op::Play(Some("c".into()))], false, true).await;
    let tail: Vec<Op> = ops.into_iter().skip_while(|o| *o != Op::Shuffle(false)).collect();
    assert_eq!(tail, vec![Op::Shuffle(false), Op::Play(Some("c".into())), Op::Shuffle(false)]);
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_liked_shuffle_rechecks_shuffle_on_and_next() {
    let ops = deferred_with_shuffle(vec![Op::Shuffle(true), Op::Play(Some("c".into()))], true, false).await;
    let tail: Vec<Op> = ops.into_iter().skip_while(|o| *o != Op::Shuffle(true)).collect();
    assert_eq!(tail, vec![Op::Shuffle(true), Op::Play(Some("c".into())), Op::Shuffle(true), Op::Next]);
}

#[tokio::test]
async fn false_wake_style_end_after_answer_is_delayed() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::with_timing(Duration::from_secs(60), Duration::from_secs(600), Duration::from_millis(300)));
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    t.handle(Mark::EndAfterAnswer, s.clone());
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(t.in_talk());
    assert_eq!(s.ops(), vec![Op::Pause]);
    assert!(until(|| s.ops() == vec![Op::Pause, Op::Resume]).await);
    assert!(!t.in_talk());
}

#[tokio::test]
async fn new_start_cancels_delayed_end() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::with_timing(Duration::from_secs(60), Duration::from_secs(600), Duration::from_millis(250)));
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    t.handle(Mark::EndAfterAnswer, s.clone());
    t.handle(Mark::Start, s.clone());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(t.in_talk());
    assert_eq!(s.ops(), vec![Op::Pause]);
}

#[tokio::test]
async fn end_forget_does_not_resume() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    t.handle(Mark::EndForget, s.clone());
    settle().await;
    settle().await;
    assert!(!t.in_talk());
    assert_eq!(s.ops(), vec![Op::Pause]);
}

#[tokio::test]
async fn end_forget_still_runs_deferred_start() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::default());
    t.handle(Mark::Start, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    assert!(t.defer(Op::Play(Some("u2".into()))));
    t.handle(Mark::EndForget, s.clone());
    assert!(until(|| s.ops() == vec![Op::Pause, Op::Play(Some("u2".into()))]).await);
}

fn write_log(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).unwrap();
}

#[tokio::test]
async fn plugin_lines_do_not_refresh_failsafe() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::new(Duration::from_millis(300)));
    let log = std::env::temp_dir().join(format!("talk-plug-{}.log", std::process::id()));
    write_log(&log, "");
    t.clone().spawn_with(s.clone(), log.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut body = line("Джарвис: активация (слово, уверенность 1.00)\n");
    write_log(&log, &body);
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    // плагины и посторонние подсистемы пишут дольше страховки: она всё равно срабатывает
    let stop = std::time::Instant::now();
    let mut i = 0;
    while s.ops().len() < 2 && stop.elapsed() < Duration::from_secs(3) {
        body.push_str(&format!("spotify-плагин: шум {i}\n{}spotify: шум\n", TS));
        write_log(&log, &body);
        i += 1;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(s.ops(), vec![Op::Pause, Op::Resume]);
    assert!(stop.elapsed() < Duration::from_millis(1500));
    let _ = std::fs::remove_file(&log);
}

#[tokio::test]
async fn hard_cap_ends_talk_despite_activity() {
    let s = Stub::new("playing");
    let t = Arc::new(Talk::with_timing(Duration::from_secs(60), Duration::from_millis(500), Duration::from_secs(2)));
    let log = std::env::temp_dir().join(format!("talk-cap-{}.log", std::process::id()));
    write_log(&log, "");
    t.clone().spawn_with(s.clone(), log.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut body = line("Джарвис: активация (слово)\n");
    write_log(&log, &body);
    assert!(until(|| s.ops() == vec![Op::Pause]).await);
    let begin = std::time::Instant::now();
    let mut i = 0;
    while s.ops().len() < 2 && begin.elapsed() < Duration::from_secs(3) {
        body.push_str(&line(&format!("Gemini: шум {i}\n")));
        write_log(&log, &body);
        i += 1;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(s.ops(), vec![Op::Pause, Op::Resume]);
    assert!(begin.elapsed() < Duration::from_millis(1800));
    let _ = std::fs::remove_file(&log);
}
