//! Where KiCad's IPC socket lives when the environment does not say.
//!
//! KiCad exports `KICAD_API_SOCKET` only to plugins it launches itself, so a
//! standalone server started by an MCP client sees nothing and every IPC call
//! fails as unconfigured. KiCad's own default is predictable —
//! `<temp dir>/kicad/api.sock` — so look there before giving up.
//!
//! That default is not the whole story, because KiCad binds one socket per
//! *frame* rather than one per process. The project manager owns `api.sock`
//! and answers document queries with `AS_UNHANDLED` — it holds no document to
//! answer about — while a standalone editor registers its own
//! `api-<pid>.sock` beside it. Both are offered here, live frames first:
//! adopting the manager's socket while an editor is open spends every
//! document command on an endpoint that was never going to answer one, and
//! the file-editing fallback that follows looks like a bug in this crate
//! rather than the wrong address.
//!
//! Looking is all this module does, and it never opens a connection: what
//! sits at that path is KiCad's NNG endpoint, and dialling it outside the NNG
//! protocol is what wedged the editor in #498. Whether the endpoint answers is
//! decided later, by the bounded NNG `Ping` in [`crate::client`].
//!
//! Where "looking" happens is per-platform, because the endpoint is. On Unix
//! the socket is a filesystem entry and this reads its metadata. On Windows
//! NNG maps `ipc://` to a **named pipe**, which has no filesystem presence at
//! all, so the same path is looked for in the pipe namespace instead (#529).

use std::path::{Path, PathBuf};

/// Socket paths KiCad may have created on this platform, most likely first.
pub fn candidate_socket_paths() -> Vec<PathBuf> {
    candidates_in(&std::env::temp_dir())
}

fn candidates_in(temp_dir: &Path) -> Vec<PathBuf> {
    candidates_listed_by(temp_dir, frame_sockets_in)
}

/// [`candidates_in`] with per-frame discovery supplied, so a test can prove the
/// ordering without a running KiCad to register a frame socket.
fn candidates_listed_by(
    temp_dir: &Path,
    frame_sockets: impl Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    let mut directories = vec![temp_dir.join("kicad")];
    // macOS resolves the temp dir under /var/folders/…, but KiCad has been
    // seen on /tmp there; try both rather than betting on one. Only there:
    // /tmp is shared and world-writable, and on Linux the temp dir already
    // *is* /tmp unless TMPDIR says otherwise, so the fallback would buy no
    // coverage while letting any local user pre-bind the path we adopt.
    if cfg!(target_os = "macos") {
        let shared_tmp = PathBuf::from("/tmp/kicad");
        if !directories.contains(&shared_tmp) {
            directories.push(shared_tmp);
        }
    }

    // Live per-frame sockets first. KiCad exposes one socket per *frame*, not
    // one per process: the project manager owns `api.sock` and answers
    // `GetOpenDocuments` with `AS_UNHANDLED` because it holds no document,
    // while a standalone editor registers its own `api-<pid>.sock`. Offering
    // only the well-known path handed such a session an endpoint that cannot
    // serve a single document command, and the file-editing fallback that
    // followed read as a Konnect bug rather than as the wrong socket.
    let mut candidates: Vec<PathBuf> = directories
        .iter()
        .flat_map(|directory| frame_sockets(directory.as_path()))
        .collect();

    // …then the well-known default. It is still the right answer when the
    // frame is hosted by the manager's own process, and it stays a candidate
    // even when stale, for the reasons [`detect_ipc_address`] documents.
    for directory in &directories {
        let default = directory.join("api.sock");
        if !candidates.contains(&default) {
            candidates.push(default);
        }
    }
    candidates
}

/// Live per-frame sockets in `kicad_dir`, best endpoint first.
///
/// KiCad names these after the frame's process — `api-<pid>.sock` — and never
/// unlinks them, so this directory collects one corpse per editor that has ever
/// run on a long-lived desktop. Here, unlike `api.sock`, the stale ones are
/// dropped rather than offered: the pid in the name puts liveness one `/proc`
/// read away, and a dead frame placed ahead of a live manager would be a
/// regression rather than a new option. Nothing here connects to find out —
/// that remains the bounded `Ping`'s job, for the reasons [`is_adoptable`]
/// gives.
#[cfg(unix)]
fn frame_sockets_in(kicad_dir: &Path) -> Vec<PathBuf> {
    frame_sockets_found_by(kicad_dir, frame_owner)
}

