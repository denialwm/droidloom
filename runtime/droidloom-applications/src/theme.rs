//! Android follows the host's dark or light theme. The desktop portal's
//! `org.freedesktop.appearance` `color-scheme` is 1 when the user prefers
//! dark; Android's night mode is set to match at start and on each change.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use async_io::Timer;
use droidloom_supervisor::control::{ControlRequest, request};
use futures_lite::{StreamExt, future};
use zbus::message::Type;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";
/// Wait before trying again while the session bus, the portal or Android is
/// not there.
const RETRY: Duration = Duration::from_secs(2);

/// Follows the theme on its own thread for the life of the process.
pub fn follow(socket: PathBuf) {
    let spawned = thread::Builder::new()
        .name("droidloom-theme".into())
        .spawn(move || {
            future::block_on(async {
                loop {
                    if let Err(error) = watch(&socket).await {
                        eprintln!("droidloom-applications: theme follow deferred: {error}");
                    }
                    Timer::after(RETRY).await;
                }
            });
        });
    if let Err(error) = spawned {
        eprintln!("droidloom-applications: theme follow unavailable: {error}");
    }
}

async fn watch(socket: &Path) -> zbus::Result<()> {
    let bus = Connection::session().await?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(SETTINGS)?
        .member("SettingChanged")?
        .path(PATH)?
        .build();
    let mut changes = MessageStream::for_match_rule(rule, &bus, Some(16)).await?;
    let portal = Proxy::new(&bus, PORTAL, PATH, SETTINGS).await?;
    let scheme: OwnedValue = portal.call("ReadOne", &(NAMESPACE, KEY)).await?;
    let mut pending = Some(is_dark(&scheme));
    let mut reported = false;
    loop {
        if let Some(night) = pending {
            match request(socket, &ControlRequest::SetNightMode { night }) {
                Ok(response) if response.ok => {
                    pending = None;
                    reported = false;
                }
                // Android is stopped or still booting: once in the log, then quietly.
                Ok(response) if !reported => {
                    eprintln!("droidloom-applications: theme deferred: {}", response.message);
                    reported = true;
                }
                Err(error) if !reported => {
                    eprintln!("droidloom-applications: theme deferred: {error}");
                    reported = true;
                }
                _ => {}
            }
        }
        let next = async { Some(changes.next().await) };
        let message = if pending.is_some() {
            future::or(next, async {
                Timer::after(RETRY).await;
                None
            })
            .await
        } else {
            next.await
        };
        let Some(message) = message else { continue };
        let message = message.ok_or_else(|| zbus::Error::Failure("session bus closed".into()))??;
        let (namespace, key, value): (String, String, OwnedValue) = message.body().deserialize()?;
        if namespace == NAMESPACE && key == KEY {
            pending = Some(is_dark(&value));
        }
    }
}

fn is_dark(scheme: &Value<'_>) -> bool {
    matches!(scheme, Value::U32(1))
}

#[cfg(test)]
mod tests {
    use super::is_dark;
    use zbus::zvariant::Value;

    #[test]
    fn only_prefer_dark_is_night() {
        assert!(is_dark(&Value::U32(1)));
        assert!(!is_dark(&Value::U32(0)));
        assert!(!is_dark(&Value::U32(2)));
        assert!(!is_dark(&Value::from("dark")));
    }
}
