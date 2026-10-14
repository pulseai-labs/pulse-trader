#!/usr/bin/env bash
# r4.s3.w1 — signing-configuration gate (AC-1; grill G5 / G6 / G11).
#
# The macOS bundle has ONE signing identity, and it lives in the ENVIRONMENT, never
# in a committed file: `just bundle` passes PULSE_SIGNING_IDENTITY to the Tauri build
# as APPLE_SIGNING_IDENTITY (the variable Tauri v2's bundler reads), and keyring's
# real macOS store (`apple-native`) is target-scoped so the Linux QA build keeps the
# mock. Every check below reads a specific claim out of a specific file and fails
# when the claim is absent (the check-adr-0020.sh discipline: assert CONTENT, not
# existence), so an edit that quietly commits an identity, drops the hardened
# runtime or un-scopes the keyring feature reddens here.
#
# #392: the identity must also be ISSUED BY APPLE, not merely present. keyring's
# `apple-native` store is the file-based login keychain, whose items carry a
# securityd PARTITION LIST beside their ACL: an Apple-issued certificate (Apple
# Development / Developer ID) partitions as `teamid:<team>` and any other -- a
# self-signed one -- as `cdhash:<cdhash>`, which changes on every build, so every
# new self-signed build fails the partition check and prompts for the Keychain.
# The signed branch of `bundle` must therefore test the real bundle against
# securityd's own team-ID requirement (`developmentOrDevIDReq`: the Developer ID CA /
# WWDR Mac Development / WWDR Mac Distribution leaves -- a bare `anchor apple
# generic` accepts ANY Apple-issued certificate) and WARN (to stderr, the #392
# wording; never fail the build) when the test does not hold. Check 7 reads the
# signed branch's CODE only: a commented-out, relocated or gutted test, or a warning
# moved out of the test's `if` block, is a missing guard.
#
# Requires: python3 (json + re only — deliberately NOT tomllib, because the macOS CI
# python3 can be older than 3.11). Exit 0 iff every check holds; exit 1 naming every
# failure.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL: python3 is required to check the signing configuration" >&2
  exit 1
fi

python3 - "$repo_root" <<'PY'
import json
import pathlib
import re
import sys

repo = pathlib.Path(sys.argv[1])
failures = []


def fail(msg):
    failures.append(msg)


def text_of(rel):
    path = repo / rel
    if not path.is_file():
        fail(f"{rel} does not exist (this gate reads it)")
        return None
    return path.read_text()


# --- 1. bundle.macOS exists with the hardened runtime ON ---------------------
conf_text = text_of("tauri.conf.json")
conf = None
if conf_text is not None:
    try:
        conf = json.loads(conf_text)
    except json.JSONDecodeError as error:
        fail(f"tauri.conf.json is not valid JSON: {error}")

if conf is not None:
    macos = conf.get("bundle", {}).get("macOS")
    if not isinstance(macos, dict):
        fail("tauri.conf.json has no `bundle.macOS` section -- the macOS bundle "
             "configuration (hardened runtime; the signing identity from the "
             "environment) is gone")
    elif macos.get("hardenedRuntime") is not True:
        fail(f"`bundle.macOS.hardenedRuntime` is {macos.get('hardenedRuntime')!r}, "
             "expected true -- the hardened runtime must stay on for the signed build")

    def walk(node, path):
        if isinstance(node, dict):
            for key, value in node.items():
                if key == "signingIdentity":
                    fail(f"tauri.conf.json commits a signing identity at "
                         f"{path}.signingIdentity -- the identity is environment-only")
                walk(value, f"{path}.{key}")
        elif isinstance(node, list):
            for index, value in enumerate(node):
                walk(value, f"{path}[{index}]")

    walk(conf, "tauri.conf.json")

