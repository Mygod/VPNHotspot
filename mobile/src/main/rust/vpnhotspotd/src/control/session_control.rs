use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::select;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{remove_session_slot, SessionState, State};
use crate::report;
use crate::session::Session;
use vpnhotspotd::shared::model::SessionConfig;
use vpnhotspotd::shared::proto::daemon;

pub(super) struct SessionControl {
    commands: UnboundedSender<SessionCommand>,
}

pub(super) enum SessionCommand {
    Replace {
        config: SessionConfig,
        reply: oneshot::Sender<io::Result<()>>,
    },
    ReadCounters {
        reply: oneshot::Sender<SessionCounterSnapshot>,
    },
    Stop {
        withdraw_cleanup: bool,
        reply: oneshot::Sender<()>,
    },
}

pub(super) struct SessionCounterSnapshot {
    pub(super) config: SessionConfig,
    pub(super) counters: Vec<daemon::TrafficCounter>,
}

pub(super) struct PendingSessionReply<T> {
    reply: oneshot::Receiver<T>,
}

pub(super) fn channel() -> (SessionControl, UnboundedReceiver<SessionCommand>) {
    let (commands, receiver) = mpsc::unbounded_channel();
    (SessionControl { commands }, receiver)
}

pub(super) async fn stop_sessions(sessions: &[Arc<SessionState>], withdraw_cleanup: bool) {
    for session in sessions {
        stop_session(session, withdraw_cleanup).await;
    }
}

pub(super) async fn read_session_counters(
    slot: &Arc<SessionState>,
) -> Option<SessionCounterSnapshot> {
    let pending = {
        let guard = slot.control.lock().await;
        guard.as_ref()?.read_counters().ok()?
    };
    pending.receive().await.ok()
}

pub(super) async fn run_session(
    state: &Arc<State>,
    slot: Arc<SessionState>,
    session: Session,
    mut commands: UnboundedReceiver<SessionCommand>,
    cancel: &CancellationToken,
) {
    let mut session = Some(session);
    let mut waiting_for_clean_stop = false;
    let mut remove_slot_on_exit = false;
    loop {
        select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    remove_slot_on_exit = !slot.cleaning.load(Ordering::Acquire);
                    break;
                };
                if handle_session_command(&mut session, command).await {
                    break;
                }
            }
            _ = cancel.cancelled(), if !waiting_for_clean_stop => {
                if slot.cleaning.load(Ordering::Acquire) {
                    waiting_for_clean_stop = true;
                    continue;
                }
                *slot.control.lock().await = None;
                while let Ok(command) = commands.try_recv() {
                    if handle_session_command(&mut session, command).await {
                        break;
                    }
                }
                if let Some(session) = session.take() {
                    session.stop(false).await;
                }
                remove_session_slot(state, &slot).await;
                break;
            }
        }
    }
    if let Some(session) = session {
        session.stop(false).await;
    }
    if remove_slot_on_exit {
        remove_session_slot(state, &slot).await;
    }
}

impl SessionControl {
    pub(super) fn replace_config(
        &self,
        config: SessionConfig,
    ) -> io::Result<PendingSessionReply<io::Result<()>>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(SessionCommand::Replace { config, reply })
            .map_err(|_| session_closed_error())?;
        Ok(PendingSessionReply { reply: receiver })
    }

    fn read_counters(&self) -> io::Result<PendingSessionReply<SessionCounterSnapshot>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(SessionCommand::ReadCounters { reply })
            .map_err(|_| session_closed_error())?;
        Ok(PendingSessionReply { reply: receiver })
    }

    fn stop(&self, withdraw_cleanup: bool) -> io::Result<PendingSessionReply<()>> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(SessionCommand::Stop {
                withdraw_cleanup,
                reply,
            })
            .map_err(|_| session_closed_error())?;
        Ok(PendingSessionReply { reply: receiver })
    }
}

impl<T> PendingSessionReply<T> {
    pub(super) async fn receive(self) -> io::Result<T> {
        self.reply.await.map_err(|_| session_closed_error())
    }
}

async fn stop_session(slot: &Arc<SessionState>, withdraw_cleanup: bool) {
    let pending = {
        let mut guard = slot.control.lock().await;
        let Some(control) = guard.take() else {
            return;
        };
        control.stop(withdraw_cleanup).ok()
    };
    if let Some(pending) = pending {
        if let Err(e) = pending.receive().await {
            report::io("control.stop_session", e);
        }
    }
}

async fn handle_session_command(session: &mut Option<Session>, command: SessionCommand) -> bool {
    match command {
        SessionCommand::Replace { config, reply } => {
            let result = match session.as_mut() {
                Some(session) => session.replace_config(config).await,
                None => Err(session_closed_error()),
            };
            let _ = reply.send(result);
            false
        }
        SessionCommand::ReadCounters { reply } => {
            if let Some(session) = session.as_ref() {
                let _ = reply.send(SessionCounterSnapshot {
                    config: session.config_snapshot().await,
                    counters: session.traffic_counters().await,
                });
            }
            false
        }
        SessionCommand::Stop {
            withdraw_cleanup,
            reply,
        } => {
            if let Some(session) = session.take() {
                session.stop(withdraw_cleanup).await;
            }
            let _ = reply.send(());
            true
        }
    }
}

fn session_closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "session closed")
}
