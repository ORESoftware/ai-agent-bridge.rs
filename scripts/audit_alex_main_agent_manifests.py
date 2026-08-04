#!/usr/bin/env python3
"""Fail-closed drift audit for alex-main-agent routing manifests (DEN-1320)."""
from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence

MAX_REGISTRY = 1_048_576
MAX_LOCK = 262_144
MAX_REMOTE = 1_048_576
MAX_MANIFEST = 65_536
COUNT = 13
WORKSPACE = "T01B3C83PMK"
APP = "A0BMBAMM5NJ"
TEAM = "Denman"
TEAM_ID = "eb8ab169-5afe-4b6f-9cab-3f2aa3e887dc"
TEAM_KEY = "DEN"
REJECTED_DAEDALUS = "C0BMB9GSSKY"
SHA1 = re.compile(r"[0-9a-f]{40}")
SHA256 = re.compile(r"[0-9a-f]{64}")
ISSUE = re.compile(r"DEN-[1-9][0-9]*")
SLACK = re.compile(r"[CTUW][A-Z0-9]{8,}")
REPO = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
EXPECTED_ROUTING = {
    "require_linear_issue": True,
    "branch_and_pr_include_issue_id": True,
    "post_pr_ci_review_merge_updates_to_origin_thread": True,
    "idempotency_source": "slack_event_id",
    "organization_allowlist_only": True,
    "redact_secrets": True,
}


class AuditError(RuntimeError):
    def __init__(self, code: str, detail: str):
        super().__init__(f"{code}: {detail}")
        self.code, self.detail = code, detail


def _pairs(pairs: Sequence[tuple[str, Any]]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for key, value in pairs:
        if key in out:
            raise AuditError("duplicate_json_key", key)
        out[key] = value
    return out


def read_bounded(path: Path, limit: int, label: str) -> bytes:
    try:
        stat = path.stat()
        if not path.is_file():
            raise AuditError("file_not_regular", label)
        if stat.st_size > limit:
            raise AuditError("file_too_large", label)
        data = path.read_bytes()
    except AuditError:
        raise
    except OSError as error:
        raise AuditError("file_unreadable", f"{label}: {error.__class__.__name__}") from None
    if len(data) > limit:
        raise AuditError("file_too_large", label)
    return data


def parse_json_bytes(data: bytes, label: str) -> Any:
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=_pairs)
    except AuditError:
        raise
    except UnicodeDecodeError:
        raise AuditError("invalid_utf8", label) from None
    except json.JSONDecodeError as error:
        raise AuditError("invalid_json", f"{label}: {error.lineno}:{error.colno}") from None


def load_json(path: Path, limit: int, label: str) -> Any:
    return parse_json_bytes(read_bounded(path, limit, label), label)


def canonical_json_sha256(value: Any) -> str:
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(raw).hexdigest()


