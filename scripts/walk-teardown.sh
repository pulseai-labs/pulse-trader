#!/usr/bin/env bash
# Walk teardown and the one-time prod cleanup (r7.s1.w3, PDR-0051 decision 3).
#
# A release walk must leave nothing behind in the trader's Library. This script
# is a client of the server's own routes. It deletes nothing: it archives
# strategies and stops paper sessions, one session at a time.
#
# ## The four modes
#
#   start     Write the walk's manifest BEFORE the walk: the start instant, the
#             server, and every strategy id and paper session id the server
#             already holds (the "before" snapshot).
#   record    Append one strategy id or one session id to the manifest, as the
#             walk creates it. No server call. The file outlives a killed walk.
#   teardown  Stop and archive ONLY the manifest ids that are new since `start`
#             (not in the before snapshot). An id outside the manifest is never
#             touched. Then check that the Library's default list is exactly the
#             keep-list and that no paper session off the keep-list is running.
#   cleanup   The one-time prod cleanup: archive every strategy off the
#             keep-list and stop every running paper session whose strategy is
#             off it. `--dry-run` prints the full list and a digest and writes
#             nothing; `--apply` writes only when `--confirm` quotes that digest.
#
# Exit codes: 0 done; 2 usage error; 3 refused before any write; 4 teardown
# refused one or more manifest rows as predating the walk; 5 the keep-list check
# failed; 6 one or more writes failed. The highest of 4, 5 and 6 wins.
#
# ## The manifest (format v1, UTF-8, one TAB between fields)
#
#   start<TAB><instant><TAB><server>     line 1, written by start
#   before-strategy<TAB><id>             one per strategy the server listed
#   before-session<TAB><id>              one per session the server listed
#   strategy<TAB><id>                    appended by record --strategy
#   session<TAB><id>                     appended by record --session
#
# The manifest is the walk's own file. `start` writes it with mode 0600 and
# publishes it with a hard link, so a manifest is written once. Teardown and
# record trust its contents, including the before snapshot, once these checks
# pass: it is opened without following a symbolic link, it is a regular file the
# current user owns, and it has no group or other write bit.
#
# ## The token rules
#
# The token is an APP-scope token (an agent token is refused by the server's
# routes with 403 scope_refused). It lives in a file with no group or other
# permission bits and is read only by Python. It is never in argv, a log line or
# the output. There is no shell tracing here. The HTTP opener follows no
# redirect and uses no environment proxy, so the Authorization header reaches
# only the server named by --server.
#
# Python does all the work: flags are read by hand from sys.argv, HTTP goes
# through urllib, and every output line is made here.

set -uo pipefail

exec python3 -I - "$@" <<'PY'
import datetime
import errno
import hashlib
import json
import os
import re
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request

PREFIX = "walk-teardown: "
USAGE = (
    "usage: walk-teardown.sh start    --manifest <path> --server <url> --token-file <path>\n"
    "       walk-teardown.sh record   --manifest <path> (--strategy <id> | --session <id>)\n"
    "       walk-teardown.sh teardown --manifest <path> --keep <id>[,<id>...] --server <url> --token-file <path>\n"
    "       walk-teardown.sh cleanup  --keep <id>[,<id>...] --server <url> --token-file <path> (--dry-run | --apply --confirm <sha256>)\n"
)

ID_RE = re.compile(r"[A-Za-z0-9._-]{1,128}")
CODE_RE = re.compile(r"[A-Za-z0-9_.-]{1,64}")
CONFIRM_RE = re.compile(r"[0-9a-f]{64}")
TOKEN_RE = re.compile(r"pt_[A-Za-z0-9_-]{43}")
INSTANT_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z")

HANDSHAKE = "/api/v1/handshake"
LIB = "/api/v1/library-overview"
LIB_ALL = "/api/v1/library-overview/with-archived"
SESSIONS = "/api/v1/paper/sessions"

TOKEN = ""
SERVER = ""

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except (AttributeError, ValueError):
    pass


# --- output -------------------------------------------------------------------


def scrub(text):
    """Control characters become '?'; so does the token, if it ever shows up."""
    text = "".join("?" if (ord(ch) < 0x20 or ord(ch) == 0x7F) else ch for ch in text)
    if TOKEN and TOKEN in text:
        text = text.replace(TOKEN, "?")
    return text


