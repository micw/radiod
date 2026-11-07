//! Small, deliberately unauthenticated MPD compatibility endpoint for configured radios.
use crate::core::Core;
use anyhow::Result;
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

const COMMANDS: &str = "lsinfo listall listallinfo commands notcommands tagtypes urlhandlers decoders status stats currentsong playlistinfo playlistid plchanges plchangesposid listplaylists listplaylist listplaylistinfo load clear add addid delete deleteid move moveid swap swapid play playid pause stop next previous setvol volume repeat random single consume outputs enableoutput disableoutput ping close idle noidle command_list_begin command_list_ok_begin command_list_end";
const LIMIT: usize = 8192;

pub fn serve(listener: TcpListener, core: Arc<Mutex<Core>>) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Slot(Arc<AtomicUsize>);
    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let clients = Arc::new(AtomicUsize::new(0));
    for connection in listener.incoming() {
        let stream = connection?;
        if clients.fetch_add(1, Ordering::Relaxed) >= 32 {
            clients.fetch_sub(1, Ordering::Relaxed);
            drop(stream);
            continue;
        }
        let slot = Slot(Arc::clone(&clients));
        let core = Arc::clone(&core);
        std::thread::Builder::new()
            .name("mpd-client".into())
            .spawn(move || {
                let _slot = slot;
                let _ = client(stream, core);
            })?;
    }
    Ok(())
}
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn parse(line: &str) -> std::result::Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escape = false;
    let mut active = false;
    for c in line.chars() {
        if escape {
            word.push(c);
            escape = false;
            active = true;
            continue;
        }
        if c == '\\' {
            escape = true;
            active = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            active = true;
        } else if c.is_whitespace() {
            if active {
                words.push(std::mem::take(&mut word));
                active = false;
            }
        } else {
            word.push(c);
            active = true;
        }
    }
    if escape || quote.is_some() {
        return Err("unterminated quote or escape".into());
    }
    if active {
        words.push(word);
    }
    if words.is_empty() {
        return Err("empty command".into());
    }
    Ok(words)
}
fn ack(code: u8, index: usize, cmd: &str, error: &str) -> String {
    format!("ACK [{code}@{index}] {{{}}} {}\n", clean(cmd), clean(error))
}
// Keep incomplete lines across read timeouts; BufRead::read_line would lose them.
struct Lines {
    bytes: Vec<u8>,
}
impl Lines {
    fn next(&mut self, s: &mut TcpStream) -> io::Result<Option<String>> {
        loop {
            if let Some(n) = self.bytes.iter().position(|b| *b == b'\n') {
                if n > LIMIT {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                let mut line = self.bytes.drain(..=n).collect::<Vec<_>>();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return String::from_utf8(line)
                    .map(Some)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8"));
            }
            if self.bytes.len() > LIMIT {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            let mut buf = [0; 1024];
            match s.read(&mut buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.bytes.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }
}
fn client(mut socket: TcpStream, core: Arc<Mutex<Core>>) -> Result<()> {
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    socket.write_all(b"OK MPD 0.23.0\n")?;
    let mut input = Lines { bytes: Vec::new() };
    let mut list: Option<(bool, Vec<Vec<String>>)> = None;
    let mut idle: Option<(u64, Vec<String>)> = None;
    let mut observed = core
        .lock()
        .map_err(|_| anyhow::anyhow!("core poisoned"))?
        .revision;
    loop {
        let line = match input.next(&mut socket) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                socket.write_all(ack(2, 0, "", &e.to_string()).as_bytes())?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(line) = line {
            let args = match parse(&line) {
                Ok(a) => a,
                Err(e) => {
                    list = None;
                    socket.write_all(ack(2, 0, "", &e).as_bytes())?;
                    continue;
                }
            };
            let cmd = args[0].as_str();
            if idle.is_some() {
                if cmd == "noidle" && args.len() == 1 {
                    idle = None;
                    socket.write_all(b"OK\n")?;
                } else {
                    idle = None;
                    socket
                        .write_all(ack(5, 0, cmd, "only noidle allowed while idle").as_bytes())?;
                }
                continue;
            }
            if cmd == "command_list_end" {
                if args.len() != 1 {
                    list = None;
                    socket.write_all(ack(2, 0, cmd, "wrong arguments").as_bytes())?;
                    continue;
                }
                if let Some((verbose, commands)) = list.take() {
                    let mut failed = false;
                    for (i, a) in commands.iter().enumerate() {
                        let result = if matches!(a[0].as_str(), "idle" | "noidle" | "close") {
                            Err((5, "command not permitted in list".into()))
                        } else {
                            let mut c =
                                core.lock().map_err(|_| anyhow::anyhow!("core poisoned"))?;
                            let result = execute(&mut c, a);
                            observed = c.revision;
                            result
                        };
                        match result {
                            Ok(out) => {
                                socket.write_all(out.as_bytes())?;
                                if verbose {
                                    socket.write_all(b"list_OK\n")?;
                                }
                            }
                            Err((code, e)) => {
                                socket.write_all(ack(code, i, &a[0], &e).as_bytes())?;
                                failed = true;
                                break;
                            }
                        }
                    }
                    if !failed {
                        socket.write_all(b"OK\n")?;
                    }
                } else {
                    socket.write_all(ack(5, 0, cmd, "not in command list").as_bytes())?;
                }
                continue;
            }
            if let Some((_, commands)) = list.as_mut() {
                if commands.len() >= 256 {
                    list = None;
                    socket.write_all(ack(2, 256, cmd, "command list limit exceeded").as_bytes())?;
                } else {
                    commands.push(args);
                }
                continue;
            }
            if matches!(cmd, "command_list_begin" | "command_list_ok_begin") && args.len() == 1 {
                list = Some((cmd == "command_list_ok_begin", Vec::new()));
                continue;
            }
            if cmd == "close" && args.len() == 1 {
                return Ok(());
            }
            // An idle notification may race with cancellation. MPD ignores a
            // noidle received after idle already ended; no extra OK is emitted.
            if cmd == "noidle" && args.len() == 1 {
                continue;
            }
            if cmd == "idle" {
                let filters = args[1..].to_vec();
                if filters.iter().any(|s| {
                    !matches!(
                        s.as_str(),
                        "database"
                            | "update"
                            | "stored_playlist"
                            | "playlist"
                            | "player"
                            | "mixer"
                            | "output"
                            | "options"
                            | "partition"
                            | "sticker"
                            | "subscription"
                            | "message"
                            | "neighbor"
                            | "mount"
                    )
                }) {
                    socket.write_all(ack(2, 0, cmd, "unknown idle subsystem").as_bytes())?;
                } else {
                    idle = Some((observed, filters));
                }
                continue;
            }
            let result = {
                let mut c = core.lock().map_err(|_| anyhow::anyhow!("core poisoned"))?;
                let result = execute(&mut c, &args);
                observed = c.revision;
                result
            };
            match result {
                Ok(out) => {
                    socket.write_all(out.as_bytes())?;
                    socket.write_all(b"OK\n")?;
                }
                Err((code, e)) => socket.write_all(ack(code, 0, cmd, &e).as_bytes())?,
            }
        }
        if let Some((revision, filters)) = &idle {
            let latest = core
                .lock()
                .map_err(|_| anyhow::anyhow!("core poisoned"))?
                .revision;
            if idle_changed(*revision, latest) {
                let names = ["player", "mixer", "playlist", "options", "stored_playlist"];
                let selected: Vec<_> = names
                    .iter()
                    .filter(|s| filters.is_empty() || filters.iter().any(|f| f == **s))
                    .collect();
                if !selected.is_empty() {
                    let mut out = String::new();
                    for name in selected {
                        out.push_str(&format!("changed: {name}\n"));
                    }
                    out.push_str("OK\n");
                    socket.write_all(out.as_bytes())?;
                    observed = latest;
                    idle = None;
                }
            }
        }
    }
}
// Compare against the last delivered snapshot, never the revision at idle entry.
fn idle_changed(observed: u64, latest: u64) -> bool {
    observed != latest
}
type Reply = std::result::Result<String, (u8, String)>;
fn invalid(s: &str) -> (u8, String) {
    (2, s.into())
}
fn number(s: &str) -> std::result::Result<usize, (u8, String)> {
    s.parse().map_err(|_| invalid("invalid number"))
}
fn range(s: &str, len: usize) -> std::result::Result<std::ops::Range<usize>, (u8, String)> {
    let (a, b) = if let Some((a, b)) = s.split_once(':') {
        (number(a)?, if b.is_empty() { len } else { number(b)? })
    } else {
        let a = number(s)?;
        (a, a.saturating_add(1))
    };
    if a > b || b > len {
        return Err((50, "bad song range".into()));
    }
    Ok(a..b)
}
fn song(c: &Core, id: u32, pos: Option<usize>, metadata: bool) -> String {
    let Some(s) = c.song(id) else {
        return String::new();
    };
    let mut out = format!("file: {}\n", clean(&s.station.url));
    if metadata {
        out.push_str(&format!(
            "Name: {}\nTitle: {}\n",
            clean(&s.station.name),
            clean(if c.current == Some(id) {
                c.title.as_deref().unwrap_or(&s.station.name)
            } else {
                &s.station.name
            })
        ));
        if let Some(a) = c.artist.as_ref().filter(|_| c.current == Some(id)) {
            out.push_str(&format!("Artist: {}\n", clean(a)));
        }
    }
    if let Some(p) = pos {
        out.push_str(&format!("Pos: {p}\nId: {id}\n"));
    }
    out
}
fn execute(c: &mut Core, a: &[String]) -> Reply {
    let cmd = a[0].as_str();
    let p = &a[1..];
    let mut out = String::new();
    let bounds = match cmd {
        "commands" | "notcommands" | "tagtypes" | "urlhandlers" | "decoders" | "status"
        | "stats" | "currentsong" | "listplaylists" | "clear" | "stop" | "next" | "previous"
        | "outputs" | "ping" | "noidle" => (0, 0),
        "lsinfo" | "listall" | "listallinfo" | "playlistinfo" | "playlistid" | "play"
        | "playid" | "pause" => (0, 1),
        "addid" => (1, 2),
        "move" | "moveid" | "swap" | "swapid" => (2, 2),
        "load" => (1, 2),
        _ => (1, 1),
    };
    if !COMMANDS.split_whitespace().any(|s| s == cmd) {
        return Err((5, "unknown command".into()));
    }
    if p.len() < bounds.0 || p.len() > bounds.1 {
        return Err(invalid("wrong number of arguments"));
    }
    let mut queue_changed = false;
    match cmd {
        "lsinfo" | "listall" | "listallinfo" => {
            if !p.is_empty() && !matches!(p[0].as_str(), "" | "/" | "radio") {
                return Err((50, "no such directory".into()));
            }
            if cmd == "lsinfo" {
                out.push_str("playlist: radio\nLast-Modified: 1970-01-01T00:00:00Z\n");
            }
            for s in &c.songs {
                out.push_str(&song(c, s.id, None, cmd != "listall"));
            }
        }
        "commands" => {
            for name in COMMANDS.split_whitespace() {
                out.push_str(&format!("command: {name}\n"));
            }
        }
        "notcommands" | "noidle" | "ping" => {}
        "tagtypes" => out.push_str("tagtype: Name\ntagtype: Title\ntagtype: Artist\n"),
        "urlhandlers" => out.push_str("handler: http\nhandler: https\n"),
        "decoders" => out.push_str("plugin: radiod\nsuffix: mp3\nsuffix: aac\nsuffix: ogg\nmime_type: audio/mpeg\nmime_type: audio/aac\nmime_type: application/ogg\n"),
        "status" => {
            let state = match c.state.as_str() {
                "pause" => "pause",
                "stop" | "reconnecting" => "stop",
                _ => "play",
            };
            out = format!(
                "volume: {}\nrepeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nplaylist: {}\nplaylistlength: {}\nmixrampdb: 0.000000\nstate: {state}\nradiod_state: {}\n",
                c.volume,
                c.playlist_version,
                c.queue.len(),
                clean(&c.state)
            );
            if let Some((id, pos)) = c.current.and_then(|id| c.queue.iter().position(|v| *v == id).map(|pos| (id, pos))) {
                out.push_str(&format!("song: {pos}\nsongid: {id}\ntime: 0:0\nelapsed: 0.000\n"));
            }
            if let Some(retry) = c.retry_seconds() {
                out.push_str(&format!("radiod_retry: {retry}\n"));
            }
            if let Some(error) = &c.error {
                out.push_str("error: Radio playback failed\n");
                // Core errors are typed/safe diagnostics, not upstream debug text.
                out.push_str(&format!("radiod_error: {}\n", clean(error)));
            }
            if c.persistence_error.is_some() {
                out.push_str("radiod_persistence_error: Unable to save playback state\n");
            }
        }
        "stats" => out = format!("artists: 0\nalbums: 0\nsongs: {}\nuptime: 0\ndb_playtime: 0\ndb_update: 0\nplaytime: 0\n", c.songs.len()),
        "currentsong" => {
            if let Some(id) = c.current {
                out = song(c, id, c.queue.iter().position(|v| *v == id), true);
            }
        }
        "playlistinfo" | "playlistid" | "plchanges" | "plchangesposid" => {
            let positions: Vec<usize> = if cmd == "playlistid" && !p.is_empty() {
                let id = u32::try_from(number(&p[0])?).map_err(|_| invalid("invalid id"))?;
                vec![c.queue.iter().position(|v| *v == id).ok_or((50, "no such song".into()))?]
            } else if cmd.starts_with("plchanges") {
                let version = p[0].parse::<u64>().map_err(|_| invalid("invalid version"))?;
                if version == c.playlist_version {
                    vec![]
                } else {
                    (0..c.queue.len()).collect()
                }
            } else if !p.is_empty() {
                range(&p[0], c.queue.len())?.collect()
            } else {
                (0..c.queue.len()).collect()
            };

            for pos in positions {
                let id = c.queue[pos];
                if cmd == "plchangesposid" {
                    out.push_str(&format!("cpos: {pos}\nId: {id}\n"));
                } else {
                    out.push_str(&song(c, id, Some(pos), true));
                }
            }
        }
        "listplaylists" => out.push_str("playlist: radio\nLast-Modified: 1970-01-01T00:00:00Z\n"),
        "listplaylist" | "listplaylistinfo" | "load" => {
            if p[0] != "radio" {
                return Err((50, "no such playlist".into()));
            }
            let ids: Vec<_> = c.songs.iter().map(|s| s.id).collect();
            if cmd == "load" {
                let ids = if p.len() == 2 { ids[range(&p[1], ids.len())?].to_vec() } else { ids };
                for id in ids {
                    if !c.queue.contains(&id) {
                        c.queue.push(id);
                    }
                }
                queue_changed = true;
            } else {
                for id in ids {
                    out.push_str(&song(c, id, None, cmd == "listplaylistinfo"));
                }
            }
        }
        "clear" => {
            c.stop(false).map_err(|e| (52, e.to_string()))?;
            c.queue.clear();
            c.current = None;
            queue_changed = true;
        }
        "add" | "addid" => {
            let id = c.songs.iter().find(|s| s.station.url == p[0]).map(|s| s.id).ok_or((50, "only configured URLs are allowed".into()))?;
            let pos = if p.len() == 2 { number(&p[1])? } else { c.queue.len() };
            if pos > c.queue.len() {
                return Err((50, "bad position".into()));
            }
            if !c.queue.contains(&id) {
                c.queue.insert(pos, id);
                queue_changed = true;
            }
            if cmd == "addid" {
                out = format!("Id: {id}\n");
            }
        }
        "delete" | "deleteid" => {
            let r = if cmd == "deleteid" {
                let id = u32::try_from(number(&p[0])?).map_err(|_| invalid("invalid id"))?;
                let pos = c.queue.iter().position(|v| *v == id).ok_or((50, "no such song".into()))?;
                pos..pos + 1
            } else {
                range(&p[0], c.queue.len())?
            };
            if c.current.is_some_and(|id| c.queue[r.clone()].contains(&id)) {
                c.stop(false).map_err(|e| (52, e.to_string()))?;
                c.current = None;
            }
            c.queue.drain(r);
            queue_changed = true;
        }
        "move" | "moveid" | "swap" | "swapid" => {
            let by_id = cmd.ends_with("id");
            let resolve = |s: &str| -> std::result::Result<usize, (u8, String)> {
                let n = number(s)?;
                if by_id {
                    c.queue.iter().position(|v| *v as usize == n).ok_or((50, "no such song".into()))
                } else if n < c.queue.len() {
                    Ok(n)
                } else {
                    Err((50, "bad position".into()))
                }
            };
            let from = resolve(&p[0])?;
            let to = if cmd == "moveid" {
                let n = number(&p[1])?;
                if n >= c.queue.len() {
                    return Err((50, "bad position".into()));
                }
                n
            } else {
                resolve(&p[1])?
            };
            if cmd.starts_with("swap") {
                c.queue.swap(from, to);
            } else {
                let id = c.queue.remove(from);
                c.queue.insert(to, id);
            }
            queue_changed = true;
        }
        "play" | "playid" => {
            let id = if p.is_empty() || p[0] == "-1" {
                None
            } else if cmd == "playid" {
                Some(u32::try_from(number(&p[0])?).map_err(|_| invalid("invalid id"))?)
            } else {
                Some(*c.queue.get(number(&p[0])?).ok_or((50, "bad position".into()))?)
            };
            if let Some(id) = id { c.select(id) } else { c.play() }.map_err(|e| (52, e.to_string()))?;
        }
        "pause" => {
            let pause = if p.is_empty() {
                c.state != "pause"
            } else {
                match p[0].as_str() {
                    "0" => false,
                    "1" => true,
                    _ => return Err(invalid("expected 0 or 1")),
                }
            };
            if pause { c.stop(true) } else { c.play() }.map_err(|e| (52, e.to_string()))?;
        }
        "stop" => c.stop(false).map_err(|e| (52, e.to_string()))?,
        "next" | "previous" => {
            if !c.queue.is_empty() {
                let pos = c.current.and_then(|id| c.queue.iter().position(|v| *v == id)).unwrap_or(0);
                let next = if cmd == "next" { (pos + 1) % c.queue.len() } else { (pos + c.queue.len() - 1) % c.queue.len() };
                c.select(c.queue[next]).map_err(|e| (52, e.to_string()))?;
            }
        }
        "setvol" | "volume" => {
            let n = p[0].parse::<i32>().map_err(|_| invalid("invalid volume"))?;
            let v = if cmd == "volume" {
                (c.volume as i64 + n as i64).clamp(0, 100) as u8
            } else {
                if !(0..=100).contains(&n) {
                    return Err(invalid("volume out of range"));
                }
                n as u8
            };
            c.set_volume(v).map_err(|e| (52, e.to_string()))?;
        }
        "repeat" | "random" | "single" | "consume" => {
            if p[0] != "0" {
                return Err((5, "live radio does not support automatic playback options".into()));
            }
        }
        "outputs" => out.push_str("outputid: 0\noutputname: ALSA\noutputenabled: 1\nplugin: alsa\n"),
        "enableoutput" | "disableoutput" => {
            if p[0] != "0" {
                return Err((50, "no such output".into()));
            }
            if cmd == "disableoutput" {
                return Err((5, "disabling the fixed ALSA output is unsupported".into()));
            }
        }
        _ => return Err((5, "command not allowed here".into())),
    }

    if queue_changed {
        c.playlist_version += 1;
        c.changed();
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_before_idle_is_pending() {
        let delivered = 7;
        // Another client changes the core after this client's last status reply.
        let current_at_idle_entry = 8;
        assert!(idle_changed(delivered, current_at_idle_entry));
        assert!(!idle_changed(current_at_idle_entry, current_at_idle_entry));
    }
    #[test]
    fn quoted_arguments() {
        assert_eq!(
            parse(r#"add "https://radio/a b\"c\\d""#).unwrap(),
            vec!["add", "https://radio/a b\"c\\d"]
        );
        assert_eq!(parse("ping").unwrap(), vec!["ping"]);
        assert!(parse("add \"oops").is_err());
        assert!(parse("add x\\").is_err());
    }
    #[test]
    fn ranges() {
        assert_eq!(range("1:3", 4).unwrap(), 1..3);
        assert_eq!(range("2:", 4).unwrap(), 2..4);
        assert!(range("4", 4).is_err());
    }
    #[test]
    fn protocol_sanitization() {
        assert_eq!(ack(5, 2, "bad", "x\ny\r"), "ACK [5@2] {bad} x y \n");
    }
    #[test]
    fn fragmented_line_survives_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let mut lines = Lines { bytes: Vec::new() };
        peer.write_all(b"pi").unwrap();
        assert!(lines.next(&mut server).unwrap().is_none());
        peer.write_all(b"ng\nstatus\n").unwrap();
        assert_eq!(lines.next(&mut server).unwrap().unwrap(), "ping");
        assert_eq!(lines.next(&mut server).unwrap().unwrap(), "status");
    }
}