def as_obj(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise AuditError("invalid_type", f"{label} must be object")
    return value


def as_arr(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        raise AuditError("invalid_type", f"{label} must be array")
    return value


def as_str(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise AuditError("invalid_type", f"{label} must be non-empty string")
    return value


def as_int(value: Any, label: str, low: int = 0, high: int = 2**31 - 1) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise AuditError("invalid_type", f"{label} must be integer [{low},{high}]")
    return value


def exact(value: Mapping[str, Any], required: set[str], optional: set[str], label: str) -> None:
    present = set(value)
    missing, unknown = sorted(required - present), sorted(present - required - optional)
    if missing:
        raise AuditError("missing_field", f"{label}: {','.join(missing)}")
    if unknown:
        raise AuditError("unknown_field", f"{label}: {','.join(unknown)}")


def eq(actual: Any, expected: Any, label: str) -> None:
    if actual != expected:
        raise AuditError("contract_mismatch", label)


def pattern(value: Any, regex: re.Pattern[str], label: str, code: str) -> str:
    text = as_str(value, label)
    if not regex.fullmatch(text):
        raise AuditError(code, label)
    return text


def string_list(value: Any, label: str) -> list[str]:
    out = [as_str(item, f"{label}[{i}]") for i, item in enumerate(as_arr(value, label))]
    if len(out) != len(set(out)):
        raise AuditError("duplicate_value", label)
    return out


def budget(value: Any, label: str) -> dict[str, int]:
    obj = as_obj(value, label)
    ranges = {
        "max_concurrent_runs": (1, 64),
        "max_runtime_secs": (1, 86_400),
        "max_tokens": (1, 10_000_000),
        "max_spend_cents": (0, 1_000_000),
        "max_retries": (0, 20),
    }
    exact(obj, set(ranges), set(), label)
    return {key: as_int(obj[key], f"{label}.{key}", *bounds) for key, bounds in ranges.items()}


@dataclass(frozen=True)
class LockEntry:
    source: dict[str, Any]
    slack: dict[str, Any]
    linear: dict[str, Any]
    github: dict[str, Any]
    central: dict[str, Any]


@dataclass(frozen=True)
class Lock:
    registry: dict[str, str]
    policy: dict[str, Any]
    entries: tuple[LockEntry, ...]


def validate_policy(raw: Any) -> dict[str, Any]:
    obj = as_obj(raw, "lock.central_policy")
    keys = {"linear_team_id", "linear_team_key", "default_agent_mode", "allowed_agent_modes", "allowed_user_ids", "allowed_user_group_ids", "write_policy", "budget_policy"}
    exact(obj, keys, set(), "lock.central_policy")
    eq(obj["linear_team_id"], TEAM_ID, "central Linear team ID")
    eq(obj["linear_team_key"], TEAM_KEY, "central Linear team key")
    modes = string_list(obj["allowed_agent_modes"], "central_policy.allowed_agent_modes")
    default_mode = as_str(obj["default_agent_mode"], "central_policy.default_agent_mode")
    if default_mode not in modes:
        raise AuditError("contract_mismatch", "default agent mode")
    users = string_list(obj["allowed_user_ids"], "central_policy.allowed_user_ids")
    groups = string_list(obj["allowed_user_group_ids"], "central_policy.allowed_user_group_ids")
    for i, principal in enumerate(users + groups):
        pattern(principal, SLACK, f"central_policy.principal[{i}]", "invalid_slack_id")
    write_policy = as_str(obj["write_policy"], "central_policy.write_policy")
    if write_policy not in {"none", "draft_pull_request"}:
        raise AuditError("contract_mismatch", "write policy")
    return {**obj, "allowed_agent_modes": modes, "allowed_user_ids": users, "allowed_user_group_ids": groups, "budget_policy": budget(obj["budget_policy"], "central_policy.budget_policy")}


def validate_lock(raw: Any) -> Lock:
    obj = as_obj(raw, "lock")
    top = {"schema_version", "workspace_id", "app_id", "linear_team", "central_registry", "central_policy", "entries"}
    exact(obj, top, set(), "lock")
    eq(as_int(obj["schema_version"], "lock.schema_version", 1, 1), 1, "lock schema")
    eq(obj["workspace_id"], WORKSPACE, "workspace")
    eq(obj["app_id"], APP, "app")
    eq(obj["linear_team"], TEAM, "Linear team")
    reg = as_obj(obj["central_registry"], "lock.central_registry")
    exact(reg, {"path", "canonical_sha256"}, set(), "lock.central_registry")
    eq(reg["path"], "config/alex-main-agent.channels.json", "registry path")
    pattern(reg["canonical_sha256"], SHA256, "registry digest", "invalid_digest")
    policy = validate_policy(obj["central_policy"])
    rows = as_arr(obj["entries"], "lock.entries")
    if len(rows) != COUNT:
        raise AuditError("contract_mismatch", f"expected {COUNT} lock entries")

    entries, channels, repos, routing_issues, delivery_issues = [], set(), set(), set(), set()
    for i, raw_entry in enumerate(rows):
        label = f"lock.entries[{i}]"
        entry = as_obj(raw_entry, label)
        exact(entry, {"source", "slack", "linear", "github", "central"}, set(), label)
        source = as_obj(entry["source"], f"{label}.source")
        source_keys = {"repository", "pull_request", "expected_state", "base_ref", "head_ref", "head_sha", "manifest_path", "manifest_sha256"}
        exact(source, source_keys, set(), f"{label}.source")
        source_repo = pattern(source["repository"], REPO, f"{label}.source.repository", "invalid_repository")
        if ".." in source_repo:
            raise AuditError("invalid_repository", source_repo)
        as_int(source["pull_request"], f"{label}.source.pull_request", 1, 10_000_000)
        if source["expected_state"] not in {"open", "merged"}:
            raise AuditError("contract_mismatch", f"{label}.expected_state")
        as_str(source["base_ref"], f"{label}.base_ref")
        as_str(source["head_ref"], f"{label}.head_ref")
        pattern(source["head_sha"], SHA1, f"{label}.head_sha", "invalid_digest")
        eq(source["manifest_path"], ".github/alex-main-agent.json", f"{label}.manifest_path")
        pattern(source["manifest_sha256"], SHA256, f"{label}.manifest_sha256", "invalid_digest")

        slack = as_obj(entry["slack"], f"{label}.slack")
        exact(slack, {"channel_id", "channel_name"}, {"rejected_channel_ids"}, f"{label}.slack")
        channel = pattern(slack["channel_id"], SLACK, f"{label}.channel_id", "invalid_slack_id")
        name = pattern(slack["channel_name"], re.compile(r"[a-z0-9][a-z0-9-]{0,79}"), f"{label}.channel_name", "invalid_channel_name")
        rejected = string_list(slack.get("rejected_channel_ids", []), f"{label}.rejected_channel_ids")
        if channel in rejected:
            raise AuditError("contract_mismatch", f"{name} rejects canonical channel")

        linear = as_obj(entry["linear"], f"{label}.linear")
        lin_req = {"project", "routing_issue", "delivery_issue"}
        lin_opt = {"canonical_repository_bootstrap_issue", "canonical_mcp_bootstrap_issue"}
        exact(linear, lin_req, lin_opt, f"{label}.linear")
        as_str(linear["project"], f"{label}.linear.project")
        routing_issue = pattern(linear["routing_issue"], ISSUE, f"{label}.routing_issue", "invalid_issue")
        delivery_issue = pattern(linear["delivery_issue"], ISSUE, f"{label}.delivery_issue", "invalid_issue")
        for key in lin_opt & set(linear):
            pattern(linear[key], ISSUE, f"{label}.{key}", "invalid_issue")

        github = as_obj(entry["github"], f"{label}.github")
        gh_req, gh_opt = {"organization", "repository"}, {"legacy_repository", "temporary_execution_target"}
        exact(github, gh_req, gh_opt, f"{label}.github")
        expected_repo = f"{as_str(github['organization'], f'{label}.organization')}/{as_str(github['repository'], f'{label}.repository')}"
        eq(source_repo, expected_repo, f"{label} source/GitHub repository")
        for key in gh_opt & set(github):
            if not isinstance(github[key], bool):
                raise AuditError("invalid_type", f"{label}.{key}")

        central = as_obj(entry["central"], f"{label}.central")
        exact(central, {"linear_project_id", "default_repository"}, set(), f"{label}.central")
        pattern(central["linear_project_id"], re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"), f"{label}.project_id", "invalid_uuid")
        eq(central["default_repository"], source_repo, f"{label} central/source repository")

        for value, seen, code in ((channel, channels, "duplicate_channel"), (source_repo.lower(), repos, "duplicate_repository"), (routing_issue, routing_issues, "duplicate_issue"), (delivery_issue, delivery_issues, "duplicate_issue")):
            if value in seen:
                raise AuditError(code, value)
            seen.add(value)
        entries.append(LockEntry(source, slack, linear, github, central))

    if REJECTED_DAEDALUS in channels:
        raise AuditError("rejected_channel_mapped", REJECTED_DAEDALUS)
    by_name = {entry.slack["channel_name"]: entry for entry in entries}
    if by_name["daedalus-fab"].slack.get("rejected_channel_ids") != [REJECTED_DAEDALUS]:
        raise AuditError("contract_mismatch", "Daedalus typo-channel rejection")
    meme, vox = by_name["memebank"], by_name["voxletra"]
    if meme.github.get("legacy_repository") is not True or meme.linear.get("canonical_repository_bootstrap_issue") != "DEN-1043":
        raise AuditError("contract_mismatch", "MemeBank legacy target marker")
    if vox.github.get("temporary_execution_target") is not True:
        raise AuditError("contract_mismatch", "Voxletra temporary target marker")
    if vox.linear.get("canonical_mcp_bootstrap_issue") != "DEN-164":
        raise AuditError("contract_mismatch", "Voxletra canonical bootstrap issue")
    return Lock(reg, policy, tuple(entries))


def validate_registry(raw: Any, lock: Lock) -> None:
    obj = as_obj(raw, "central_registry")
    exact(obj, {"schema_version", "bindings"}, set(), "central_registry")
    eq(as_int(obj["schema_version"], "central_registry.schema_version", 1, 1), 1, "registry schema")
    bindings = as_arr(obj["bindings"], "central_registry.bindings")
    if len(bindings) != COUNT:
        raise AuditError("contract_mismatch", f"expected {COUNT} central bindings")
    required = {"workspace_id", "channel_id", "linear_team_id", "linear_team_key", "linear_project_id", "default_repository", "repository_allowlist", "default_agent_mode", "allowed_agent_modes", "allowed_user_ids", "allowed_user_group_ids", "write_policy", "budget_policy", "updated_by", "updated_at"}
    indexed, repos = {}, set()
    for i, raw_binding in enumerate(bindings):
        label = f"central_registry.bindings[{i}]"
        binding = as_obj(raw_binding, label)
        exact(binding, required, set(), label)
        eq(binding["workspace_id"], WORKSPACE, f"{label}.workspace")
        channel = pattern(binding["channel_id"], SLACK, f"{label}.channel", "invalid_slack_id")
        if channel == REJECTED_DAEDALUS:
            raise AuditError("rejected_channel_mapped", channel)
        if channel in indexed:
            raise AuditError("duplicate_channel", channel)
        eq(binding["linear_team_id"], lock.policy["linear_team_id"], f"{label}.team_id")
        eq(binding["linear_team_key"], lock.policy["linear_team_key"], f"{label}.team_key")
        repo = pattern(binding["default_repository"], REPO, f"{label}.repository", "invalid_repository")
        eq(string_list(binding["repository_allowlist"], f"{label}.allowlist"), [repo], f"{label}.allowlist")
        if repo.lower() in repos:
            raise AuditError("duplicate_repository", repo)
        repos.add(repo.lower())
        for key in ("default_agent_mode", "allowed_agent_modes", "allowed_user_ids", "allowed_user_group_ids", "write_policy"):
            eq(binding[key], lock.policy[key], f"{label}.{key}")
        eq(budget(binding["budget_policy"], f"{label}.budget"), lock.policy["budget_policy"], f"{label}.budget")
        as_str(binding["linear_project_id"], f"{label}.project_id")
        as_str(binding["updated_by"], f"{label}.updated_by")
        as_str(binding["updated_at"], f"{label}.updated_at")
        indexed[channel] = binding
    if set(indexed) != {entry.slack["channel_id"] for entry in lock.entries}:
        raise AuditError("contract_mismatch", "central/lock channel set")
    for entry in lock.entries:
        binding = indexed[entry.slack["channel_id"]]
        eq(binding["linear_project_id"], entry.central["linear_project_id"], f"{entry.slack['channel_name']} project UUID")
        eq(binding["default_repository"], entry.central["default_repository"], f"{entry.slack['channel_name']} repository")


def validate_manifest(raw: Any, entry: LockEntry) -> dict[str, Any]:
    label = entry.source["repository"]
    obj = as_obj(raw, f"manifest:{label}")
    exact(obj, {"version", "slack", "linear", "github", "routing"}, set(), f"manifest:{label}")
    eq(obj["version"], 1, f"manifest:{label}.version")
    slack = as_obj(obj["slack"], f"manifest:{label}.slack")
    exact(slack, {"workspace_id", "app_id", "channel_id", "channel_name"}, {"rejected_channel_ids"}, f"manifest:{label}.slack")
    expected_slack = {"workspace_id": WORKSPACE, "app_id": APP, "channel_id": entry.slack["channel_id"], "channel_name": entry.slack["channel_name"]}
    for key, expected in expected_slack.items():
        eq(slack[key], expected, f"manifest:{label}.slack.{key}")
    eq(slack.get("rejected_channel_ids", []), entry.slack.get("rejected_channel_ids", []), f"manifest:{label}.rejected")
    linear = as_obj(obj["linear"], f"manifest:{label}.linear")
    optional = {"canonical_repository_bootstrap_issue", "canonical_mcp_bootstrap_issue"}
    exact(linear, {"team", "project", "routing_issue", "delivery_issue"}, optional, f"manifest:{label}.linear")
    eq(linear["team"], TEAM, f"manifest:{label}.team")
    for key in {"project", "routing_issue", "delivery_issue"} | optional:
        if key in linear or key in entry.linear:
            eq(linear.get(key), entry.linear.get(key), f"manifest:{label}.linear.{key}")
    github = as_obj(obj["github"], f"manifest:{label}.github")
    gh_optional = {"legacy_repository", "temporary_execution_target"}
    exact(github, {"organization", "repository"}, gh_optional, f"manifest:{label}.github")
    for key in {"organization", "repository"} | gh_optional:
        if key in github or key in entry.github:
            eq(github.get(key), entry.github.get(key), f"manifest:{label}.github.{key}")
    routing = as_obj(obj["routing"], f"manifest:{label}.routing")
    exact(routing, set(EXPECTED_ROUTING), set(), f"manifest:{label}.routing")
    eq(routing, EXPECTED_ROUTING, f"manifest:{label}.routing")
    return obj


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise AuditError("redirect_rejected", f"HTTP {code}")


class GitHubApiClient:
    def __init__(self, token: str | None = None, timeout: float = 15.0):
        self.opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
        self.token, self.timeout = token.strip() if token else None, timeout

    def get(self, path: str) -> Any:
        if not path.startswith("/repos/") or ".." in path or "//" in path:
            raise AuditError("invalid_api_path", "GitHub API path")
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "alex-main-agent-manifest-audit/1"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(f"https://api.github.com{path}", headers=headers)
        try:
            with self.opener.open(request, timeout=self.timeout) as response:
                if response.status != 200:
                    raise AuditError("github_http_error", f"HTTP {response.status}")
                if response.headers.get_content_type() not in {"application/json", "application/vnd.github+json"}:
                    raise AuditError("invalid_content_type", "GitHub API")
                length = response.headers.get("Content-Length")
                try:
                    if length and int(length) > MAX_REMOTE:
                        raise AuditError("remote_body_too_large", "Content-Length")
                except ValueError:
                    raise AuditError("invalid_content_length", "GitHub API") from None
                data = response.read(MAX_REMOTE + 1)
        except AuditError:
            raise
        except urllib.error.HTTPError as error:
            raise AuditError("github_http_error", f"HTTP {error.code}") from None
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise AuditError("github_transport_error", error.__class__.__name__) from None
        if len(data) > MAX_REMOTE:
            raise AuditError("remote_body_too_large", "stream")
        return parse_json_bytes(data, "GitHub API")

    def fetch(self, entry: LockEntry) -> tuple[Any, Any]:
        source = entry.source
        repo, number, sha = source["repository"], source["pull_request"], source["head_sha"]
        pr = self.get(f"/repos/{repo}/pulls/{number}")
        path = "/".join(urllib.parse.quote(part, safe="") for part in source["manifest_path"].split("/"))
        content = as_obj(self.get(f"/repos/{repo}/contents/{path}?ref={urllib.parse.quote(sha, safe='')}"), "GitHub content")
        exact(content, {"type", "encoding", "size", "content", "sha", "name", "path", "url", "git_url", "html_url", "download_url", "_links"}, set(), "GitHub content")
        if content["type"] != "file" or content["encoding"] != "base64":
            raise AuditError("invalid_remote_manifest", "expected base64 file")
        size = as_int(content["size"], "manifest size", 1, MAX_MANIFEST)
        try:
            raw = base64.b64decode(as_str(content["content"], "manifest content"), validate=True)
        except (ValueError, binascii.Error):
            raise AuditError("invalid_remote_manifest", "invalid base64") from None
        if len(raw) != size or len(raw) > MAX_MANIFEST:
            raise AuditError("invalid_remote_manifest", "size mismatch")
        return pr, parse_json_bytes(raw, f"manifest:{repo}")


RemoteFetcher = Callable[[LockEntry], tuple[Any, Any]]


def validate_pr(raw: Any, entry: LockEntry) -> None:
    label, source = entry.source["repository"], entry.source
    pr = as_obj(raw, f"pull_request:{label}")
    state, merged = as_str(pr.get("state"), f"pull_request:{label}.state"), pr.get("merged_at")
    if source["expected_state"] == "open" and (state != "open" or merged is not None):
        raise AuditError("pull_request_state_drift", label)
    if source["expected_state"] == "merged" and (state != "closed" or not isinstance(merged, str) or not merged):
        raise AuditError("pull_request_state_drift", label)
    eq(as_int(pr.get("number"), f"pull_request:{label}.number", 1, 10_000_000), source["pull_request"], f"pull_request:{label}.number")
    base, head = as_obj(pr.get("base"), f"pull_request:{label}.base"), as_obj(pr.get("head"), f"pull_request:{label}.head")
    eq(base.get("ref"), source["base_ref"], f"pull_request:{label}.base.ref")
    eq(head.get("ref"), source["head_ref"], f"pull_request:{label}.head.ref")
    eq(head.get("sha"), source["head_sha"], f"pull_request:{label}.head.sha")
    full_name = as_str(as_obj(head.get("repo"), f"pull_request:{label}.head.repo").get("full_name"), f"pull_request:{label}.head.repo.full_name")
    if full_name.lower() != source["repository"].lower():
        raise AuditError("repository_escape", label)


def audit(registry_path: Path, lock_path: Path, remote_fetcher: RemoteFetcher | None = None) -> dict[str, Any]:
    lock = validate_lock(load_json(lock_path, MAX_LOCK, "manifest lock"))
    registry = parse_json_bytes(read_bounded(registry_path, MAX_REGISTRY, "central registry"), "central registry")
    registry_digest = canonical_json_sha256(registry)
    eq(registry_digest, lock.registry["canonical_sha256"], "central registry canonical SHA-256")
    validate_registry(registry, lock)
    manifests = []
    for entry in lock.entries:
        status = "locked"
        if remote_fetcher:
            pr, manifest = remote_fetcher(entry)
            validate_pr(pr, entry)
            validated = validate_manifest(manifest, entry)
            eq(canonical_json_sha256(validated), entry.source["manifest_sha256"], f"{entry.source['repository']} canonical manifest digest")
            status = "verified"
        manifests.append({"repository": entry.source["repository"], "pull_request": entry.source["pull_request"], "head_sha": entry.source["head_sha"], "manifest_sha256": entry.source["manifest_sha256"], "channel_id": entry.slack["channel_id"], "linear_project_id": entry.central["linear_project_id"], "routing_issue": entry.linear["routing_issue"], "delivery_issue": entry.linear["delivery_issue"], "status": status})
    return {"schema_version": 1, "status": "pass", "mode": "remote" if remote_fetcher else "offline", "central_registry": {"path": lock.registry["path"], "canonical_sha256": registry_digest, "bindings": len(manifests)}, "manifests": manifests}


def write_report(path: Path, report: Mapping[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(report, ensure_ascii=False, sort_keys=True, indent=2) + "\n"
    if len(encoded.encode()) > MAX_REMOTE:
        raise AuditError("report_too_large", str(path))
    path.write_text(encoded, encoding="utf-8")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry", type=Path, default=Path("config/alex-main-agent.channels.json"))
    parser.add_argument("--lock", type=Path, default=Path("config/alex-main-agent.manifests.lock.json"))
    parser.add_argument("--report", type=Path, default=Path("artifacts/alex-main-agent-manifest-audit.json"))
    parser.add_argument("--remote", action="store_true")
    args = parser.parse_args(argv or sys.argv[1:])
    try:
        client = GitHubApiClient(os.environ.get("GITHUB_TOKEN")) if args.remote else None
        report = audit(args.registry, args.lock, client.fetch if client else None)
        write_report(args.report, report)
        print(json.dumps({"bindings": report["central_registry"]["bindings"], "mode": report["mode"], "status": "pass"}, sort_keys=True))
        return 0
    except AuditError as error:
        report = {"schema_version": 1, "status": "fail", "mode": "remote" if args.remote else "offline", "finding": {"code": error.code, "detail": error.detail[:256]}}
        try:
            write_report(args.report, report)
        except OSError:
            pass
        print(json.dumps(report, sort_keys=True), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
