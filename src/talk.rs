//! Пауза Spotify на время разговора Джарвиса: хвост лога ядра, классификация меток,
//! автомат `TalkPause` и страховочный таймер.

use crate::soloist::{Op, Soloist, SpotState};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const FAILSAFE: Duration = Duration::from_secs(180);
/// Моменты сверки перемешивания после Play (от Play): Spotify может вернуть прежнее позже.
/// Потолок сверки от Play.
pub const MAX_RECHECK: Duration = Duration::from_secs(6);
pub const RECHECKS: [Duration; 3] = [Duration::from_millis(1000), Duration::from_millis(2500), Duration::from_millis(5000)];
const POLL_EVERY: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Start,
    /// Разговор закончился, ничего не говорится: можно возобновлять сразу.
    End,
    /// Разговор закрыт маршрутом (плагин, Алиса, Smart Life, быстрая команда): ядро может ещё
    /// коротко озвучить ответ, поэтому конец откладывается на `END_DELAY`.
    EndAfterAnswer,
    /// Разговор закрыт, потому что заиграл другой плеер (YouTube): свою паузу не возобновлять,
    /// отложенный запуск Spotify выполнить.
    EndForget,
}

/// Через сколько после строки о маршруте считать разговор законченным (озвучка короткого ответа).
pub const END_DELAY: Duration = Duration::from_secs(2);
/// Жёсткий потолок разговора от метки старта, что бы ни писалось в лог.
pub const HARD_CAP: Duration = Duration::from_secs(600);

/// Сообщение строки лога без префикса времени `ГГГГ/ММ/ДД ЧЧ:ММ:СС `.
pub fn message(line: &str) -> &str {
    let b = line.as_bytes();
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let ok = b.len() >= 20
        && digits.iter().all(|&i| b[i].is_ascii_digit())
        && b[4] == b'/'
        && b[7] == b'/'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b' ';
    if ok { &line[20..] } else { line }
}

/// Метка разговора в строке лога ядра. Шаблоны привязаны к началу сообщения (после времени):
/// ядро повторяет в логе слова пользователя и ответы в `«…»`, строки плагинов идут без времени.
pub fn classify(line: &str) -> Option<Mark> {
    let m = message(line);
    if m.starts_with("Джарвис: активация") {
        return Some(Mark::Start);
    }
    if m.starts_with("разговор: музыка включена — закрываю разговор") {
        return Some(Mark::EndForget);
    }
    if (m.starts_with("разговор: тишина") && m.contains("окончен"))
        || m.starts_with("Джарвис: разговор прерван")
        || m.starts_with("разговор не открылся")
        || m.starts_with("Джарвис: ложное срабатывание")
        || m.starts_with("Джарвис: остановлен до начала разговора")
    {
        return Some(Mark::End);
    }
    // `Джарвис: «текст» — …` и `Джарвис: «текст» → …`: хвост считается после первой `»`.
    if let Some(rest) = m.strip_prefix("Джарвис: «") {
        let tail = rest.split_once('»')?.1;
        if tail.starts_with(" — разговор закрыт") || tail.starts_with(" — стоп") {
            return Some(Mark::End);
        }
        if tail.starts_with(" → плагин") || tail.starts_with(" → Алисе") || tail.starts_with(" → Smart Life") {
            return Some(Mark::EndAfterAnswer);
        }
        return None;
    }
    if let Some(rest) = m.strip_prefix("Джарвис: быстрая команда «") {
        if rest.split_once('»').is_some_and(|(_, tail)| tail.starts_with(" →")) {
            return Some(Mark::EndAfterAnswer);
        }
    }
    None
}

/// Строка относится к разговору и продлевает страховку (плагины, `spotify:` и прочее — нет).
pub fn is_activity(line: &str) -> bool {
    let m = message(line);
    ["Gemini:", "Джарвис:", "разговор", "инструмент"].iter().any(|p| m.starts_with(p))
}

/// Пауза Spotify на время разговора: помним, что именно мы остановили.
#[derive(Default)]
pub struct TalkPause {
    /// uri трека в момент нашей паузы.
    ours: Option<String>,
    /// Разговор идёт: запуски Spotify откладываем до его конца.
    active: bool,
    /// Отложенная команда (запуск после разговора, чтобы не играть поверх него).
    deferred: Option<(Vec<Op>, Option<bool>)>,
    /// После нашего next/prev возобновляем то, что играет сейчас, а не только исходный трек.
    follow: bool,
}

impl TalkPause {
    /// Джарвис проснулся. `true` — Spotify играет и его надо поставить на паузу.
    pub fn on_wake(&mut self, sp: &SpotState) -> bool {
        if self.ours.is_none() && self.deferred.is_none() && sp.status == "playing" {
            self.ours = Some(sp.uri.clone());
            return true;
        }
        false
    }

