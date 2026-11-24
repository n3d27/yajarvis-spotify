use spotify_core::soloist::{Op, Soloist};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const NOW_JSON: &str = include_str!("fixtures/now_playing.json");

struct Env {
    dir: PathBuf,
    log: PathBuf,
}

impl Env {
    fn new(name: &str, with_control: bool, pos_ms: u64) -> Env {
        let dir = std::env::temp_dir().join(format!("soloist-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        let log = dir.join("calls.log");
        let now = NOW_JSON.replace("62762", &pos_ms.to_string()).replace("paused", "playing");
        std::fs::write(dir.join("now.json"), now).unwrap();
        let stamp = "perl -MTime::HiRes=time -e 'printf \"%.3f\", time'";
        let soloist = format!(
            "#!/bin/sh\necho \"$({stamp}) soloist LD=$LD_LIBRARY_PATH $*\" >> '{log}'\n\
             case \"$*\" in *'now --json'*) cat '{now}';; esac\n\
             [ -f '{hang}' ] && {{ sleep 30 & echo $! > '{pids}'; echo $$ >> '{pids}'; wait; }}\nexit 0\n",
            log = log.display(),
            now = dir.join("now.json").display(),
            hang = dir.join("hang_soloist").display(),
            pids = dir.join("soloist.pids").display(),
        );
        write_exec(&dir.join("bin/soloist"), &soloist);
        if with_control {
            let control = format!(
                "#!/bin/sh\necho \"control.sh cwd=$(pwd) $*\" >> '{log}'\n\
                 [ -f '{hang}' ] && {{ sleep 30 & echo $! > '{pids}'; echo $$ >> '{pids}'; wait; }}\nexit 0\n",
                log = log.display(),
                hang = dir.join("hang_control").display(),
                pids = dir.join("control.pids").display(),
            );
            write_exec(&dir.join("scripts/control.sh"), &control);
        }
        Env { dir, log }
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Вызовы soloist без опроса состояния: (время, аргументы после "ctl -w <addr>").
    fn ctl_calls(&self) -> Vec<(f64, String)> {
        self.lines()
            .iter()
            .filter(|l| l.contains(" soloist "))
            .map(|l| {
                let (ts, rest) = l.split_once(' ').unwrap();
                let args = rest.split("ctl -w 127.0.0.1:9090 ").nth(1).unwrap().to_string();
                (ts.parse().unwrap(), args)
            })
            .filter(|(_, a)| !a.starts_with("now"))
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill").args(["-0", pid.trim()]).output().unwrap().status.success()
}

fn assert_pids_gone(file: &Path) {
    std::thread::sleep(Duration::from_millis(200));
    let pids = std::fs::read_to_string(file).expect("stub did not start");
    for pid in pids.lines() {
        assert!(!alive(pid), "process {pid} outlived the call");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uses_control_sh_when_present() {
    let env = Env::new("control", true, 1000);
    Soloist::new(&env.dir).cmd(Op::Next).await.unwrap();
    assert_eq!(env.lines(), vec!["control.sh cwd=/ next"]);
}

#[tokio::test(flavor = "current_thread")]
async fn control_sh_words() {
    let env = Env::new("words", true, 1000);
    let s = Soloist::new(&env.dir);
    s.cmd(Op::Play(Some("spotify:track:x".into()))).await.unwrap();
    s.cmd(Op::Resume).await.unwrap();
    s.cmd(Op::Pause).await.unwrap();
    s.cmd(Op::Prev).await.unwrap();
    s.cmd(Op::Shuffle(true)).await.unwrap();
    s.cmd(Op::Shuffle(false)).await.unwrap();
    assert_eq!(
        env.lines(),
        vec![
            "control.sh cwd=/ play spotify:track:x",
            "control.sh cwd=/ play",
            "control.sh cwd=/ pause",
            "control.sh cwd=/ prev",
            "control.sh cwd=/ shuffle on",
            "control.sh cwd=/ shuffle off",
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn falls_back_to_ctl_with_activate() {
    let env = Env::new("fallback", false, 1000);
    Soloist::new(&env.dir).cmd(Op::Resume).await.unwrap();
    let calls = env.ctl_calls();
    let verbs: Vec<&str> = calls.iter().map(|c| c.1.as_str()).collect();
    assert_eq!(verbs, vec!["activate", "play"]);
    assert!(calls[1].0 - calls[0].0 >= 0.3, "gap {}", calls[1].0 - calls[0].0);
    let lib = format!("LD={}/lib64 ", env.dir.display());
    assert!(env.lines()[0].contains(&lib), "{}", env.lines()[0]);
}

#[tokio::test(flavor = "current_thread")]
async fn prev_twice_after_3s() {
    let env = Env::new("prev5", false, 5000);
    Soloist::new(&env.dir).cmd(Op::Prev).await.unwrap();
    let calls = env.ctl_calls();
    assert_eq!(calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), vec!["prev", "prev"]);
    assert!(calls[1].0 - calls[0].0 >= 0.3);

    let env = Env::new("prev1", false, 1000);
    Soloist::new(&env.dir).cmd(Op::Prev).await.unwrap();
    assert_eq!(env.ctl_calls().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn seek_is_direct_ctl() {
    let env = Env::new("seek", true, 1000);
    Soloist::new(&env.dir).cmd(Op::Seek(42000)).await.unwrap();
    let calls = env.ctl_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "seek 42000");
    assert!(!env.lines().iter().any(|l| l.starts_with("control.sh")));
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_kills_and_errors() {
    let env = Env::new("hang", true, 1000);
    std::fs::write(env.dir.join("hang_control"), "").unwrap();
    let t = Instant::now();
    let r = Soloist::new(&env.dir).cmd(Op::Next).await;
    assert!(r.is_err());
    assert!(t.elapsed() < Duration::from_secs(9), "{:?}", t.elapsed());
    assert_pids_gone(&env.dir.join("control.pids"));
}

#[tokio::test(flavor = "current_thread")]
async fn ctl_timeout_kills_too() {
    let env = Env::new("hangctl", false, 1000);
    std::fs::write(env.dir.join("hang_soloist"), "").unwrap();
    let t = Instant::now();
    assert!(Soloist::new(&env.dir).cmd(Op::Pause).await.is_err());
    assert!(t.elapsed() < Duration::from_secs(9));
    assert_pids_gone(&env.dir.join("soloist.pids"));
    // now() has the shorter 4 s limit.
    let t = Instant::now();
    assert!(Soloist::new(&env.dir).now().await.is_none());
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "current_thread")]
async fn now_reads_state_from_stub() {
    let env = Env::new("now", false, 5000);
    let s = Soloist::new(&env.dir).now().await.unwrap();
    assert_eq!((s.status.as_str(), s.pos_ms), ("playing", 5000));
}

#[tokio::test(flavor = "current_thread")]
async fn missing_soloist_is_none() {
    let t = Instant::now();
    assert!(Soloist::new("/nonexistent/soloist-dir").now().await.is_none());
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "current_thread")]
async fn stale_snapshot_while_playing_sends_prev_twice() {
    let env = Env::new("stale", false, 1000);
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() - 10_000;
    let now = NOW_JSON
        .replace("62762", "1000")
        .replace("paused", "playing")
        .replace("\"timestamp_ms\":1700000000000,\"speed\":0", &format!("\"timestamp_ms\":{ts},\"speed\":1"));
    std::fs::write(env.dir.join("now.json"), now).unwrap();
    Soloist::new(&env.dir).cmd(Op::Prev).await.unwrap();
    assert_eq!(env.ctl_calls().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_sends_term_before_kill() {
    let env = Env::new("term", true, 1000);
    let marker = env.dir.join("term.marker");
    let body = format!(
        "#!/bin/sh\ntrap 'echo term > \'{m}\'; exit 143' TERM\nsleep 30 &\nwait\n",
        m = marker.display()
    );
    write_exec(&env.dir.join("scripts/control.sh"), &body);
    assert!(Soloist::new(&env.dir).cmd(Op::Next).await.is_err());
    assert!(marker.exists(), "TERM trap did not run");
}
