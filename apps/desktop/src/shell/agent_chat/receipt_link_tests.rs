//! The chat's receipts and retry counts, written through to a real host core.

use std::sync::Arc;

use gpui::TestAppContext;
use oximux_agents::send_receipts::{KeyKind, ReceiptState, key_hash};
use oximux_agents::thread::StubConnection;
use oximux_settings::agent_retry::AgentRetrySettings;
use oximux_settings::{Density, Theme, Typography};

use super::super::*;
use crate::persisted_chat::PersistedRetry;
use crate::remote_control::RemoteControl;

/// A chat whose app has a host core on an in-memory database.
fn chat_with_host(cx: &mut TestAppContext) -> (gpui::WindowHandle<AgentChatView>, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let db = oximux_storage::db::open_memory().unwrap();
    let lineage = oximux_agents::lineage::LineageStore::new(db.conn());
    let handle = rt.handle().clone();
    cx.update(gpui_component::init);
    cx.update(|cx| {
        let mut rc = RemoteControl::new();
        rc.install_host_core(handle, lineage, oximux_remote_host::HostConfig::default());
        cx.set_global(rc);
        cx.set_global(AgentRetrySettings::shipped());
    });
    let window = cx.add_window(|window, cx| {
        AgentChatView::with_connection_for_test(
            Arc::new(StubConnection::default()),
            Theme::default(),
            Density::default(),
            Typography::default(),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    (window, rt)
}

#[gpui::test]
async fn a_send_is_reserved_and_settled_by_its_acceptance(cx: &mut TestAppContext) {
    let (window, _rt) = chat_with_host(cx);
    let (session, id) = window
        .update(cx, |view, _window, cx| {
            view.send_text("hello".into(), Vec::new(), cx);
            (view.remote_session_id.clone(), view.health.sent_id().expect("tagged"))
        })
        .unwrap();
    cx.run_until_parked();
    let receipts = cx.update(|cx| cx.global::<RemoteControl>().host_core().unwrap().lineage().receipts());
    let key = key_hash(KeyKind::Gate, &id);
    assert_eq!(receipts.state(&session, &key).unwrap(), Some(ReceiptState::Pending));
    window
        .update(cx, |view, _window, cx| view.apply_batch(vec![ThreadEvent::UserMessageAccepted { uuid: id.clone() }], cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(receipts.state(&session, &key).unwrap(), Some(ReceiptState::Accepted));
}

/// Restart mid-retry: the host's count carries on even when the blob's is
/// behind (B6).
#[gpui::test]
async fn a_restored_retry_keeps_the_durable_attempt_count(cx: &mut TestAppContext) {
    let (window, _rt) = chat_with_host(cx);
    let session = window.update(cx, |view, _window, _cx| view.remote_session_id.clone()).unwrap();
    let retries = cx.update(|cx| cx.global::<RemoteControl>().host_core().unwrap().lineage().retries());
    retries.arm(&session, "turn-k", "window", "Usage limit reached", 1, "h", chrono::Local::now()).unwrap();
    retries.fired(&session, "turn-k").unwrap();
    retries.fired(&session, "turn-k").unwrap();
    window
        .update(cx, |view, _window, cx| {
            let saved = PersistedRetry {
                wake_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
                reason: "Usage limit reached".into(),
                attempt: 0,
            };
            view.restore_pending_retry(saved, cx);
            assert_eq!(view.retry.attempt, 2);
            assert_eq!(view.retry.turn_key.as_deref(), Some("turn-k"));
        })
        .unwrap();
}

/// A send the host recorded as taken is not offered back after a restart.
#[gpui::test]
async fn an_accepted_send_is_not_offered_back(cx: &mut TestAppContext) {
    let (window, _rt) = chat_with_host(cx);
    let session = window.update(cx, |view, _window, _cx| view.remote_session_id.clone()).unwrap();
    let receipts = cx.update(|cx| cx.global::<RemoteControl>().host_core().unwrap().lineage().receipts());
    let now = chrono::Local::now();
    receipts.settle(&session, &key_hash(KeyKind::Gate, "u-taken"), ReceiptState::Accepted, "h", now).unwrap();
    window
        .update(cx, |view, _window, cx| {
            view.thread.push_user_message_with_images("ship it".to_string(), Vec::new());
            view.restore_health(RestoredTurn { unanswered_send: Some("u-taken".into()), ..Default::default() }, cx);
            assert!(view.composer.read(cx).held_for_test().is_empty());
            view.restore_health(RestoredTurn { unanswered_send: Some("u-other".into()), ..Default::default() }, cx);
            assert_eq!(view.composer.read(cx).held_for_test().len(), 1, "no record: still offered");
        })
        .unwrap();
}

/// The CLI acknowledges a line before the provider answers; an overload with
/// no output then marks the send refused, safe to resend (either write order).
#[gpui::test]
async fn a_refused_send_ends_rejected_after_the_clis_acknowledgement(cx: &mut TestAppContext) {
    let (window, _rt) = chat_with_host(cx);
    let (session, id) = window
        .update(cx, |view, _window, cx| {
            view.send_text("hello".into(), Vec::new(), cx);
            let id = view.health.sent_id().expect("tagged");
            view.apply_batch(vec![ThreadEvent::UserMessageAccepted { uuid: id.clone() }], cx);
            view.thread.turn_active = false;
            view.thread.last_turn_failure = Some((Some(529), None));
            view.thread.last_error = Some("API Error: 529".into());
            view.arm_retry_if_limited(cx);
            assert!(view.retry.is_armed(), "an overload is retried");
            (view.remote_session_id.clone(), id)
        })
        .unwrap();
    cx.run_until_parked();
    let receipts = cx.update(|cx| cx.global::<RemoteControl>().host_core().unwrap().lineage().receipts());
    assert_eq!(receipts.state(&session, &key_hash(KeyKind::Gate, &id)).unwrap(), Some(ReceiptState::Rejected));
    let live = cx.update(|cx| cx.global::<RemoteControl>().host_core().unwrap().lineage().retries().live(&session).unwrap());
    assert_eq!(live.map(|l| l.attempts), Some(0), "armed, not yet fired");
}