    /// Разговор закончился. `true` — пауза наша и ничего не сменилось: надо слать play. Всегда забывает паузу.
    pub fn on_end(&mut self, sp: &SpotState) -> bool {
        let ours = self.ours.take();
        let follow = std::mem::take(&mut self.follow);
        ours.is_some_and(|u| follow || u == sp.uri) && sp.status == "paused"
    }

    /// Пауза не состоялась или человек сам распорядился музыкой: возобновлять нечего.
    pub fn forget(&mut self) {
        self.ours = None;
        self.follow = false;
        self.deferred = None;
    }

    /// Забыть нашу паузу, но оставить отложенный запуск (его ждёт пользователь).
    pub fn forget_pause(&mut self) {
        self.ours = None;
        self.follow = false;
    }

    /// Разговор начался.
    pub fn begin(&mut self) {
        self.active = true;
    }

    /// Разговор закончился: новые запуски идут сразу.
    pub fn finish(&mut self) {
        self.active = false;
    }

    /// Пока идёт разговор — запоминает команду на после него (и заменяет «вернуть нашу паузу»).
    /// `false` — разговора нет, команду надо выполнить сразу.
    pub fn defer(&mut self, op: Op) -> bool {
        self.defer_seq(vec![op])
    }

    /// То же для короткой последовательности (например, «перемешивание выкл» и запуск).
    pub fn defer_seq(&mut self, ops: Vec<Op>) -> bool {
        self.defer_start(ops, None)
    }

    /// Как `defer_seq`; `want_shuffle` — какое перемешивание должно быть после запуска.
    pub fn defer_start(&mut self, ops: Vec<Op>, want_shuffle: Option<bool>) -> bool {
        if !self.active {
            return false;
        }
        self.ours = None;
        self.follow = false;
        self.deferred = Some((ops, want_shuffle));
        true
    }

    pub fn take_deferred(&mut self) -> Option<Vec<Op>> {
        self.deferred.take().map(|(ops, _)| ops)
    }

    fn take_deferred_full(&mut self) -> Option<(Vec<Op>, Option<bool>)> {
        self.deferred.take()
    }

    /// Мы сменили трек (next/prev) во время нашей паузы: возобновить надо тот, что стал текущим.
    pub fn follow(&mut self) {
        if self.ours.is_some() {
            self.follow = true;
        }
    }

    pub fn is_ours(&self) -> bool {
        self.ours.is_some()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

/// Состояние для решения о возобновлении: живой опрос, а при сбое — последнее известное.
fn end_state(live: Option<SpotState>, cached: SpotState) -> SpotState {
    live.filter(|s| !s.status.is_empty()).unwrap_or(cached)
}

type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// То, что от Soloist нужно наблюдателю; в тестах подменяется заглушкой.
pub trait Player: Send + Sync + 'static {
    fn now(&self) -> Fut<'_, Option<SpotState>>;
    fn cmd(&self, op: Op) -> Fut<'_, Result<(), String>>;
}

impl Player for Soloist {
    fn now(&self) -> Fut<'_, Option<SpotState>> {
        Box::pin(Soloist::now(self))
    }
    fn cmd(&self, op: Op) -> Fut<'_, Result<(), String>> {
        Box::pin(Soloist::cmd(self, op))
    }
}

/// Общий запуск для немедленного и отложенного путей: команды по порядку (сбой не-Play
/// игнорируется, сбой Play — ошибка), затем в фоне сверка перемешивания, т.к. Spotify может вернуть прежнее.
pub async fn run_start(p: Arc<dyn Player>, ops: Vec<Op>, want_shuffle: Option<bool>) -> Result<(), String> {
    run_start_with(p, ops, want_shuffle, &RECHECKS).await
}

/// Как `run_start`, но с заданными моментами сверки (от Play, по возрастанию; не дольше 6 с).
pub async fn run_start_with(
    p: Arc<dyn Player>,
    ops: Vec<Op>,
    want_shuffle: Option<bool>,
    checks: &[Duration],
) -> Result<(), String> {
    for op in ops {
        let is_play = matches!(op, Op::Play(_));
        if let Err(e) = p.cmd(op).await {
            if is_play {
                return Err(e);
            }
        }
    }
    if let Some(want) = want_shuffle {
        let checks: Vec<Duration> = checks.iter().copied().filter(|d| *d <= MAX_RECHECK).collect();
        tokio::spawn(async move {
            let began = tokio::time::Instant::now();
            let (mut first, mut good) = (true, 0);
            for at in checks {
                tokio::time::sleep_until(began + at).await;
                match p.now().await {
                    Some(s) if !s.status.is_empty() && s.shuffle == want => {
                        good += 1;
                        if good >= 2 {
                            return;
                        }
                    }
                    Some(s) if !s.status.is_empty() => {
                        good = 0;
                        let _ = p.cmd(Op::Shuffle(want)).await;
                        if want && first {
                            let _ = p.cmd(Op::Next).await;
                        }
                        first = false;
                    }
                    _ => good = 0,
                }
            }
        });
    }
    Ok(())
}