def emit(stream, text):
    try:
        stream.write(PREFIX + scrub(text) + "\n")
        stream.flush()
    except (BrokenPipeError, ValueError):
        pass


def out(text):
    emit(sys.stdout, text)


def err(text):
    emit(sys.stderr, text)


def refuse(reason):
    err("refusing: " + reason)
    sys.exit(3)


def usage_error(reason):
    err("usage error: " + reason)
    try:
        sys.stderr.write(USAGE)
        sys.stderr.flush()
    except (BrokenPipeError, ValueError):
        pass
    sys.exit(2)


# --- arguments ----------------------------------------------------------------

VALUE_FLAGS = ("--manifest", "--server", "--token-file", "--keep", "--strategy", "--session", "--confirm")
BOOL_FLAGS = ("--dry-run", "--apply")
MODE_FLAGS = {
    "start": ("--manifest", "--server", "--token-file"),
    "record": ("--manifest", "--strategy", "--session"),
    "teardown": ("--manifest", "--keep", "--server", "--token-file"),
    "cleanup": ("--keep", "--server", "--token-file", "--dry-run", "--apply", "--confirm"),
}
REQUIRED = {
    "start": ("--manifest", "--server", "--token-file"),
    "record": ("--manifest",),
    "teardown": ("--manifest", "--keep", "--server", "--token-file"),
    "cleanup": ("--keep", "--server", "--token-file"),
}


def parse_args(argv):
    if len(argv) == 1 and argv[0] in ("-h", "--help"):
        sys.stdout.write(USAGE)
        sys.stdout.flush()
        sys.exit(0)
    if not argv:
        usage_error("no mode given")
    mode = argv[0]
    if mode not in MODE_FLAGS:
        usage_error("unknown mode")
    allowed = MODE_FLAGS[mode]
    opts = {}
    i = 1
    while i < len(argv):
        flag = argv[i]
        if flag in BOOL_FLAGS and flag in allowed:
            if flag in opts:
                usage_error("%s given twice" % flag)
            opts[flag] = True
            i += 1
        elif flag in VALUE_FLAGS and flag in allowed:
            if flag in opts:
                usage_error("%s given twice" % flag)
            if i + 1 >= len(argv) or argv[i + 1].startswith("--"):
                usage_error("%s has no value" % flag)
            opts[flag] = argv[i + 1]
            i += 2
        else:
            usage_error("an unknown flag, or a flag the %s mode does not take" % mode)
    for flag in REQUIRED[mode]:
        if flag not in opts:
            usage_error("%s is required" % flag)
    if mode == "record":
        if ("--strategy" in opts) == ("--session" in opts):
            usage_error("record needs exactly one of --strategy and --session")
    if mode == "cleanup":
        if "--dry-run" in opts and "--apply" in opts:
            usage_error("--dry-run and --apply exclude each other")
        if "--dry-run" not in opts and "--apply" not in opts:
            usage_error("cleanup needs --dry-run or --apply")
        if "--confirm" in opts and "--apply" not in opts:
            usage_error("--confirm needs --apply")
        if "--apply" in opts and "--confirm" not in opts:
            usage_error("--apply needs --confirm")
    return mode, opts


# --- value checks (step 2) ----------------------------------------------------


def is_tmp(path):
    for candidate in (path, os.path.normpath(path)):
        collapsed = re.sub(r"^/+", "/", candidate)
        if collapsed == "/tmp" or collapsed.startswith("/tmp/"):
            return True
    return False


def normalize_server(value):
    """The server URL rule. Returns '<scheme>://<netloc>' or None."""
    if value == "" or any(ch.isspace() for ch in value):
        return None
    try:
        parts = urllib.parse.urlsplit(value)
        parts.port
    except ValueError:
        return None
    if parts.scheme not in ("http", "https"):
        return None
    if not parts.hostname:
        return None
    if parts.username is not None or parts.password is not None:
        return None
    if parts.path not in ("", "/"):
        return None
    if parts.query or parts.fragment:
        return None
    return "%s://%s" % (parts.scheme, parts.netloc)


