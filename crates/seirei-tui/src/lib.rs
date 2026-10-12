pub mod app;
pub mod keymap;
pub mod render;

#[cfg(test)]
mod fixtures;

use app::{App, Effect, Event, Link};
use crossterm::event::{Event as TermEvent, KeyEventKind};
use seirei_proto::{
    ids::SendId,
    wire::{BuildId, ClientMsg, ServerMsg, Subscription, decode, encode},
};
use std::{
    io::{self, ErrorKind},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Notify, mpsc},
};

const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);
const SETTLED: Duration = Duration::from_secs(2);

pub struct Launch {
    pub socket: PathBuf,
    pub open: Option<String>,
    pub spawn_daemon: fn() -> io::Result<()>,
}

pub async fn run(launch: Launch) -> anyhow::Result<()> {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (outbound, outbound_rx) = mpsc::unbounded_channel();
    let reconnect = Arc::new(Notify::new());
    let mut app = App::new(launch.open, BuildId::current(), SendId::fresh());
    let mut terminal = ratatui::init();
    spawn_input(events_tx.clone());
    tokio::spawn(link(launch.socket, launch.spawn_daemon, events_tx, outbound_rx, reconnect.clone()));

    let mut sent: Option<Subscription> = None;
    let result = loop {
        if let Err(e) = terminal.draw(|frame| render::render(&app, frame)) {
            break Err(e.into());
        }
        let Some(event) = events.recv().await else { break Ok(()) };
        if let Event::Disconnected(_) = event {
            sent = None;
        }
        let mut quit = false;
        for effect in app.update(event) {
            match effect {
                Effect::Send(msg) => drop(outbound.send(msg)),
                Effect::Reconnect => reconnect.notify_one(),
                Effect::Quit => quit = true,
            }
        }
        if quit {
            break Ok(());
        }
        let wanted = app.subscription();
        if app.link == Link::Up && sent.as_ref() != Some(&wanted) {
            drop(outbound.send(ClientMsg::Subscribe(wanted.clone())));
            sent = Some(wanted);
        }
    };
    ratatui::restore();
    result
}

fn spawn_input(events: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            let event = match crossterm::event::read() {
                Ok(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => Event::Key(key),
                Ok(TermEvent::Resize(..)) => Event::Resize,
                Ok(_) => continue,
                Err(_) => return,
            };
            if events.send(event).is_err() {
                return;
            }
        }
    });
}

async fn link(
    socket: PathBuf,
    spawn_daemon: fn() -> io::Result<()>,
    events: mpsc::UnboundedSender<Event>,
    mut outbound: mpsc::UnboundedReceiver<ClientMsg>,
    reconnect: Arc<Notify>,
) {
    let mut pause = RETRY_MIN;
    let mut spawned = false;
    loop {
        let reason = match UnixStream::connect(&socket).await {
            Ok(stream) => {
                spawned = false;
                while outbound.try_recv().is_ok() {}
                let opened = Instant::now();
                if events.send(Event::Connected).is_err() {
                    return;
                }
                let reason = match session(stream, &events, &mut outbound).await {
                    SessionEnd::Reconnect(reason) => reason,
                    SessionEnd::Stop => return,
                };
                if opened.elapsed() >= SETTLED {
                    pause = RETRY_MIN;
                }
                reason
            }
            Err(e) if !spawned && matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                spawned = true;
                match spawn_daemon() {
                    Ok(()) => "starting the daemon".into(),
                    Err(e) => format!("could not start the daemon: {e}"),
                }
            }
            Err(e) => format!("cannot reach the daemon at {}: {e}", socket.display()),
        };
        if events.send(Event::Disconnected(reason)).is_err() {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(pause) => pause = (pause * 2).min(RETRY_MAX),
            () = reconnect.notified() => pause = RETRY_MIN,
        }
    }
}

enum SessionEnd {
    Reconnect(String),
    /// The app is gone, so nobody needs the link.
    Stop,
}

async fn session(
    stream: UnixStream,
    events: &mpsc::UnboundedSender<Event>,
    outbound: &mut mpsc::UnboundedReceiver<ClientMsg>,
) -> SessionEnd {
    enum Step {
        Read(io::Result<usize>),
        Write(Option<ClientMsg>),
    }
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        let step = tokio::select! {
            n = read.read_until(b'\n', &mut line) => Step::Read(n),
            msg = outbound.recv() => Step::Write(msg),
        };
        match step {
            Step::Read(Err(e)) => return SessionEnd::Reconnect(format!("lost the daemon: {e}")),
            Step::Read(Ok(_)) if line.last() != Some(&b'\n') => {
                return SessionEnd::Reconnect("the daemon closed the connection".into());
            }
            Step::Read(Ok(_)) => {
                let msg = match decode::<ServerMsg>(&line) {
                    Ok(msg) => msg,
                    Err(e) => {
                        return SessionEnd::Reconnect(format!("the daemon sent a frame this build cannot read: {e}"));
                    }
                };
                line.clear();
                if events.send(Event::Server(msg)).is_err() {
                    return SessionEnd::Stop;
                }
            }
            Step::Write(None) => return SessionEnd::Stop,
            Step::Write(Some(msg)) => {
                if let Err(e) = write.write_all(&encode(&msg)).await {
                    return SessionEnd::Reconnect(format!("lost the daemon: {e}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::UnixListener;

    static SPAWNS: AtomicUsize = AtomicUsize::new(0);

    fn count_spawn() -> io::Result<()> {
        SPAWNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[tokio::test]
    async fn the_link_spawns_once_per_outage_pumps_lines_and_reconnects() {
        let dir = std::env::temp_dir().join(format!("seirei-tui-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("daemon.sock");
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (outbound, outbound_rx) = mpsc::unbounded_channel();
        let reconnect = Arc::new(Notify::new());
        tokio::spawn(link(socket.clone(), count_spawn, events_tx, outbound_rx, reconnect.clone()));

        assert_eq!(events.recv().await, Some(Event::Disconnected("starting the daemon".into())));
        let Some(Event::Disconnected(again)) = events.recv().await else { panic!("expected a second failure") };
        assert!(again.starts_with("cannot reach the daemon"), "{again}");
        assert_eq!(SPAWNS.load(Ordering::SeqCst), 1, "one spawn per outage");

        let listener = UnixListener::bind(&socket).unwrap();
        outbound.send(ClientMsg::Subscribe(Subscription::default())).unwrap();
        reconnect.notify_one();
        let (stream, _) = listener.accept().await.unwrap();
        assert_eq!(events.recv().await, Some(Event::Connected));
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();

        let hello = ClientMsg::Hello { build: seirei_proto::wire::BuildId("b".into()), take_over: false };
        outbound.send(hello.clone()).unwrap();
        let first = lines.next_line().await.unwrap().unwrap();
        assert_eq!(decode::<ClientMsg>(first.as_bytes()).unwrap(), hello, "messages queued while down are dropped");
        write.write_all(&encode(&ServerMsg::Welcome)).await.unwrap();
        assert_eq!(events.recv().await, Some(Event::Server(ServerMsg::Welcome)));

        drop((lines, write));
        assert_eq!(events.recv().await, Some(Event::Disconnected("the daemon closed the connection".into())));
        reconnect.notify_one();
        listener.accept().await.unwrap();
        assert_eq!(events.recv().await, Some(Event::Connected));
        assert_eq!(SPAWNS.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