pub struct Talk {
    pause: Mutex<TalkPause>,
    /// Состояние на момент нашей паузы — запасное для решения о возобновлении.
    cached: Mutex<SpotState>,
    /// Сериализует паузу и возобновление.
    gate: tokio::sync::Mutex<()>,
    /// Номер разговора: страховочный таймер действует только на «свой».
    epoch: AtomicU64,
    failsafe: Duration,
    /// Потолок длины разговора от метки старта.
    hard_cap: Duration,
    /// Задержка конца разговора после маршрута с коротким ответом.
    end_delay: Duration,
    /// Последняя активность в логе во время разговора: от неё считается страховка.
    last_activity: Mutex<Instant>,
}

impl Default for Talk {
    fn default() -> Self {
        Talk::new(FAILSAFE)
    }
}

impl Talk {
    pub fn new(failsafe: Duration) -> Self {
        Talk::with_timing(failsafe, HARD_CAP, END_DELAY)
    }

    pub fn with_timing(failsafe: Duration, hard_cap: Duration, end_delay: Duration) -> Self {
        Talk {
            pause: Mutex::new(TalkPause::default()),
            cached: Mutex::new(SpotState::default()),
            gate: tokio::sync::Mutex::new(()),
            epoch: AtomicU64::new(0),
            failsafe,
            hard_cap,
            end_delay,
            last_activity: Mutex::new(Instant::now()),
        }
    }

    pub fn in_talk(&self) -> bool {
        self.pause.lock().unwrap().is_active()
    }

    /// Во время разговора запоминает команду на его конец; `false` — выполнять сразу.
    pub fn defer(&self, op: Op) -> bool {
        self.pause.lock().unwrap().defer(op)
    }

    /// Запуск с ожидаемым перемешиванием: после старта оно сверяется (см. `run_start`).
    pub fn defer_start(&self, ops: Vec<Op>, want_shuffle: Option<bool>) -> bool {
        self.pause.lock().unwrap().defer_start(ops, want_shuffle)
    }

    /// Как `defer`, но для последовательности команд.
    pub fn defer_seq(&self, ops: Vec<Op>) -> bool {
        self.pause.lock().unwrap().defer_seq(ops)
    }

    /// Наша пауза стоит (и запуск не отложен): после разговора музыка вернётся.
    pub fn paused_by_us(&self) -> bool {
        self.pause.lock().unwrap().is_ours()
    }

    /// Человек сам распорядился музыкой: возобновлять не надо.
    pub fn user_paused(&self) {
        self.pause.lock().unwrap().forget();
    }

    /// Мы сменили трек во время нашей паузы.
    pub fn followed(&self) {
        self.pause.lock().unwrap().follow();
    }