def read_token_file(path):
    bad = "token file %s is missing, not a regular file, or not yours" % path
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError:
        refuse(bad)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            refuse(bad)
        if info.st_mode & 0o077:
            refuse(
                "token file %s is mode %o; want no group or other bits (chmod 600)"
                % (path, info.st_mode & 0o777)
            )
        raw = b""
        while len(raw) <= 4096:
            chunk = os.read(fd, 4097)
            if not chunk:
                break
            raw += chunk
    finally:
        os.close(fd)
    try:
        token = raw.decode("ascii").strip()
    except UnicodeDecodeError:
        token = ""
    if len(raw) > 4096 or not TOKEN_RE.fullmatch(token):
        refuse("token file %s does not hold a pulse token" % path)
    return token


def value_checks(mode, opts):
    """Step 2. Returns the parsed values; a failed check refuses with exit 3."""
    global TOKEN, SERVER
    values = {}
    path_flags = [flag for flag in ("--manifest", "--token-file") if flag in opts]
    for flag in path_flags:
        if not opts[flag].startswith("/"):
            refuse("%s must be an absolute path, got '%s'" % (flag, opts[flag]))
    for flag in path_flags:
        if is_tmp(opts[flag]):
            refuse("%s must not be under /tmp" % flag)
    if "--server" in opts:
        server = normalize_server(opts["--server"])
        if server is None:
            refuse("--server must be http(s)://host[:port], got '%s'" % opts["--server"])
        values["server"] = server
    if "--keep" in opts:
        keep = opts["--keep"].split(",")
        if len(set(keep)) != len(keep) or not all(ID_RE.fullmatch(item) for item in keep):
            refuse("--keep must be a comma-separated list of distinct ids")
        values["keep"] = keep
    for flag in ("--strategy", "--session"):
        if flag in opts and not ID_RE.fullmatch(opts[flag]):
            refuse("the id must match [A-Za-z0-9._-]{1,128}")
    if "--confirm" in opts and not CONFIRM_RE.fullmatch(opts["--confirm"]):
        refuse("--confirm must be 64 lowercase hex digits")
    if "--token-file" in opts:
        TOKEN = read_token_file(opts["--token-file"])
    if "server" in values:
        SERVER = values["server"]
    return values


# --- HTTP ---------------------------------------------------------------------


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect)


def api(method, path):
    """One request. Returns (status, parsed JSON or None, transport error type name)."""
    data = b"{}" if method == "POST" else None
    request = urllib.request.Request(SERVER + path, data=data, method=method)
    request.add_header("Accept", "application/json")
    request.add_header("Authorization", "Bearer " + TOKEN)
    if method == "POST":
        request.add_header("Content-Type", "application/json")
    timeout = 120 if method == "POST" else 300
    try:
        with OPENER.open(request, timeout=timeout) as response:
            status = response.status
            raw = response.read()
    except urllib.error.HTTPError as exc:
        status = exc.code
        try:
            raw = exc.read()
        except Exception:
            raw = b""
        finally:
            exc.close()
    except Exception as exc:
        return None, None, type(exc).__name__
    try:
        body = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        body = None
    return status, body, ""


def code_of(body):
    if isinstance(body, dict):
        code = body.get("code")
        if isinstance(code, str) and CODE_RE.fullmatch(code):
            return code
    return "-"


# --- the server reads ----------------------------------------------------------


def parse_library(body):
    if not isinstance(body, dict) or not isinstance(body.get("strategies"), list):
        return None
    seen = set()
    result = []
    for item in body["strategies"]:
        if not isinstance(item, dict):
            return None
        sid = item.get("id")
        name = item.get("name")
        archived = item.get("archived")
        versions = item.get("versions")
        if not isinstance(sid, str) or not isinstance(name, str):
            return None
        if not isinstance(archived, bool) or not isinstance(versions, list):
            return None
        vids = []
        for version in versions:
            if not isinstance(version, dict) or not isinstance(version.get("id"), str):
                return None
            vids.append(version["id"])
        if sid in seen:
            return None
        seen.add(sid)
        result.append({"id": sid, "name": name, "archived": archived, "versions": vids})
    return result


def parse_sessions(body):
    if not isinstance(body, list):
        return None
    seen = set()
    result = []
    for item in body:
        if not isinstance(item, dict):
            return None
        sid = item.get("id")
        vid = item.get("strategy_version_id")
        status = item.get("status")
        if not isinstance(sid, str) or not isinstance(vid, str) or not isinstance(status, dict):
            return None
        state = status.get("state")
        if state not in ("running", "stopped"):
            return None
        if sid in seen:
            return None
        seen.add(sid)
        result.append({"id": sid, "version": vid, "state": state})
    return result