/// [`frame_sockets_in`] where the endpoint has no filesystem presence.
///
/// NNG maps `ipc://` to a named pipe on Windows, and KiCad's per-frame pipes
/// cannot be enumerated the way [`is_adoptable`] looks one up by name, so
/// Windows keeps the well-known default it already had.
#[cfg(not(unix))]
fn frame_sockets_in(_kicad_dir: &Path) -> Vec<PathBuf> {
    Vec::new()
}

/// Which KiCad frame a socket belongs to, in the order this crate prefers them.
///
/// Every typed command this crate sends is a board command, so a `pcbnew` frame
/// is the endpoint worth having. A schematic frame is ranked last rather than
/// dropped: KiCad 10 answers `AS_UNHANDLED` on it for even `GetVersion`, but
/// `schematic_commands.proto` is empty only on the 10.0 branch — master carries
/// `GetSchematicHierarchy`, `GetSchematicNetlist` and `PlaceSymbolFromLibrary`
/// — so that frame becomes worth dialling without a change here.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FrameRank {
    Board,
    Unidentified,
    Schematic,
}

/// The live KiCad frame behind `pid`, or `None` when nothing there is one.
///
/// Liveness and identity are one question. A pid in a socket name is only as
/// good as the process still being there, and pids are reused — so a name whose
/// pid now belongs to something that is not a KiCad frame is treated as the
/// stale socket it is, rather than as an endpoint to hand the board to.
#[cfg(target_os = "linux")]
fn frame_owner(pid: u32) -> Option<FrameRank> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    match comm.trim() {
        "pcbnew" => Some(FrameRank::Board),
        "eeschema" => Some(FrameRank::Schematic),
        // The manager hosts editor frames inside its own process through
        // KIFACE, so its pid is a frame's pid too — and which editor that is
        // cannot be read off the process name.
        "kicad" => Some(FrameRank::Unidentified),
        _ => None,
    }
}

/// [`frame_owner`] where `/proc` is not available.
///
/// macOS uses the same `api-<pid>.sock` naming and offers no process name to
/// read, so liveness is asked of the kernel directly and the frame is left
/// unidentified. Signal 0 runs the existence and permission checks without
/// delivering anything.
#[cfg(all(unix, not(target_os = "linux")))]
fn frame_owner(pid: u32) -> Option<FrameRank> {
    // SAFETY: kill() with signal 0 delivers no signal and touches no memory.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    alive.then_some(FrameRank::Unidentified)
}

/// [`frame_sockets_in`] with owner resolution supplied, so a test can put a
/// live frame of a chosen kind behind a socket name without starting KiCad.
#[cfg(unix)]
fn frame_sockets_found_by(
    kicad_dir: &Path,
    owner: impl Fn(u32) -> Option<FrameRank>,
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(kicad_dir) else {
        return Vec::new();
    };

    let mut found: Vec<(FrameRank, std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let rank = owner(frame_pid(&path)?)?;
            let registered = entry.metadata().and_then(|meta| meta.modified()).ok()?;
            Some((rank, registered, path))
        })
        .collect();

    // Newest first within a rank: with two frames of the same kind open, the
    // one started most recently is the one the user most likely just opened
    // the document in.
    found.sort_by(|left, right| left.0.cmp(&right.0).then(right.1.cmp(&left.1)));
    found.into_iter().map(|(_, _, path)| path).collect()
}

/// The pid KiCad named a per-frame socket after, or `None` for any other name.
///
/// `api.sock` deliberately does not match: it is the manager's, it is already
/// handled as the well-known default, and it carries no pid to prove anything
/// with.
#[cfg(unix)]
fn frame_pid(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("api-")?
        .strip_suffix(".sock")?
        .parse()
        .ok()
}

