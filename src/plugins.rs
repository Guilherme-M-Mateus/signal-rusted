//! Plugins: programs in the data dir's `plugins/` folder, any language, enabled one by one in
//! Settings → Plugins. Each runs as a child process speaking JSON lines (see docs/plugins.md):
//! chat events on its stdin, actions on its stdout. A plugin runs with your user's rights.

use std::path::{Path, PathBuf};

use futures::{SinkExt, Stream};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
}

/// What a plugin asks the app to do.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Action {
    /// Sends `body` to an existing chat (`chat` is the key events carry).
    Send { chat: String, body: String },
    /// Shows a desktop notification.
    Notify { title: String, #[serde(default)] body: String },
}

#[derive(Clone, Debug)]
pub enum Update {
    /// Started: send events (one JSON object per line) here.
    Ready(UnboundedSender<String>),
    Action(Action),
    /// A stdout line that isn't a valid action.
    Invalid(String),
    Exited(String),
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

/// Executable files in `dir`, by name.
pub fn discover(dir: &Path) -> Vec<Plugin> {
    let mut found: Vec<Plugin> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|f| f.metadata().is_ok_and(|m| m.is_file() && is_executable(&m)))
        .map(|f| Plugin { name: f.file_name().to_string_lossy().into_owned(), path: f.path() })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

#[cfg(unix)]
fn is_executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    true
}

/// Runs the plugin until its stream is dropped (disabling it kills the process).
pub fn run(p: &Plugin) -> impl Stream<Item = Update> + use<> {
    let p = p.clone();
    iced::stream::channel(64, async move |mut out: futures::channel::mpsc::Sender<Update>| {
        let child = tokio::process::Command::new(&p.path)
            .current_dir(p.path.parent().unwrap_or(Path::new(".")))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return drop(out.send(Update::Exited(format!("couldn't start: {e}"))).await),
        };
        let (mut stdin, stdout) = (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"));
        let (tx, mut rx) = unbounded_channel::<String>();
        let _ = out.send(Update::Ready(tx)).await;
        let mut lines = BufReader::new(stdout).lines();
        let status = loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) if l.trim().is_empty() => {}
                    Ok(Some(l)) => {
                        let u = serde_json::from_str(&l).map(Update::Action).unwrap_or(Update::Invalid(l));
                        let _ = out.send(u).await;
                    }
                    _ => break child.wait().await.map_or_else(|e| e.to_string(), |s| s.to_string()),
                },
                Some(event) = rx.recv() => {
                    // A plugin that stops reading isn't fatal; its exit is reported once stdout closes.
                    let _ = stdin.write_all(format!("{event}\n").as_bytes()).await;
                    let _ = stdin.flush().await;
                }
            }
        };
        let _ = out.send(Update::Exited(status)).await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions() {
        assert_eq!(
            serde_json::from_str::<Action>(r#"{"type":"send","chat":"abc","body":"hi"}"#).unwrap(),
            Action::Send { chat: "abc".into(), body: "hi".into() }
        );
        assert_eq!(
            serde_json::from_str::<Action>(r#"{"type":"notify","title":"t"}"#).unwrap(),
            Action::Notify { title: "t".into(), body: String::new() }
        );
        assert!(serde_json::from_str::<Action>(r#"{"type":"rm","path":"/"}"#).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn discovers_executables_and_talks_json_lines() {
        use futures::StreamExt;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("echo");
        // Answers each event with a notification carrying its body, then exits on "quit".
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile read -r line; do\n  case \"$line\" in *quit*) exit 0;; esac\n  echo '{\"type\":\"notify\",\"title\":\"got\",\"body\":\"x\"}'\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a plugin").unwrap();
        let found = discover(dir.path());
        assert_eq!(found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["echo"]);

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut s = Box::pin(run(&found[0]));
            let Some(Update::Ready(tx)) = s.next().await else { panic!("not ready") };
            tx.send(r#"{"type":"message","body":"hi"}"#.into()).unwrap();
            assert!(matches!(s.next().await, Some(Update::Action(Action::Notify { title, .. })) if title == "got"));
            tx.send("quit".into()).unwrap();
            assert!(matches!(s.next().await, Some(Update::Exited(_))));
        });
    }
}
