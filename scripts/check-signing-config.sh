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
# The signed branch of `bundle` must therefore test the bundle against the
# Apple-issued anchor and WARN (to stderr, the #392 wording; never fail the build)
# when the test does not hold.
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
    # #392: the bare text `Developer ID` is ALLOWED -- comments and warning lines
    # name the identity CLASS ("an Apple Development or Developer ID identity"),
    # which commits nothing. The identity VALUE forms are what must never appear:
    # codesign prints an Apple-issued identity as `<prefix> <name> (<TEAMID>)`, so
    # these prefixes (each carrying the certificate name and the team id) are the
    # banned spellings.
    "Developer ID Application:",
    "Developer ID Installer:",
    "Apple Development:",
    "Apple Distribution:",
    "Mac Developer:",
]
for rel in ("tauri.conf.json", "justfile", "package.json"):
    text = text_of(rel)
    if text is None:
        continue
    for banned in COMMIT_BANNED:
        if banned in text:
            fail(f"{rel} commits `{banned}` -- no signing identity, certificate name "
                 "or team id may be committed; the identity comes from the environment")

# --- 5. the bundle recipe passes PULSE_SIGNING_IDENTITY through --------------
justfile = text_of("justfile")
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

# --- 7. the signed branch proves the identity is Apple-issued (#392) --------
# `codesign --verify --strict` passes for a SELF-SIGNED certificate too -- the
# signature is valid, only the issuer is not Apple -- so the signed branch must
# also run a requirement test against the Apple-issued anchor (the canonical
# `anchor apple generic` requirement, in the recipe, on the real bundle) and, when
# that test fails, WARN on stderr with the #392 wording. A self-signed build warns;
# it does not fail the build, so the guard is the recipe's own two lines, asserted
# here.
if bundle_body is not None:
    if not re.search(r"codesign\s+--verify\s+-R=\S*anchor\s+apple\s+generic",
                     bundle_body):
        fail("the `bundle` recipe's signed branch does not verify the bundle "
             "against the Apple-issued anchor (`codesign --verify "
             "-R='anchor apple generic' \"$app\"`) -- a self-signed identity then "
             "passes `codesign --verify --strict` as a signed build and the Keychain "
             "prompts on every new build (#392)")
    if not re.search(
            r"(?m)^[ \t].*prompts on every new build \(#392\).*>&2[ \t]*$",
            bundle_body):
        fail("the `bundle` recipe's signed branch does not warn on stderr when the "
             "anchor test fails -- an identity that is not issued by Apple makes "
             "the Keychain prompt on every new build (#392); the warning line must "
             "carry the literal `prompts on every new build (#392)` and end in "
             "`>&2`")

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
