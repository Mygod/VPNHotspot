use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::State;
use crate::report;
use vpnhotspotd::shared::proto::daemon;
use vpnhotspotd::shared::protocol::{complete_frame, daemon_io_error_report, error_frame};

pub(super) struct CallState {
    pub(super) cancel: CancellationToken,
}

pub(super) enum CallOutput {
    Reply(Vec<u8>),
    NoFrame,
}

pub(super) async fn handle_call(
    id: u64,
    command: daemon::client_envelope::Command,
    state: Arc<State>,
    sender: UnboundedSender<Vec<u8>>,
    active_calls: Arc<Mutex<HashMap<u64, Arc<CallState>>>>,
    call: Arc<CallState>,
) {
    match super::handle_command(
        id,
        command,
        state,
        &sender,
        active_calls.clone(),
        call.cancel.clone(),
    )
    .await
    {
        Ok(CallOutput::Reply(packet)) => {
            send_terminal_frame(id, &active_calls, &call, &sender, packet).await;
        }
        Ok(CallOutput::NoFrame) => {
            remove_call(id, &active_calls, &call).await;
        }
        Err(e) => {
            let report = daemon_io_error_report("control.handle_call", e);
            if call.cancel.is_cancelled() {
                if remove_call(id, &active_calls, &call).await {
                    report::report_for(Some(id), report);
                }
            } else {
                send_terminal_frame(id, &active_calls, &call, &sender, error_frame(id, report))
                    .await;
            }
        }
    }
}

async fn send_terminal_frame(
    id: u64,
    active_calls: &Mutex<HashMap<u64, Arc<CallState>>>,
    call: &Arc<CallState>,
    sender: &UnboundedSender<Vec<u8>>,
    frame: Vec<u8>,
) {
    let cancelled = call.cancel.is_cancelled();
    if remove_call(id, active_calls, call).await && !cancelled && sender.send(frame).is_err() {
        report::stderr!("controller send failed");
    }
}

pub(super) async fn detach_call(
    id: u64,
    active_calls: &Mutex<HashMap<u64, Arc<CallState>>>,
) -> bool {
    let call = active_calls.lock().await.remove(&id);
    if let Some(call) = call {
        call.cancel.cancel();
        true
    } else {
        false
    }
}

pub(super) fn send_complete(id: u64, sender: &UnboundedSender<Vec<u8>>) {
    if sender.send(complete_frame(id)).is_err() {
        report::stderr!("controller send failed");
    }
}

async fn remove_call(
    id: u64,
    active_calls: &Mutex<HashMap<u64, Arc<CallState>>>,
    call: &Arc<CallState>,
) -> bool {
    let mut active = active_calls.lock().await;
    if active
        .get(&id)
        .is_some_and(|current| Arc::ptr_eq(current, call))
    {
        active.remove(&id);
        true
    } else {
        false
    }
}
