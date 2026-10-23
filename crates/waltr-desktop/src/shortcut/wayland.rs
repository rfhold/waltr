//! Wayland `GlobalShortcuts` portal only. No interception or X11 fallback.
//! Retain and poll `run`; cancel then await it to close the owned session.
use super::{Effective, Error, Router, portal_trigger};
use ashpd::desktop::{
    CreateSessionOptions,
    global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut, Shortcut},
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

fn effective(
    shortcuts: &[Shortcut],
    requested: &[(String, String)],
) -> Result<Vec<Effective>, Error> {
    if shortcuts.len() > 64 {
        return Err(Error::Invalid);
    }
    shortcuts
        .iter()
        .map(|s| {
            if !requested.iter().any(|(id, _)| id == s.id()) || s.trigger_description().len() > 256
            {
                return Err(Error::Invalid);
            }
            Ok(Effective {
                id: s.id().into(),
                trigger: s.trigger_description().into(),
            })
        })
        .collect()
}
/// Create one portal session and bind exactly once. User-selected subsets and overrides win.
/// Hosts must continuously poll this future alongside their event receiver.
/// # Errors
/// Returns explicit unsupported/permission/session/queue failures. No retry occurs.
pub async fn run(
    bindings: &[(String, String)],
    router: Router,
    stop: &CancellationToken,
) -> Result<(), Error> {
    let result = execute(bindings, router.clone(), stop).await;
    router.fail(result.err().unwrap_or(Error::Backend));
    result
}
async fn execute(
    bindings: &[(String, String)],
    router: Router,
    stop: &CancellationToken,
) -> Result<(), Error> {
    if stop.is_cancelled() {
        return Ok(());
    }
    if bindings.len() > 64 {
        router.fail(Error::Invalid);
        return Err(Error::Invalid);
    }
    let requested: Vec<_> = bindings
        .iter()
        .map(|(id, chord)| {
            let trigger = portal_trigger(chord)?;
            if id.is_empty() || id.len() > 256 {
                return Err(Error::Invalid);
            }
            Ok(NewShortcut::new(id, id).preferred_trigger(trigger.as_str()))
        })
        .collect::<Result<_, Error>>()?;
    let portal = GlobalShortcuts::new().await.map_err(|_| Error::Backend)?;
    let session = portal
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|_| Error::Backend)?;
    let result = async {
        // Session serializes to its exact D-Bus object path (ashpd keeps path() private).
        let session_path = serde_json::to_value(&session).map_err(|_| Error::Backend)?;
        let path = session_path.as_str().ok_or(Error::Backend)?;
        let activated = portal
            .receive_activated()
            .await
            .map_err(|_| Error::Backend)?;
        let deactivated = portal
            .receive_deactivated()
            .await
            .map_err(|_| Error::Backend)?;
        let changed = portal
            .receive_shortcuts_changed()
            .await
            .map_err(|_| Error::Backend)?;
        let closed = session.receive_closed().await.map_err(|_| Error::Backend)?;
        futures_util::pin_mut!(activated, deactivated, changed, closed);
        let response = portal
            .bind_shortcuts(&session, &requested, None, BindShortcutsOptions::default())
            .await
            .map_err(|_| Error::Backend)?
            .response()
            .map_err(|_| Error::Backend)?;
        router.effective(effective(response.shortcuts(), bindings)?)?;
        loop {
            tokio::select! { biased;
                () = stop.cancelled() => return Ok(()),
                _event = closed.next() => return Err(Error::Backend),
                event = activated.next() => {
                    let event = event.ok_or(Error::Backend)?;
                    router.edge(path, event.session_handle().as_str(), event.shortcut_id(), true);
                }
                event = deactivated.next() => {
                    let event = event.ok_or(Error::Backend)?;
                    router.edge(path, event.session_handle().as_str(), event.shortcut_id(), false);
                }
                event = changed.next() => {
                    let event = event.ok_or(Error::Backend)?;
                    if event.session_handle().as_str() == path { router.effective(effective(event.shortcuts(), bindings)?)?; }
                }
            }
            if router.failure_token().is_cancelled() {
                return Err(Error::Overflow);
            }
        }
    };
    let result =
        tokio::select! { biased; () = stop.cancelled() => Ok(()), result = result => result };
    let close = session.close().await.map_err(|_| Error::Backend);
    router.fail(result.err().unwrap_or(Error::Backend));
    result.and(close)
}
