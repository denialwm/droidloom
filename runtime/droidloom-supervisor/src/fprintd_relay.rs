//! fprintd's matches, passed to Android's authentication service. Denial runs
//! the reader for Android's fingerprint requests, but Denial runs as the host
//! user, who is also Android's system user: the service counts a match only
//! once it hears it from here. fprintd announces each result on the system
//! bus, which stamps every message with its real sender, and only root may
//! own fprintd's name.
use std::fs;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;

use async_io::Async;
use futures_lite::{StreamExt, future};
use zbus::fdo::DBusProxy;
use zbus::message::Type;
use zbus::names::BusName;
use zbus::{Connection, MatchRule, MessageStream};

use super::host_lock::{self, RETRY, Watch};
use super::{android_init_pid, peer_credentials};
use crate::CellSpec;

const FPRINTD: &str = "net.reactivated.Fprint";
/// The authentication service's socket in the cell, root's alone.
const SERVICE: &str = "dev/socket/droidloom-authentication";
const MATCH: u8 = 1;

pub(super) fn start(spec: &CellSpec) -> Option<Watch> {
    let spec = spec.clone();
    host_lock::spawn("fprintd-relay", move |stop| async move {
        run(&spec, &stop).await;
    })
}

async fn run(spec: &CellSpec, stop: &Async<OwnedFd>) {
    let mut service = None;
    loop {
        let relayed = future::or(
            async {
                let _ = stop.readable().await;
                Ok(())
            },
            relay(spec, &mut service),
        )
        .await;
        match relayed {
            Ok(()) => return,
            Err(error) => eprintln!("fprintd relay: {error}"),
        }
        let stopped = future::or(
            async {
                let _ = stop.readable().await;
                true
            },
            async {
                async_io::Timer::after(RETRY).await;
                false
            },
        )
        .await;
        if stopped {
            return;
        }
    }
}

/// Passes on fprintd's matches until the bus connection fails.
async fn relay(spec: &CellSpec, service: &mut Option<UnixStream>) -> Result<(), String> {
    let error = |error: zbus::Error| error.to_string();
    let connection = Connection::system().await.map_err(error)?;
    let bus = DBusProxy::new(&connection).await.map_err(error)?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(FPRINTD)
        .map_err(error)?
        .interface("net.reactivated.Fprint.Device")
        .map_err(error)?
        .member("VerifyStatus")
        .map_err(error)?
        .build();
    let mut results = MessageStream::for_match_rule(rule, &connection, None)
        .await
        .map_err(error)?;
    while let Some(message) = results.next().await {
        let message = message.map_err(error)?;
        let Ok((result, _done)) = message.body().deserialize::<(String, bool)>() else {
            continue;
        };
        if result != "verify-match" {
            continue;
        }
        let owner = bus
            .get_name_owner(BusName::try_from(FPRINTD).map_err(|e| e.to_string())?)
            .await
            .ok();
        let header = message.header();
        let sender = header.sender().map(zbus::names::UniqueName::as_str);
        if sender.is_none() || sender != owner.as_deref().map(zbus::names::UniqueName::as_str) {
            continue;
        }
        send(spec, service);
    }
    Err("the system bus closed".into())
}

/// One byte per match. A service that restarted gets a new connection; one
/// that is not there misses the match, as Android would with no sensor.
fn send(spec: &CellSpec, service: &mut Option<UnixStream>) {
    for _ in 0..2 {
        if service.is_none() {
            *service = connect(spec);
        }
        let Some(stream) = service.as_mut() else {
            return;
        };
        if stream.write_all(&[MATCH]).is_ok() {
            return;
        }
        *service = None;
    }
}

fn connect(spec: &CellSpec) -> Option<UnixStream> {
    let init = android_init_pid(spec).ok().flatten()?;
    let path = format!("/proc/{init}/root/{SERVICE}");
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_socket() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        eprintln!("fprintd relay: {SERVICE} has unsafe ownership or mode");
        return None;
    }
    let stream = UnixStream::connect(&path).ok()?;
    (peer_credentials(&stream).ok()?.uid == 0).then_some(stream)
}