def library_ids_ok(strategies):
    for item in strategies:
        if not ID_RE.fullmatch(item["id"]):
            return False
        if not all(ID_RE.fullmatch(vid) for vid in item["versions"]):
            return False
    return True


def sessions_ids_ok(sessions):
    return all(ID_RE.fullmatch(s["id"]) and ID_RE.fullmatch(s["version"]) for s in sessions)


def fetch(path, parser):
    """GET and parse. Returns (data, problem); problem is None on success."""
    status, body, name = api("GET", path)
    if status is None:
        return None, "failed: %s" % name
    if status != 200:
        return None, "answered HTTP %d %s" % (status, code_of(body))
    data = parser(body)
    if data is None:
        return None, "answered an unexpected body"
    return data, None


def require(path, parser, scope_hint):
    data, problem = fetch(path, parser)
    if problem is not None:
        if scope_hint and problem.startswith("answered HTTP 403"):
            problem += "; the token must be app scope"
        refuse("GET %s %s" % (path, problem))
    return data


def handshake():
    status, body, name = api("GET", HANDSHAKE)
    if status is None:
        refuse("GET %s failed: %s" % (HANDSHAKE, name))
    if status != 200:
        refuse("handshake answered HTTP %d %s" % (status, code_of(body)))
    if not isinstance(body, dict) or ("role" in body and not isinstance(body["role"], str)):
        refuse("GET %s answered an unexpected body" % HANDSHAKE)
    role = body.get("role")
    out("server %s role %s" % (SERVER, "none" if role is None else role))


def initial_reads():
    """Steps 6 and 7. Returns (strategies incl. archived, sessions)."""
    handshake()
    strategies = require(LIB_ALL, parse_library, True)
    sessions = require(SESSIONS, parse_sessions, True)
    if not library_ids_ok(strategies) or not sessions_ids_ok(sessions):
        refuse("the server returned an invalid id")
    return strategies, sessions


def validate_keep(keep, strategies):
    """Step 8."""
    by_id = {item["id"]: item for item in strategies}
    for kid in keep:
        item = by_id.get(kid)
        if item is None:
            refuse("keep-list id %s is not on the server" % kid)
        if item["archived"]:
            refuse("keep-list id %s is archived; unarchive it first" % kid)


# --- the writes ----------------------------------------------------------------


def post_stop(sid):
    """Stop one session. Returns 'stopped', 'skipped' or 'failed'."""
    status, body, name = api("POST", "/api/v1/paper/sessions/%s/stop" % sid)
    if status is None:
        out("failed session %s: %s" % (sid, name))
        return "failed"
    if status == 200:
        if isinstance(body, dict) and body.get("session_id") == sid:
            out("stopped session %s" % sid)
            return "stopped"
        out("failed session %s: HTTP 200 unexpected body" % sid)
        return "failed"
    if status == 409 and code_of(body) == "session_stopped":
        out("already stopped session %s" % sid)
        return "stopped"
    if status == 404:
        out("unknown session %s: the server does not know it; skipped" % sid)
        return "skipped"
    out("failed session %s: HTTP %d %s" % (sid, status, code_of(body)))
    return "failed"


def post_archive(sid, name):
    """Archive one strategy. Returns 'archived' or 'failed'."""
    status, body, tname = api("POST", "/api/v1/strategies/%s/archive" % sid)
    if status is None:
        out("failed strategy %s: %s" % (sid, tname))
        return "failed"
    if status == 200:
        if isinstance(body, dict) and body.get("id") == sid and body.get("archived") is True:
            out("archived strategy %s %s" % (sid, name))
            return "archived"
        out("failed strategy %s: HTTP 200 unexpected body" % sid)
        return "failed"
    out("failed strategy %s: HTTP %d %s" % (sid, status, code_of(body)))
    return "failed"


# --- the keep-list check -------------------------------------------------------