/// The IPC address to use when `KICAD_API_SOCKET` is unset, or `None` when
/// this platform's default path holds nothing this user could talk to.
///
/// Returning `None` rather than a guess keeps the "socket path not configured"
/// guidance in place instead of replacing it with a dial failure against an
/// address nobody chose.
///
/// What is returned is a *candidate*, not a proven endpoint. KiCad does not
/// unlink `api.sock` when it exits, so a socket file left by a finished
/// session is indistinguishable from a live one by metadata alone, and this
/// function will return it. The `Ping` that follows then fails within its own
/// bound and the session falls back to file editing — the same outcome as a
/// KiCad that was never running, reached without touching the editor. Proving
/// liveness here instead would mean a second, hand-written handshake against
/// the socket, which is exactly what #498 is.
pub fn detect_ipc_address() -> Option<String> {
    detect_ipc_address_in(&candidate_socket_paths(), is_adoptable)
}

fn detect_ipc_address_in(
    candidates: &[PathBuf],
    is_adoptable: impl Fn(&Path) -> bool,
) -> Option<String> {
    candidates
        .iter()
        .find(|path| is_adoptable(path))
        .map(|path| format_address(path))
}

/// Whether this socket belongs to the user running Konnect.
///
/// A detected socket is adopted as the board endpoint unread, so a path
/// another account can create is a path another account can be handed the
/// board through. The shared-/tmp candidate is the one that matters: its
/// directory is world-writable, so ownership is what separates KiCad's socket
/// from a squatter's.
#[cfg(unix)]
fn is_owned_by_us(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: geteuid() is always successful and touches no memory.
    let euid = unsafe { libc::geteuid() };
    std::fs::metadata(path).is_ok_and(|meta| meta.uid() == euid)
}

/// Whether a candidate path may be adopted as the board endpoint.
///
/// Metadata only, and deliberately so. Existence alone is not enough — KiCad
/// leaves `api.sock` behind when it exits, and a directory anyone can write to
/// is a directory anyone can drop a socket into — so this asks the filesystem
/// three things it already knows: the path exists, it is a socket rather than
/// some other file, and it belongs to this user.
///
/// What it must not do is connect. KiCad's API server is an NNG `REP` socket,
/// and a raw `AF_UNIX` stream that connects and disappears without completing
/// NNG's handshake leaves that server unable to answer *any* client, KiCad's
/// own `kipy` included, until the editor is restarted (#498). Liveness is the
/// job of the bounded `Ping` in [`crate::client`], which speaks the protocol
/// the endpoint expects and is the only handshake this crate performs.
#[cfg(unix)]
fn is_adoptable(path: &Path) -> bool {
    is_adoptable_owned_by(path, is_owned_by_us)
}