    /// Обрабатывает метку. Не блокирует: работа со Soloist уходит в фон.
    pub fn handle(self: &Arc<Self>, mark: Mark, player: Arc<dyn Player>) {
        match mark {
            Mark::Start => {
                self.pause.lock().unwrap().begin();
                let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                let me = self.clone();
                let p = player.clone();
                tokio::spawn(async move {
                    let _gate = me.gate.lock().await;
                    me.pause_for_talk(&*p).await;
                });
                let me = self.clone();
                self.activity();
                let started = Instant::now();
                tokio::spawn(async move {
                    loop {
                        let idle = me.last_activity.lock().unwrap().elapsed();
                        let age = started.elapsed();
                        if idle >= me.failsafe || age >= me.hard_cap {
                            break;
                        }
                        tokio::time::sleep((me.failsafe - idle).min(me.hard_cap - age)).await;
                        if me.epoch.load(Ordering::SeqCst) != epoch || !me.in_talk() {
                            return;
                        }
                    }
                    if me.epoch.load(Ordering::SeqCst) == epoch && me.in_talk() {
                        eprintln!("spotify-плагин: не увидел конец разговора");
                        me.end(player);
                    }
                });
            }
            Mark::End => self.end(player),
            Mark::EndForget => self.end_with(player, true),
            Mark::EndAfterAnswer => {
                let epoch = self.epoch.load(Ordering::SeqCst);
                let me = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(me.end_delay).await;
                    // Новый разговор (или уже случившийся конец) отменяет отложенный конец.
                    if me.epoch.load(Ordering::SeqCst) == epoch && me.in_talk() {
                        me.end(player);
                    }
                });
            }
        }
    }

    /// Строка разговора в логе: разговор жив, страховка отсчитывается заново.
    pub fn activity(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    fn end(self: &Arc<Self>, player: Arc<dyn Player>) {
        self.end_with(player, false);
    }

    /// `forget`: свою паузу не возобновлять (отложенный запуск всё равно выполнится).
    fn end_with(self: &Arc<Self>, player: Arc<dyn Player>, forget: bool) {
        self.pause.lock().unwrap().finish();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let me = self.clone();
        tokio::spawn(async move {
            let _gate = me.gate.lock().await;
            // Под замком: пауза уже поставлена или не будет поставлена.
            if forget {
                me.pause.lock().unwrap().forget_pause();
            }
            me.resume_after_talk(player).await;
        });
    }

    async fn pause_for_talk(&self, p: &dyn Player) {
        let mut sp = p.now().await.filter(|s| !s.status.is_empty());
        if sp.is_none() {
            sp = p.now().await.filter(|s| !s.status.is_empty());
        }
        let Some(sp) = sp else {
            eprintln!("spotify-плагин: состояние Spotify не получено, паузу не ставлю");
            return;
        };
        if !self.pause.lock().unwrap().on_wake(&sp) {
            return;
        }
        *self.cached.lock().unwrap() = SpotState { status: "paused".into(), ..sp };
        if p.cmd(Op::Pause).await.is_err() {
            self.pause.lock().unwrap().forget();
            eprintln!("spotify-плагин: пауза на время разговора не удалась");
        }
    }

    async fn resume_after_talk(&self, p: Arc<dyn Player>) {
        let deferred = self.pause.lock().unwrap().take_deferred_full();
        if let Some((ops, want)) = deferred {
            if let Err(e) = run_start(p.clone(), ops, want).await {
                eprintln!("spotify-плагин: отложенный запуск не удался: {e}");
            }
            return;
        }
        if !self.pause.lock().unwrap().is_ours() {
            return;
        }
        let mut live = p.now().await;
        if live.is_none() {
            live = p.now().await;
        }
        let sp = end_state(live, self.cached.lock().unwrap().clone());
        if self.pause.lock().unwrap().on_end(&sp) {
            if let Err(e) = p.cmd(Op::Resume).await {
                eprintln!("spotify-плагин: не удалось продолжить после разговора: {e}");
            }
        }
    }

    /// Фоновый цикл: читает лог каждые 150 мс и применяет метки.
    pub fn spawn(self: Arc<Self>, soloist: Arc<Soloist>, log: PathBuf) -> tokio::task::JoinHandle<()> {
        self.spawn_with(soloist, log)
    }

    pub fn spawn_with(self: Arc<Self>, player: Arc<dyn Player>, log: PathBuf) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tail = LogTail::open(&log);
            loop {
                for line in tail.poll() {
                    if self.in_talk() && is_activity(&line) {
                        self.activity();
                    }
                    if let Some(m) = classify(&line) {
                        self.handle(m, player.clone());
                    }
                }
                tokio::time::sleep(POLL_EVERY).await;
            }
        })
    }
}

const HEAD_LEN: usize = 64;

/// Читает строки, дописанные в лог; переоткрывает при усечении, смене inode и подмене содержимого.
pub struct LogTail {
    path: PathBuf,
    pos: u64,
    ino: Option<u64>,
    /// Первые байты файла: отпечаток, меняется при пересоздании с тем же inode.
    head: Vec<u8>,
    partial: Vec<u8>,
}

fn read_head(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(HEAD_LEN as u64).read_to_end(&mut buf);
    }
    buf
}