def keep_check(keep):
    """Reads three things fresh; prints FAIL lines to stderr. True when the check holds."""
    keep_set = set(keep)
    fails = []
    default, problem = fetch(LIB, parse_library)
    if problem is None and not library_ids_ok(default):
        default, problem = None, "answered an unexpected body"
    if problem is not None:
        fails.append("GET %s %s" % (LIB, problem))
    everything, problem = fetch(LIB_ALL, parse_library)
    if problem is None and not library_ids_ok(everything):
        everything, problem = None, "answered an unexpected body"
    if problem is not None:
        fails.append("GET %s %s" % (LIB_ALL, problem))
    sessions, problem = fetch(SESSIONS, parse_sessions)
    if problem is None and not sessions_ids_ok(sessions):
        sessions, problem = None, "answered an unexpected body"
    if problem is not None:
        fails.append("GET %s %s" % (SESSIONS, problem))
    if default is not None:
        listed = set(item["id"] for item in default)
        for item in sorted(default, key=lambda entry: entry["id"]):
            if item["id"] not in keep_set:
                fails.append("listed but not kept: %s %s" % (item["id"], item["name"]))
        for kid in sorted(keep_set):
            if kid not in listed:
                fails.append("kept but not listed: %s" % kid)
    if everything is not None and sessions is not None:
        owners = {}
        names = {}
        for item in everything:
            names[item["id"]] = item["name"]
            for vid in item["versions"]:
                owners.setdefault(vid, set()).add(item["id"])
        for session in sorted(sessions, key=lambda entry: entry["id"]):
            if session["state"] != "running":
                continue
            found = owners.get(session["version"], set())
            if not found:
                fails.append(
                    "running session %s of an unknown strategy is off the keep-list" % session["id"]
                )
            elif len(found) > 1:
                fails.append(
                    "running session %s has version %s listed under more than one strategy"
                    % (session["id"], session["version"])
                )
            else:
                owner = next(iter(found))
                if owner not in keep_set:
                    fails.append(
                        "running session %s of strategy %s %s is off the keep-list"
                        % (session["id"], owner, names[owner])
                    )
    for line in fails:
        err("FAIL keep-list: " + line)
    if fails:
        return False
    out(
        "keep-list holds: %d strategies listed, no running session off the keep-list"
        % len(default)
    )
    return True


def finish(keep, refused, failed):
    code = 0
    if refused:
        code = 4
    if not keep_check(keep):
        code = 5
    if failed:
        code = 6
    if code == 0:
        out("ok")
    elif code == 4:
        err("done; refused rows above (exit 4)")
    elif code == 5:
        err("keep-list check failed (exit 5)")
    else:
        err("one or more writes failed (exit 6)")
    sys.exit(code)


# --- the manifest --------------------------------------------------------------


def open_manifest(path, writable):
    flags = (os.O_RDWR | os.O_APPEND) if writable else os.O_RDONLY
    flags |= os.O_NOFOLLOW | os.O_NONBLOCK
    try:
        fd = os.open(path, flags)
    except OSError as exc:
        if exc.errno == errno.ENOENT:
            refuse("manifest %s does not exist" % path)
        if exc.errno == errno.ELOOP:
            refuse("manifest %s is a symbolic link" % path)
        refuse("manifest %s cannot be opened: %s" % (path, type(exc).__name__))
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
            refuse(
                "manifest %s must be a regular file you own with no group or other write bits" % path
            )
        chunks = []
        while True:
            chunk = os.read(fd, 1 << 20)
            if not chunk:
                break
            chunks.append(chunk)
    except BaseException:
        os.close(fd)
        raise
    return fd, b"".join(chunks)


def parse_manifest(path, data):
    def bad(number):
        refuse("manifest %s line %d is malformed" % (path, number))

    pieces = data.split(b"\n")
    if pieces and pieces[-1] == b"":
        pieces.pop()
    if not pieces:
        bad(1)
    manifest = {
        "server": "",
        "instant": "",
        "before_strategies": set(),
        "before_sessions": set(),
        "strategies": [],
        "sessions": [],
    }
    for number, piece in enumerate(pieces, 1):
        try:
            line = piece.decode("utf-8")
        except UnicodeDecodeError:
            bad(number)
        fields = line.split("\t")
        if number == 1:
            if len(fields) != 3 or fields[0] != "start" or not INSTANT_RE.fullmatch(fields[1]):
                bad(number)
            server = normalize_server(fields[2])
            if server is None:
                bad(number)
            manifest["instant"] = fields[1]
            manifest["server"] = server
            continue
        if len(fields) != 2 or not ID_RE.fullmatch(fields[1]):
            bad(number)
        kind, ident = fields
        if kind == "before-strategy":
            manifest["before_strategies"].add(ident)
        elif kind == "before-session":
            manifest["before_sessions"].add(ident)
        elif kind == "strategy":
            if ident not in manifest["strategies"]:
                manifest["strategies"].append(ident)
        elif kind == "session":
            if ident not in manifest["sessions"]:
                manifest["sessions"].append(ident)
        else:
            bad(number)
    return manifest