/// [`is_adoptable`] with the ownership rule supplied, so a test can prove the
/// gate refuses a socket that is genuinely live. Faking the *owner* is the
/// only way to do that without a second account, and the alternative — trusting
/// that a guard nothing exercises still holds — is how a guard stops holding.
#[cfg(unix)]
fn is_adoptable_owned_by(path: &Path, is_ours: impl Fn(&Path) -> bool) -> bool {
    use std::os::unix::fs::FileTypeExt;

    if !is_ours(path) {
        return false;
    }
    std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

/// The Windows pipe namespace. NNG derives a pipe name by prefixing the
/// `ipc://` path with this, verbatim and separators intact, so the candidate
/// list needs no new entries — only a different place to look them up.
#[cfg(windows)]
const PIPE_NAMESPACE: &str = r"\\.\pipe";

/// Whether a candidate path may be adopted as the board endpoint.
///
/// On Windows the endpoint is not a file. NNG maps `ipc://<path>` to the named
/// pipe `\\.\pipe\<path>`, and with KiCad running the path itself does not
/// exist while that pipe does. Probing the path therefore answered "no" on
/// every Windows install, and a client-launched Konnect — the shipped PCM
/// package registered with an MCP client, with no `ipc_address` and no
/// `KICAD_API_SOCKET` — took the file fallback on every hybrid tool while
/// KiCad held the board open. That is the one outcome `attempt_ipc_write`'s
/// gate exists to prevent, reached because discovery could not see a
/// reachable transport (#529).
///
/// Enumerating the namespace is the whole check. `FindFirstFileW` over
/// `\\.\pipe` lists names without opening any of them, which keeps the
/// metadata-only rule #498 made non-negotiable — and note that
/// `std::fs::metadata` is *not* metadata-only here: on Windows it opens a
/// handle, which against KiCad's endpoint would consume a server instance.
///
/// Matching is case-insensitive because Windows paths are, and the name KiCad
/// derives comes from its own spelling of the temp directory rather than ours.
///
/// One limit, stated rather than papered over: the pipe namespace is
/// machine-global and any local account may create a name in it, so another
/// user could bind this name before KiCad does and be adopted. Unix answers
/// that with an ownership check; the Windows equivalent needs the pipe's
/// security descriptor, which means opening it. An explicitly configured
/// `ipc_address` has always carried the same exposure, so this is the state of
/// the art on this platform rather than something detection introduces.
#[cfg(windows)]
fn is_adoptable(path: &Path) -> bool {
    is_adoptable_among(path, pipe_names)
}

/// Every name currently bound in the pipe namespace, or nothing when it cannot
/// be read. `file_name()` returns the bound name verbatim, backslashes and
/// drive letter included, which is exactly the candidate path to compare with.
#[cfg(windows)]
fn pipe_names() -> Vec<std::ffi::OsString> {
    std::fs::read_dir(PIPE_NAMESPACE)
        .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
        .unwrap_or_default()
}

/// [`is_adoptable`] with the namespace listing supplied, so a test can state
/// what is bound instead of binding it.
#[cfg(windows)]
fn is_adoptable_among(path: &Path, names: impl Fn() -> Vec<std::ffi::OsString>) -> bool {
    let wanted = path.as_os_str().to_string_lossy().to_lowercase();
    names()
        .iter()
        .any(|name| name.to_string_lossy().to_lowercase() == wanted)
}

/// Neither a Unix socket nor a Windows pipe to look for, so nothing is
/// adopted and `IpcAddressSource::Unresolved` keeps its guidance.
#[cfg(not(any(unix, windows)))]
fn is_adoptable(_path: &Path) -> bool {
    false
}

/// Format a socket path the way KiCad prints it, so a detected address and a
/// pasted one are the same string.
fn format_address(path: &Path) -> String {
    format!("ipc://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_start_with_the_temp_dir_socket() {
        let candidates = candidates_in(Path::new("/somewhere/tmp"));
        assert_eq!(
            candidates[0],
            PathBuf::from("/somewhere/tmp/kicad/api.sock")
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shared_tmp_is_a_fallback_candidate_and_is_not_duplicated() {
        let candidates = candidates_in(Path::new("/var/folders/ab/T"));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1], PathBuf::from("/tmp/kicad/api.sock"));

        let candidates = candidates_in(Path::new("/tmp"));
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn the_shared_tmp_fallback_is_macos_only() {
        // On Linux the temp dir already is /tmp unless TMPDIR redirects it, so
        // the fallback adds no coverage — only a world-writable path anyone
        // could have bound first.
        let candidates = candidates_in(Path::new("/somewhere/else"));
        assert_eq!(
            candidates,
            vec![PathBuf::from("/somewhere/else/kicad/api.sock")]
        );
    }

    #[test]
    #[cfg(unix)]
    fn ownership_gates_the_probe() {
        // Ownership is decided before the connect, so a path that cannot be
        // stat'd is refused rather than dialled.
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_owned_by_us(&dir.path().join("absent.sock")));

        let path = dir.path().join("api.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_owned_by_us(&path));
    }

    /// A genuinely foreign-owned path, with no second account to create one.
    ///
    /// Every Unix ships files this user does not own, and `/etc/passwd` is
    /// root's on Linux and macOS alike. It is also not a socket, so this test
    /// alone would pass on either gate; `an_owned_regular_file_is_not_adopted`
    /// is what separates them, by owning its file.
    #[test]
    #[cfg(unix)]
    fn a_foreign_owned_path_is_refused() {
        // SAFETY: geteuid() is always successful and touches no memory.
        if unsafe { libc::geteuid() } == 0 {
            // root owns it, so there is nothing foreign to test against.
            return;
        }
        let foreign = Path::new("/etc/passwd");
        assert!(
            foreign.exists() && !is_owned_by_us(foreign),
            "/etc/passwd must exist and belong to another account"
        );
        assert!(
            !is_adoptable(foreign),
            "a path this user does not own must never be adopted as the board endpoint"
        );
    }

    /// The ownership gate is load-bearing over a socket that would otherwise
    /// pass every other check: a real, listening endpoint whose owner fails it
    /// is still refused. This is the shared-`/tmp` squatter, without a second
    /// account.
    #[test]
    #[cfg(unix)]
    fn a_live_socket_that_fails_the_ownership_check_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_adoptable(&path), "sanity: this socket is ours");

        assert!(
            !is_adoptable_owned_by(&path, |_| false),
            "a live listener must not be adopted when ownership does not check out"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_owned_regular_file_is_not_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"regular file").unwrap();

        assert!(is_owned_by_us(&path), "sanity: this process owns the file");
        assert!(
            !is_adoptable(&path),
            "an owned regular file must not be adopted as an IPC socket"
        );
    }

    /// Discovery must never open a stream connection to a candidate.
    ///
    /// KiCad's API server is an NNG REP endpoint. A raw `AF_UNIX` connect that
    /// never completes NNG's handshake leaves that server unable to answer any
    /// client afterwards, KiCad's own `kipy` included, until the editor is
    /// restarted (#498). The probe therefore has to answer from metadata
    /// alone, and the place to prove it is the listener's side: once detection
    /// has run, nothing may be waiting in its accept queue.
    #[test]
    #[cfg(unix)]
    fn detection_never_connects_to_a_candidate() {
        use std::io::ErrorKind;
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        let candidates = vec![path.clone()];
        let detected = detect_ipc_address_in(&candidates, is_adoptable);
        assert_eq!(
            detected,
            Some(format_address(&path)),
            "sanity: this socket is ours and is the only candidate"
        );

        match listener.accept() {
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Ok(_) => panic!(
                "detection opened a stream connection to the candidate; against KiCad's \
                 NNG endpoint that is the wedge in #498"
            ),
            Err(error) => panic!("unexpected accept error: {error}"),
        }
    }

    #[test]
    fn discovery_picks_the_first_adoptable_candidate() {
        let candidates = vec![
            PathBuf::from("/first/kicad/api.sock"),
            PathBuf::from("/second/kicad/api.sock"),
        ];
        let detected = detect_ipc_address_in(&candidates, |path| path.starts_with("/second"));
        assert_eq!(detected.as_deref(), Some("ipc:///second/kicad/api.sock"));
    }

    #[test]
    fn discovery_finds_nothing_when_no_candidate_is_adoptable() {
        let candidates = vec![PathBuf::from("/first/kicad/api.sock")];
        assert!(detect_ipc_address_in(&candidates, |_| false).is_none());
    }

    /// The cost of not connecting, asserted rather than left to be discovered.
    ///
    /// KiCad does not unlink `api.sock` on exit, and a socket file with no
    /// listener is byte-for-byte the same metadata as one with a listener, so
    /// discovery adopts it. That address then fails its `Ping` within the
    /// bound and the session falls back to file editing — the outcome a
    /// not-running KiCad produces anyway. Rejecting it here would cost a
    /// connect, and the connect is the bug.
    #[test]
    #[cfg(unix)]
    fn a_socket_left_behind_by_a_closed_kicad_is_still_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(is_adoptable(&path), "sanity: a bound socket is adoptable");

        drop(listener);
        assert!(path.exists(), "sanity: the file outlives the listener");
        assert!(
            is_adoptable(&path),
            "metadata cannot tell a stale socket from a live one, and this \
             check does not connect to find out"
        );
    }

    /// The candidate is adopted when a pipe carries its name — the case that
    /// answered "no" for every Windows install before #529.
    #[test]
    #[cfg(windows)]
    fn a_pipe_named_for_the_candidate_is_adopted() {
        let path = Path::new(r"C:\Users\someone\AppData\Local\Temp\kicad\api.sock");
        let bound = vec![
            std::ffi::OsString::from(r"some-other-pipe"),
            std::ffi::OsString::from(path),
        ];

        assert!(is_adoptable_among(path, || bound.clone()));
    }

    #[test]
    #[cfg(windows)]
    fn a_candidate_with_no_pipe_is_not_adopted() {
        let path = Path::new(r"C:\Users\someone\AppData\Local\Temp\kicad\api.sock");
        let bound = vec![std::ffi::OsString::from(r"C:\elsewhere\kicad\api.sock")];

        assert!(!is_adoptable_among(path, || bound.clone()));
        // An unreadable namespace lists nothing and adopts nothing, which is
        // the same answer as a KiCad that is not running.
        assert!(!is_adoptable_among(path, Vec::new));
    }

    /// Windows paths are case-insensitive, and the bound name is KiCad's
    /// spelling of the temp directory rather than ours.
    #[test]
    #[cfg(windows)]
    fn pipe_matching_ignores_case_as_windows_paths_do() {
        let path = Path::new(r"C:\Users\Someone\AppData\Local\Temp\kicad\api.sock");
        let bound = vec![std::ffi::OsString::from(
            r"c:\users\someone\appdata\local\temp\kicad\API.SOCK",
        )];

        assert!(is_adoptable_among(path, || bound.clone()));
    }

    /// The Windows counterpart of `detection_never_connects_to_a_candidate`,
    /// against a real pipe server: discovery must find it by name and must not
    /// become a client of it. A connect that never completes NNG's handshake
    /// is what wedged KiCad in #498, and on Windows it would also consume a
    /// server instance of the endpoint.
    ///
    /// Note the pipe name embeds a path that does not exist on disk. That is
    /// the whole point: the endpoint is not a filesystem object, so a
    /// filesystem probe can never see it.
    #[tokio::test]
    #[cfg(windows)]
    async fn detection_finds_a_live_pipe_without_connecting_to_it() {
        use tokio::net::windows::named_pipe::ServerOptions;

        let path = std::env::temp_dir()
            .join(format!("konnect-529-{}", std::process::id()))
            .join("kicad")
            .join("api.sock");
        let pipe_name = format!(r"{PIPE_NAMESPACE}\{}", path.display());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .expect("bind the pipe KiCad's endpoint would bind");
        assert!(!path.exists(), "sanity: the endpoint is not a file");

        let detected = detect_ipc_address_in(std::slice::from_ref(&path), is_adoptable);
        assert_eq!(
            detected,
            Some(format_address(&path)),
            "a bound pipe carrying the candidate's name must be detected"
        );

        // Nothing connected: the server is still waiting for its first client.
        let connected =
            tokio::time::timeout(std::time::Duration::from_millis(100), server.connect()).await;
        assert!(
            connected.is_err(),
            "detection became a client of the endpoint; against KiCad's NNG server \
             that is the wedge in #498"
        );
    }

    /// With KiCad running, detection works with no configuration at all —
    /// the property #529 is about, and the one no offline test can assert.
    ///
    ///     cargo test -p konnect-ipc --lib live_kicad -- --ignored --nocapture
    #[test]
    #[cfg(windows)]
    #[ignore = "needs a running KiCad with the API server enabled"]
    fn live_kicad_is_discovered_with_no_configuration() {
        let detected = detect_ipc_address();
        assert!(
            detected.is_some(),
            "no KiCad endpoint discovered; candidates were {:?}",
            candidate_socket_paths()
        );
        let address = detected.unwrap();
        assert!(address.starts_with("ipc://"), "{address}");
        println!("discovered {address}");
    }

    #[test]
    fn detected_address_carries_the_ipc_scheme_kicad_prints() {
        assert_eq!(
            format_address(Path::new("/tmp/kicad/api.sock")),
            "ipc:///tmp/kicad/api.sock"
        );
    }

    /// Per-frame sockets come first. The manager's `api.sock` cannot serve a
    /// single document command — it answers `GetOpenDocuments` with
    /// `AS_UNHANDLED` — which is the whole reason discovery looks wider.
    #[test]
    fn a_live_frame_is_offered_before_the_manager_socket() {
        let frame = PathBuf::from("/somewhere/tmp/kicad/api-4321.sock");
        let candidates = candidates_listed_by(Path::new("/somewhere/tmp"), |directory| {
            if directory == Path::new("/somewhere/tmp/kicad") {
                vec![frame.clone()]
            } else {
                Vec::new()
            }
        });

        assert_eq!(candidates[0], frame);
        assert_eq!(
            candidates[1],
            PathBuf::from("/somewhere/tmp/kicad/api.sock"),
            "the well-known default stays, for the manager-hosted frame case"
        );
    }

    /// With no live frame, discovery behaves exactly as it did before.
    #[test]
    fn the_manager_socket_is_the_fallback_when_no_frame_is_live() {
        let candidates = candidates_listed_by(Path::new("/somewhere/tmp"), |_| Vec::new());
        assert_eq!(
            candidates[0],
            PathBuf::from("/somewhere/tmp/kicad/api.sock")
        );
    }

    /// A socket whose process is gone is a corpse, not an endpoint. These
    /// fixtures are plain files: socket-ness is [`is_adoptable`]'s question,
    /// and asking it here would only duplicate the gate.
    #[test]
    #[cfg(unix)]
    fn a_frame_socket_whose_process_is_gone_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api-4294967295.sock"), b"").unwrap();

        assert!(
            frame_sockets_in(dir.path()).is_empty(),
            "a pid that owns nothing cannot be a frame"
        );
    }

    /// Pids are reused, so being alive is not enough — it has to be a frame.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_frame_socket_whose_pid_is_not_a_kicad_frame_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        // This process is certainly alive and certainly not a KiCad editor.
        let squatter = format!("api-{}.sock", std::process::id());
        std::fs::write(dir.path().join(squatter), b"").unwrap();

        assert!(frame_sockets_in(dir.path()).is_empty());
    }

    /// Board frames first: every typed command this crate sends is a board
    /// command, and KiCad 10's schematic frame answers `AS_UNHANDLED` on all
    /// of them.
    #[test]
    #[cfg(unix)]
    fn board_frames_outrank_schematic_frames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api-11.sock"), b"").unwrap();
        std::fs::write(dir.path().join("api-22.sock"), b"").unwrap();

        let found = frame_sockets_found_by(dir.path(), |pid| match pid {
            11 => Some(FrameRank::Schematic),
            22 => Some(FrameRank::Board),
            _ => None,
        });

        assert_eq!(
            found,
            vec![
                dir.path().join("api-22.sock"),
                dir.path().join("api-11.sock")
            ]
        );
    }

    /// Two frames of the same kind: the one registered most recently wins.
    #[test]
    #[cfg(unix)]
    fn the_newest_frame_of_a_kind_is_offered_first() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("api-11.sock");
        let newer = dir.path().join("api-22.sock");
        for (path, seconds) in [(&older, 1_000), (&newer, 2_000)] {
            let handle = std::fs::File::create(path).unwrap();
            handle
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
                )
                .unwrap();
        }

        let found = frame_sockets_found_by(dir.path(), |_| Some(FrameRank::Board));

        assert_eq!(found, vec![newer, older]);
    }

    /// The manager's socket carries no pid and must not be ranked as a frame.
    #[test]
    #[cfg(unix)]
    fn the_manager_socket_is_not_mistaken_for_a_frame() {
        assert_eq!(frame_pid(Path::new("/tmp/kicad/api.sock")), None);
        assert_eq!(frame_pid(Path::new("/tmp/kicad/api-.sock")), None);
        assert_eq!(frame_pid(Path::new("/tmp/kicad/api-notapid.sock")), None);
        assert_eq!(
            frame_pid(Path::new("/tmp/kicad/api-2417522.sock")),
            Some(2_417_522)
        );
    }
}