impl LogTail {
    /// Начинает с конца существующего файла: старые разговоры не проигрываем.
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let (pos, ino, head) = match std::fs::metadata(&path) {
            Ok(m) => (m.len(), Some(m.ino()), read_head(&path)),
            Err(_) => (0, None, Vec::new()),
        };
        LogTail { path, pos, ino, head, partial: Vec::new() }
    }

    fn reset(&mut self) {
        self.pos = 0;
        self.ino = None;
        self.head.clear();
        self.partial.clear();
    }

    pub fn poll(&mut self) -> Vec<String> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            self.reset();
            return Vec::new();
        };
        let head = read_head(&self.path);
        let same_head = head.starts_with(&self.head);
        if self.ino != Some(meta.ino()) || meta.len() < self.pos || !same_head {
            self.reset();
        }
        self.ino = Some(meta.ino());
        if head.len() > self.head.len() {
            self.head = head;
        }
        if meta.len() == self.pos {
            return Vec::new();
        }
        let Ok(mut f) = std::fs::File::open(&self.path) else { return Vec::new() };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.take(meta.len() - self.pos).read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.pos += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let mut out = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            out.push(String::from_utf8_lossy(&line[..i]).into_owned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spt(status: &str, uri: &str) -> SpotState {
        SpotState { status: status.into(), uri: uri.into(), ..Default::default() }
    }
    fn play(u: &str) -> Op {
        Op::Play(Some(u.into()))
    }
    #[test] fn resumes_only_our_pause() {
        let mut t = TalkPause::default();
        assert!(t.on_wake(&spt("playing", "u1")));
        assert!(t.on_end(&spt("paused", "u1")));
    }
    #[test] fn no_resume_if_something_else_started() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        assert!(!t.on_end(&spt("playing", "u2")));
    }
    #[test] fn no_resume_if_other_track_paused() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        assert!(!t.on_end(&spt("paused", "u2")));
    }
    #[test] fn no_pause_when_not_playing() {
        let mut t = TalkPause::default();
        assert!(!t.on_wake(&spt("paused", "")));
        assert!(!t.on_end(&spt("paused", "")));
    }
    #[test] fn second_wake_keeps_first_pause() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        assert!(!t.on_wake(&spt("paused", "u1")));
        assert!(t.on_end(&spt("paused", "u1")));
    }
    #[test] fn failed_now_falls_back_to_cached_pause() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        let sp = end_state(None, spt("paused", "u1"));
        assert!(t.on_end(&sp));
        let sp = end_state(Some(SpotState::default()), spt("paused", "u1"));
        assert_eq!(sp.uri, "u1");
    }
    #[test] fn end_always_forgets() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        assert!(!t.on_end(&spt("playing", "u1")));
        assert!(!t.on_end(&spt("paused", "u1")));
    }
    #[test] fn forget_cancels_resume() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        t.forget();
        assert!(!t.on_end(&spt("paused", "u1")));
    }
    #[test] fn defer_only_during_talk() {
        let mut t = TalkPause::default();
        assert!(!t.defer(Op::Play(None)));
        t.begin();
        assert!(t.defer(play("spotify:track:1")));
        t.finish();
        assert!(!t.defer(Op::Play(None)));
    }
    #[test] fn deferred_play_replaces_resume() {
        let mut t = TalkPause::default();
        t.begin();
        assert!(t.on_wake(&spt("playing", "u1")));
        assert!(t.defer(play("u2")));
        assert!(!t.is_ours());
        assert!(!t.on_end(&spt("paused", "u1")));
        assert_eq!(t.take_deferred(), Some(vec![play("u2")]));
        assert_eq!(t.take_deferred(), None);
    }
    #[test] fn deferred_sequence_is_kept_in_order() {
        let mut t = TalkPause::default();
        assert!(!t.defer_seq(vec![Op::Shuffle(false), play("u")]));
        t.begin();
        assert!(t.defer_seq(vec![Op::Shuffle(false), play("u")]));
        assert_eq!(t.take_deferred(), Some(vec![Op::Shuffle(false), play("u")]));
    }
    #[test] fn late_wake_does_not_override_deferred() {
        let mut t = TalkPause::default();
        t.begin();
        t.defer(Op::Play(None));
        assert!(!t.on_wake(&spt("playing", "u1")));
    }
    #[test] fn manual_pause_cancels_deferred() {
        let mut t = TalkPause::default();
        t.begin();
        t.defer(Op::Play(None));
        t.forget();
        assert_eq!(t.take_deferred(), None);
    }
    #[test] fn follow_resumes_whatever_is_current() {
        let mut t = TalkPause::default();
        assert!(t.on_wake(&spt("playing", "u1")));
        t.follow();
        assert!(t.on_end(&spt("paused", "u2")));
        // follow не переживает разговор
        assert!(t.on_wake(&spt("playing", "u3")));
        assert!(!t.on_end(&spt("paused", "u4")));
    }
    #[test] fn follow_never_resumes_manual_pause() {
        let mut t = TalkPause::default();
        t.on_wake(&spt("playing", "u1"));
        t.follow();
        t.forget();
        assert!(!t.on_end(&spt("paused", "u2")));
        // без нашей паузы follow ничего не создаёт
        let mut t = TalkPause::default();
        t.follow();
        assert!(!t.on_end(&spt("paused", "u2")));
    }
}