def write_all(fd, data):
    view = memoryview(data)
    while len(view):
        written = os.write(fd, view)
        view = view[written:]


# --- the modes -----------------------------------------------------------------


def mode_start(opts):
    path = opts["--manifest"]
    if os.path.lexists(path):
        refuse("manifest %s already exists; a walk's manifest is written once" % path)
    now = datetime.datetime.now(datetime.timezone.utc)
    instant = now.strftime("%Y-%m-%dT%H:%M:%S") + ".%03dZ" % (now.microsecond // 1000)
    strategies, sessions = initial_reads()
    lines = ["start\t%s\t%s\n" % (instant, SERVER)]
    lines += ["before-strategy\t%s\n" % sid for sid in sorted(item["id"] for item in strategies)]
    lines += ["before-session\t%s\n" % sid for sid in sorted(item["id"] for item in sessions)]
    data = "".join(lines).encode("utf-8")
    parent = os.path.dirname(path)
    try:
        os.makedirs(parent, mode=0o700, exist_ok=True)
    except OSError as exc:
        refuse("cannot create %s: %s" % (parent, type(exc).__name__))
    partial = "%s.partial.%d" % (path, os.getpid())
    try:
        fd = os.open(partial, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    except FileExistsError:
        refuse("%s already exists; remove it" % partial)
    except OSError as exc:
        refuse("cannot write %s: %s" % (partial, type(exc).__name__))
    try:
        try:
            os.fchmod(fd, 0o600)
            write_all(fd, data)
            os.fsync(fd)
        finally:
            os.close(fd)
    except OSError as exc:
        os.unlink(partial)
        refuse("cannot write %s: %s" % (partial, type(exc).__name__))
    try:
        os.link(partial, path)
    except FileExistsError:
        os.unlink(partial)
        refuse("manifest %s already exists; a walk's manifest is written once" % path)
    except OSError as exc:
        os.unlink(partial)
        refuse("cannot publish %s: %s" % (path, type(exc).__name__))
    os.unlink(partial)
    try:
        dir_fd = os.open(parent, os.O_RDONLY)
        try:
            os.fsync(dir_fd)
        finally:
            os.close(dir_fd)
    except OSError:
        pass
    out(
        "started %s; %d strategies and %d sessions existed before the walk"
        % (instant, len(strategies), len(sessions))
    )
    out("manifest %s" % path)
    sys.exit(0)


def mode_record(opts):
    path = opts["--manifest"]
    kind = "strategy" if "--strategy" in opts else "session"
    ident = opts["--" + kind]
    fd, data = open_manifest(path, True)
    try:
        manifest = parse_manifest(path, data)
        if ident in manifest["strategies" if kind == "strategy" else "sessions"]:
            out("already recorded %s %s" % (kind, ident))
            sys.exit(0)
        row = ("%s\t%s\n" % (kind, ident)).encode("utf-8")
        if data and not data.endswith(b"\n"):
            row = b"\n" + row
        try:
            written = os.write(fd, row)
            if written != len(row):
                raise OSError(errno.EIO, "short write")
            os.fsync(fd)
        except OSError as exc:
            err("failed record %s %s: %s" % (kind, ident, type(exc).__name__))
            sys.exit(6)
    finally:
        os.close(fd)
    out("recorded %s %s" % (kind, ident))
    sys.exit(0)


def mode_teardown(opts, keep):
    path = opts["--manifest"]
    fd, data = open_manifest(path, False)
    os.close(fd)
    manifest = parse_manifest(path, data)
    if manifest["server"] != SERVER:
        refuse("the manifest was started against %s, not %s" % (manifest["server"], SERVER))
    for kid in keep:
        if kid in manifest["strategies"]:
            refuse("keep-list id %s is in the manifest" % kid)
    strategies, sessions = initial_reads()
    validate_keep(keep, strategies)
    by_session = dict((item["id"], item) for item in sessions)
    by_strategy = dict((item["id"], item) for item in strategies)
    refused = 0
    failed = 0
    for sid in manifest["sessions"]:
        session = by_session.get(sid)
        if session is None:
            out("unknown session %s: the server does not know it; skipped" % sid)
        elif sid in manifest["before_sessions"]:
            out("refused session %s: predates the walk" % sid)
            refused += 1
        elif session["state"] == "stopped":
            out("already stopped session %s" % sid)
        elif post_stop(sid) == "failed":
            failed += 1
    for sid in manifest["strategies"]:
        item = by_strategy.get(sid)
        if item is None:
            out("unknown strategy %s: the server does not know it; skipped" % sid)
        elif sid in manifest["before_strategies"]:
            out("refused strategy %s: predates the walk" % sid)
            refused += 1
        elif item["archived"]:
            out("already archived strategy %s %s" % (sid, item["name"]))
        elif post_archive(sid, item["name"]) == "failed":
            failed += 1
    finish(keep, refused, failed)


def list_digest(keep, stop_ids, archive_ids):
    parts = ["server\t%s\n" % SERVER]
    parts += ["keep\t%s\n" % ident for ident in sorted(keep)]
    parts += ["stop\t%s\n" % ident for ident in sorted(stop_ids)]
    parts += ["archive\t%s\n" % ident for ident in sorted(archive_ids)]
    return hashlib.sha256("".join(parts).encode("utf-8")).hexdigest()


def mode_cleanup(opts, keep):
    strategies, sessions = initial_reads()
    validate_keep(keep, strategies)
    keep_set = set(keep)
    names = dict((item["id"], item["name"]) for item in strategies)
    owners = {}
    for item in strategies:
        for vid in item["versions"]:
            owners.setdefault(vid, set()).add(item["id"])
    for vid in sorted(owners):
        if len(owners[vid]) > 1:
            refuse("version %s is listed under more than one strategy" % vid)
    running = sorted((s for s in sessions if s["state"] == "running"), key=lambda s: s["id"])
    owner_of = {}
    for session in running:
        found = owners.get(session["version"])
        if not found:
            refuse("cannot resolve the strategy of running session %s" % session["id"])
        owner_of[session["id"]] = next(iter(found))
    stop_ids = [s["id"] for s in running if owner_of[s["id"]] not in keep_set]
    left_ids = [s["id"] for s in running if owner_of[s["id"]] in keep_set]
    archive_ids = sorted(
        item["id"] for item in strategies if item["id"] not in keep_set and not item["archived"]
    )
    for kid in sorted(keep):
        out("keep %s %s" % (kid, names[kid]))
    for sid in stop_ids:
        out("stop %s %s" % (sid, names[owner_of[sid]]))
    for sid in archive_ids:
        out("archive %s %s" % (sid, names[sid]))
    for sid in left_ids:
        out("left running %s %s" % (sid, names[owner_of[sid]]))
    out("%d to stop, %d to archive" % (len(stop_ids), len(archive_ids)))
    digest = list_digest(keep, stop_ids, archive_ids)
    out("confirm: %s" % digest)
    if "--dry-run" in opts:
        out("dry run; nothing written")
        sys.exit(0)
    if opts["--confirm"] != digest:
        refuse(
            "--confirm %s does not match the live list's digest %s; run --dry-run again and review the list"
            % (opts["--confirm"], digest)
        )
    failed = 0
    for sid in stop_ids:
        if post_stop(sid) == "failed":
            failed += 1
    for sid in archive_ids:
        if post_archive(sid, names[sid]) == "failed":
            failed += 1
    finish(keep, 0, failed)


def main():
    mode, opts = parse_args(sys.argv[1:])
    values = value_checks(mode, opts)
    if mode == "start":
        mode_start(opts)
    elif mode == "record":
        mode_record(opts)
    elif mode == "teardown":
        mode_teardown(opts, values["keep"])
    else:
        mode_cleanup(opts, values["keep"])


try:
    main()
except SystemExit:
    raise
except Exception as exc:
    err("failed: unexpected %s" % type(exc).__name__)
    sys.exit(6)
PY
