//! TUI adapters for shared native OIDC access coordination.

use std::{env, io, process::Stdio, time::Duration};

use waltr_client::{
    AccessAttemptId, NativeAccessAction, NativeAccessConfig, NativeAccessCoordinator,
    NativeAccessOutput, NativeAccessProgress, NativeCallbackPolicy, NativeCredentialProvider,
    NativeLoopbackPolicy,
};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc, task::JoinHandle};

use crate::{
    event::{AppEvent, NativeLoginEvent},
    runtime::RuntimeClientCommand,
};

const CLIENT_ID_ENV: &str = "WALTR_OIDC_NATIVE_CLIENT_ID";
const SCOPES_ENV: &str = "WALTR_OIDC_NATIVE_SCOPES";
const AUDIENCE_ENV: &str = "WALTR_OIDC_NATIVE_AUDIENCE";
const LOOPBACK_POLICY_ENV: &str = "WALTR_OIDC_NATIVE_LOOPBACK_POLICY";

/// Native side effect requested by terminal input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeAuthAction {
    /// Begin or retry Waltr login.
    Start,
    /// Cancel the active login.
    Cancel,
    /// Copy the currently displayed authorization URL.
    CopyUrl(String),
    /// Remove in-memory Waltr credentials.
    Logout,
    /// Shared client accepted a login start and assigned its attempt ID.
    AttemptStarted {
        /// Reducer-issued generation that must accompany terminal commands.
        attempt_id: AccessAttemptId,
    },
}

/// Spawn the TUI adapter around the shared native access coordinator.
#[must_use]
pub(crate) fn spawn(
    events: mpsc::Sender<io::Result<AppEvent>>,
    commands: mpsc::Sender<RuntimeClientCommand>,
    mut actions: mpsc::Receiver<NativeAuthAction>,
    endpoint: String,
    credentials: NativeCredentialProvider,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (config, config_error) = config_from_env(endpoint);
        let mut coordinator = NativeAccessCoordinator::spawn(config, credentials, 8);
        let shared_actions = coordinator.action_sender();
        let mut browser_task = None;

        loop {
            tokio::select! {
                action = actions.recv() => {
                    let Some(action) = action else { break };
                    match action {
                        NativeAuthAction::Start => {
                            if shared_actions.send(NativeAccessAction::Start).await.is_err() { break; }
                        }
                        NativeAuthAction::Cancel => {
                            abort_task(&mut browser_task).await;
                            if shared_actions.send(NativeAccessAction::Cancel).await.is_err() { break; }
                        }
                        NativeAuthAction::Logout => {
                            abort_task(&mut browser_task).await;
                            if shared_actions.send(NativeAccessAction::Logout).await.is_err() { break; }
                        }
                        NativeAuthAction::AttemptStarted { attempt_id } => {
                            if shared_actions.send(NativeAccessAction::AttemptStarted { attempt_id }).await.is_err() { break; }
                        }
                        NativeAuthAction::CopyUrl(url) => {
                            let copied = copy_to_clipboard(&url).await;
                            send_event(&events, NativeLoginEvent::UrlCopyFinished { copied }).await;
                        }
                    }
                }
                output = coordinator.next_output() => {
                    let Some(output) = output else { break };
                    match output {
                        NativeAccessOutput::ClientCommand { command, logout_barrier } => {
                            let command = if logout_barrier {
                                RuntimeClientCommand::logout()
                            } else {
                                RuntimeClientCommand::dispatch(*command)
                            };
                            if commands.send(command).await.is_err() { break; }
                        }
                        NativeAccessOutput::Progress(progress) => {
                            abort_task(&mut browser_task).await;
                            browser_task = publish_progress(&events, progress, config_error.as_deref()).await;
                        }
                    }
                }
            }
        }

        abort_task(&mut browser_task).await;
        coordinator.stop().await;
    })
}