# --- 2-4. no identity, certificate name or team id is committed ---------------
# Case-sensitive on purpose: `APPLE_SIGNING_IDENTITY` / `PULSE_SIGNING_IDENTITY`
# are env-var NAMES and must stay allowed; an identity VALUE must not appear.
COMMIT_BANNED = [
    "Local Signing",     # the certificate's name (G11)
    "signingIdentity",   # the Tauri config key
    "signing_identity",
    "Team ID",
    "team_id",
    "teamId",
    "teamID",
    "TEAM_ID",
    # #392: the two-word text `Developer ID` is not itself an identity VALUE --
    # comments and warning lines name the identity CLASS ("an Apple Development or
    # Developer ID identity"), which commits nothing -- so it has no entry here; it IS
    # a certificate selector in code, and IDENTITY_CLASS_NAMES bans it there. The
    # identity VALUE forms are what must never appear anywhere:
    # codesign prints an Apple-issued identity as `<prefix> <name> (<TEAMID>)`, so
    # these prefixes (each carrying the certificate name and the team id) are the
    # banned spellings. `3rd Party Mac Developer` is verbatim, without the colon of
    # the others, because its two value forms are spelled `3rd Party Mac Developer
    # Application:` and `3rd Party Mac Developer Installer:`.
    "Developer ID Application:",
    "Developer ID Installer:",
    "Apple Development:",
    "Apple Distribution:",
    "Mac Developer:",
    "3rd Party Mac Developer",
]

# #392: a colon-less CLASS name selects a certificate too --
# `PULSE_SIGNING_IDENTITY="Apple Development"` or `--sign "Developer ID
# Application"` -- so the class names must not appear as code either. In
# tauri.conf.json and package.json, pure config with no comment to excuse them, they
# are banned ANYWHERE; in the justfile they are banned on every line that is not a
# comment and not the #392 warning line, whose text has to name the classes it talks
# about. The bare two-word `Developer ID` is on the list (Codex 4269177281): codesign
# resolves an identity by common-name SUBSTRING, so `PULSE_SIGNING_IDENTITY="Developer
# ID"` already selects a Developer ID certificate.
IDENTITY_CLASS_NAMES = [
    "Developer ID Application",
    "Developer ID Installer",
    "Developer ID",
    "Apple Development",
    "Apple Distribution",
    "Mac Developer",
    "3rd Party Mac Developer",
]

for rel in ("tauri.conf.json", "package.json"):
    text = text_of(rel)
    if text is None:
        continue
    for banned in COMMIT_BANNED:
        if banned in text:
            fail(f"{rel} commits `{banned}` -- no signing identity, certificate name "
                 "or team id may be committed; the identity comes from the environment")
    for banned in IDENTITY_CLASS_NAMES:
        if banned in text:
            fail(f"{rel} names the identity class `{banned}` -- a certificate can be "
                 "selected by class name as well as by a full identity value, so no "
                 "signing identity may be committed; the identity comes from the "
                 "environment (#392)")

justfile = text_of("justfile")
if justfile is not None:
    for number, line in enumerate(justfile.splitlines(), start=1):
        for banned in COMMIT_BANNED:
            if banned in line:
                fail(f"justfile line {number} commits `{banned}` -- no signing "
                     "identity, certificate name or team id may be committed; the "
                     "identity comes from the environment")
        if line.lstrip().startswith("#") or "prompts on every new build (#392)" in line:
            continue  # a comment, or the #392 warning line, may name the classes
        for banned in IDENTITY_CLASS_NAMES:
            if banned in line:
                fail(f"justfile line {number} uses the identity class `{banned}` as "
                     'code -- `PULSE_SIGNING_IDENTITY="Apple Development"` selects a '
                     "certificate, so the class name must not appear in a recipe; "
                     "only a comment or the #392 warning line may name it (#392)")

# --- 5. the bundle recipe passes PULSE_SIGNING_IDENTITY through --------------
bundle_body = None
if justfile is not None:
    match = re.search(r"(?ms)^bundle:\s*\n((?:[ \t].*\n|\n)*)", justfile)
    if match is None:
        fail("the justfile has no `bundle:` recipe")
    else:
        bundle_body = match.group(1)
        for var in ("PULSE_SIGNING_IDENTITY", "APPLE_SIGNING_IDENTITY"):
            if var not in bundle_body:
                fail(f"the `bundle` recipe does not pass {var} through -- a signed "
                     'build needs APPLE_SIGNING_IDENTITY="$PULSE_SIGNING_IDENTITY" '
                     "(the variable Tauri v2's bundler reads)")

# --- 6. keyring's apple-native is target-scoped to macOS ---------------------
cargo_toml = text_of("Cargo.toml")
if cargo_toml is not None:
    # Comment prose may NAME the feature; only code counts. A `#` inside a quoted
    # value does not occur in this file (checked when this gate was authored), so
    # stripping from the first `#` on a line is exact here.
    tables = re.split(r"(?m)^\[([^\]]+)\]\s*$", cargo_toml)
    headers = tables[1::2]
    bodies = tables[2::2]

    hits = []  # (header, stripped line) for every code occurrence
    for header, body in zip(headers, bodies):
        for line in body.splitlines():
            if line.lstrip().startswith("#"):
                continue
            code = line.split("#", 1)[0]
            if "apple-native" in code:
                hits.append((header, line.strip()))

    def is_macos_target(header):
        return header.startswith("target.") and 'target_os = "macos"' in header

    for header, line in hits:
        if not is_macos_target(header) or not re.match(r"\s*keyring\s*=", line):
            fail(f"`apple-native` is enabled outside a macOS-scoped keyring entry "
                 f"(in [{header}]: {line}) -- that would reach every platform build "
                 "and Linux QA would lose the mock store")

    if not any(re.match(r"\s*keyring\s*=", line) for header, line in hits
               if is_macos_target(header)):
        fail('no `[target.\'cfg(target_os = "macos")\'.dependencies]` keyring entry '
             "carries `apple-native` -- macOS would keep compiling the mock store "
             "and the Keychain would stay inert")

# --- 7. the signed branch runs the team-ID test, and warns in it (#392) ------
# `codesign --verify --strict` passes for a SELF-SIGNED certificate too -- the
# signature is valid, only the issuer is not Apple -- so the signed branch must run
# the requirement test securityd itself uses (`developmentOrDevIDReq`: the Developer
# ID CA / WWDR Mac Development / WWDR Mac Distribution leaves; a bare
# `anchor apple generic` accepts ANY Apple-issued certificate) against the real
# bundle and WARN on stderr with the #392 wording when it fails. The guard binds to
# CODE: comments are dropped, only the SIGNED branch counts, the requirement must be
# an uncommented `if ! codesign --verify -R=` test carrying the three team-ID leaf
# OIDs (inline, or through a variable assigned in the same branch), and the warning
# must sit inside that `if` block -- a commented-out, relocated or gutted test, or a
# warning moved out of the block, is a missing guard. The `-R=` form is part of the
# test: codesign compiles an argument that begins with `=` as literal requirement
# text, while `-R '...'` would be read as a FILE and fail for every identity.
TEAM_ID_LEAF_OIDS = (
    "1.2.840.113635.100.6.1.13",  # Developer ID leaf
    "1.2.840.113635.100.6.1.12",  # Mac Development leaf (an Apple Development cert)
    "1.2.840.113635.100.6.1.7",   # Mac Distribution leaf
)


def signed_branch(body):
    """The `bundle` recipe's signed branch as (line number, line) CODE pairs, or
    None when the branch itself cannot be read. Comments are dropped: a
    commented-out test or warning is no test and no warning."""
    code = [(number, line) for number, line in enumerate(body.splitlines())
            if not line.lstrip().startswith("#")]
    start = None
    for number, line in code:
        match = re.match(
            r"^([ \t]*)if \[ -n \"\$\{PULSE_SIGNING_IDENTITY:-\}\" \]; then[ \t]*$",
            line)
        if match:
            start = (number, match.group(1))
            break
    if start is None:
        return None
    number, indent = start
    for end_number, line in code:
        if end_number > number and line.rstrip() == indent + "else":
            return [(n, l) for n, l in code if number < n < end_number]
    return None