fn config_from_env(endpoint: String) -> (NativeAccessConfig, Option<String>) {
    let mut error = None;
    let client_id = env::var(CLIENT_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            error = Some(format!("{CLIENT_ID_ENV} is required"));
            String::new()
        });
    let scopes = env::var(SCOPES_ENV)
        .ok()
        .map(|value| value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let audience = env::var(AUDIENCE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let loopback = match env::var(LOOPBACK_POLICY_ENV) {
        Ok(value) if value != "ephemeral" => {
            if let Some(message) = loopback_url_error(&value) {
                error = Some(format!("{LOOPBACK_POLICY_ENV} {message}"));
            }
            NativeLoopbackPolicy::Fixed(value)
        }
        Ok(_) | Err(env::VarError::NotPresent) => NativeLoopbackPolicy::Ephemeral,
        Err(env_error) => {
            error = Some(format!("invalid {LOOPBACK_POLICY_ENV}: {env_error}"));
            NativeLoopbackPolicy::Ephemeral
        }
    };
    (
        NativeAccessConfig {
            endpoint,
            client_id,
            scopes,
            audience,
            callback: NativeCallbackPolicy::Loopback(loopback),
        },
        error,
    )
}

fn loopback_url_error(value: &str) -> Option<&'static str> {
    let Ok(url) = url::Url::parse(value) else {
        return Some("must be ephemeral or a loopback URL");
    };
    (url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.query().is_some()
        || url.fragment().is_some())
    .then_some("must use http://127.0.0.1:<port> without query or fragment")
}

async fn publish_progress(
    events: &mpsc::Sender<io::Result<AppEvent>>,
    progress: NativeAccessProgress,
    config_error: Option<&str>,
) -> Option<JoinHandle<()>> {
    let mut browser_task = None;
    let event = match progress {
        NativeAccessProgress::Discovering => NativeLoginEvent::Discovering,
        NativeAccessProgress::AwaitingBrowser {
            authorization_url,
            timeout_millis,
        } => {
            let browser = open_browser(&authorization_url);
            let browser_opened = browser.is_some();
            if let Some(mut browser) = browser {
                let events = events.clone();
                browser_task = Some(tokio::spawn(async move {
                    if !matches!(browser.wait().await, Ok(status) if status.success()) {
                        send_event(&events, NativeLoginEvent::BrowserLaunchFailed).await;
                    }
                }));
            }
            NativeLoginEvent::AwaitingBrowser {
                authorization_url,
                expires_at: std::time::Instant::now() + Duration::from_millis(timeout_millis),
                browser_opened,
            }
        }
        NativeAccessProgress::Exchanging => NativeLoginEvent::Exchanging,
        NativeAccessProgress::Refreshing => NativeLoginEvent::Refreshing,
        NativeAccessProgress::RefreshSucceeded => NativeLoginEvent::RefreshSucceeded,
        NativeAccessProgress::Warning { .. } => return None,
        NativeAccessProgress::Succeeded => NativeLoginEvent::Succeeded,
        NativeAccessProgress::Failed { error } => NativeLoginEvent::Failed {
            error: config_error.unwrap_or(&error).to_owned(),
        },
        NativeAccessProgress::Canceled => NativeLoginEvent::Canceled,
    };
    send_event(events, event).await;
    browser_task
}

async fn abort_task(task: &mut Option<JoinHandle<()>>) {
    if let Some(task) = task.take() {
        task.abort();
        drop(task.await);
    }
}

fn open_browser(url: &str) -> Option<tokio::process::Child> {
    let command = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("rundll32", vec!["url.dll,FileProtocolHandler", url])
    } else {
        ("xdg-open", vec![url])
    };
    let mut process = Command::new(command.0);
    process
        .args(command.1)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process.spawn().ok()
}

async fn copy_to_clipboard(value: &str) -> bool {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    for (program, args) in candidates {
        let Ok(mut child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take()
            && stdin.write_all(value.as_bytes()).await.is_ok()
        {
            drop(stdin);
            if child.wait().await.is_ok_and(|status| status.success()) {
                return true;
            }
        }
    }
    false
}

async fn send_event(events: &mpsc::Sender<io::Result<AppEvent>>, event: NativeLoginEvent) {
    drop(events.send(Ok(AppEvent::NativeLogin(event))).await);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_policy_accepts_only_fixed_ipv4_loopback_callbacks() {
        assert_eq!(
            loopback_url_error("not a URL"),
            Some("must be ephemeral or a loopback URL")
        );
        assert_eq!(
            loopback_url_error("http://localhost:9000/callback"),
            Some("must use http://127.0.0.1:<port> without query or fragment")
        );
        assert_eq!(loopback_url_error("http://127.0.0.1:9000/callback"), None);
    }

    #[tokio::test]
    async fn persistence_warning_is_ignored_by_intentionally_nonpersistent_tui() {
        let (events, mut event_receiver) = mpsc::channel(1);

        let task = publish_progress(
            &events,
            NativeAccessProgress::Warning {
                warning: "must-not-be-presented".to_owned(),
            },
            None,
        )
        .await;

        assert!(task.is_none());
        assert!(event_receiver.try_recv().is_err());
    }
}