if bundle_body is not None:
    branch = signed_branch(bundle_body)
    if branch is None:
        fail("the `bundle` recipe has no readable signed branch (`if [ -n "
             "\"${PULSE_SIGNING_IDENTITY:-}\" ]; then` up to its `else`) -- check 7 "
             "cannot read the team-ID test or the #392 warning out of it (#392)")
    else:
        test = None
        for number, line in branch:
            if re.match(r"^[ \t]*if\s+!\s+codesign\s+--verify\s+-R=", line):
                test = (number, line)
                break
        if test is None:
            fail("the `bundle` recipe's signed branch does not run the Apple-issued "
                 "anchor test -- an uncommented `if ! codesign --verify -R=` line "
                 "testing the real bundle against securityd's team-ID requirement "
                 "(developmentOrDevIDReq) is missing, commented out or outside this "
                 "branch; a self-signed identity then passes `codesign --verify "
                 "--strict`, and the Keychain prompts on every new build (#392)")
        else:
            number, line = test
            argument = re.search(r"-R=(\"[^\"]*\"|'[^']*'|\S+)", line)
            requirement = argument.group(1).strip("\"'") if argument else ""
            if "$" in requirement:
                variable = re.search(r"\$\{?(\w+)\}?", requirement)
                requirement = ""
                if variable:
                    for _, candidate in branch:
                        assignment = re.match(
                            r"[ \t]*" + re.escape(variable.group(1)) + r"=(.*)$",
                            candidate)
                        if assignment is not None:
                            requirement = assignment.group(1).strip().strip("\"'")
                            break
            missing = [oid for oid in TEAM_ID_LEAF_OIDS if oid not in requirement]
            if missing:
                fail("the `bundle` recipe's team-ID requirement does not carry the "
                     f"team-ID leaf OIDs {' '.join(missing)} -- securityd's "
                     "developmentOrDevIDReq needs the Developer ID CA / WWDR Mac "
                     "Development / WWDR Mac Distribution leaves; a bare "
                     "`anchor apple generic` accepts ANY Apple-issued certificate, "
                     "and the Keychain prompts on every new build (#392)")
            indent = re.match(r"^([ \t]*)", line).group(1)
            close = None
            for candidate_number, candidate in branch:
                if candidate_number > number and candidate.rstrip() == indent + "fi":
                    close = candidate_number
                    break
            warning = None
            for candidate_number, candidate in branch:
                if "prompts on every new build (#392)" in candidate:
                    warning = (candidate_number, candidate)
                    break
            if close is None:
                fail("the `bundle` recipe's team-ID test has no closing `fi` in the "
                     "signed branch, so check 7 cannot find the #392 warning's block "
                     "(#392)")
            elif warning is None:
                fail("the `bundle` recipe's signed branch does not warn on stderr when "
                     "the team-ID test fails -- the warning line inside the test must "
                     "carry the literal `prompts on every new build (#392)` (#392)")
            else:
                warning_number, warning_line = warning
                if (not number < warning_number < close
                        or not warning_line.rstrip().endswith(">&2")):
                    fail("the `bundle` recipe's #392 warning is not inside its `if ! "
                         "codesign --verify -R=` block on stderr -- it must sit after "
                         "the test line and before its `fi`, and end in `>&2`, or a "
                         "build whose identity fails the team-ID test is never told "
                         "why the Keychain prompts on every new build (#392)")

# --- report -----------------------------------------------------------------
if failures:
    for message in failures:
        print(f"FAIL: {message}", file=sys.stderr)
    print(f"check-signing-config: {len(failures)} failure(s)", file=sys.stderr)
    sys.exit(1)

print("check-signing-config: OK (bundle.macOS hardened runtime on; no committed "
      "identity; PULSE_SIGNING_IDENTITY passed through to the Tauri build; keyring "
      "apple-native macOS-scoped)")
PY
