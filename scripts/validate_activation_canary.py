#!/usr/bin/env python3
"""Validate the bounded DEN-847 activation plan without touching a cluster."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
PLAN_PATH = ROOT / "canary" / "den-847.activation-plan.json"
SCHEMA_PATH = ROOT / "canary" / "activation-plan.schema.json"
SCHEMA_VERSION = "ai-agent-bridge.activation-canary.v1"
PRECONDITIONS = {
    "DEN-845",
    "DEN-391",
    "DEN-680",
    "DEN-1227",
    "DEN-287",
    "DEN-523",
    "DEN-601",
}
WORKFLOW_MODES = {
    "single",
    "sequential",
    "competitive",
    "consensus",
    "blind_competition",
}
FAILURE_DRILLS = {
    "cancellation",
    "provider_failure",
    "budget_exhaustion",
    "lease_loss",
    "stale_claim",
    "runner_drain",
    "bridge_restart",
}
REQUIRED_EVIDENCE = {
    "admission_record",
    "provider_reservation",
    "normalized_usage",
    "submission_record",
    "lease_lifecycle",
    "claim_lifecycle",
    "readiness_transition",
    "scale_to_zero",
}
BUDGET_BOUNDS = {
    "maxProviderCalls": (5, 20),
    "maxCostUsdMicros": (1, 10_000_000),
    "maxWallClockSeconds": (60, 3_600),
    "maxPromptBytes": (1, 16_384),
    "maxOutputBytes": (1, 131_072),
}
PROVIDER_ID = re.compile(r"^[a-z][a-z0-9_-]{1,63}$")
CREDENTIAL_REFERENCE = re.compile(
    r"^external-secret://[a-z0-9][a-z0-9/._-]{2,191}$"
)
IMAGE_DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
SENSITIVE_VALUE = re.compile(
    r"(?:bearer\s+|(?:api[_-]?key|password|private[_-]?key|access[_-]?token)\s*[:=])",
    re.IGNORECASE,
)
URL_SCHEME = re.compile(r"[a-z][a-z0-9+.-]*://", re.IGNORECASE)


def fail(message: str) -> None:
    raise SystemExit(message)


def require_exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    observed = set(value)
    if observed != expected:
        fail(
            f"{context} keys drifted: missing={sorted(expected - observed)} "
            f"unexpected={sorted(observed - expected)}"
        )


def require_exact_set(value: Any, expected: set[str], context: str) -> None:
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        fail(f"{context} must be an array of strings")
    if len(value) != len(set(value)):
        fail(f"{context} contains duplicate entries")
    if set(value) != expected:
        fail(
            f"{context} drifted: missing={sorted(expected - set(value))} "
            f"unexpected={sorted(set(value) - expected)}"
        )


def walk_strings(value: Any) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, dict):
        strings: list[str] = []
        for child in value.values():
            strings.extend(walk_strings(child))
        return strings
    if isinstance(value, list):
        strings: list[str] = []
        for child in value:
            strings.extend(walk_strings(child))
        return strings
    return []


def validate_schema(schema: dict[str, Any]) -> None:
    if schema.get("$schema") != "https://json-schema.org/draft/2020-12/schema":
        fail("activation plan schema must use JSON Schema Draft 2020-12")
    if schema.get("additionalProperties") is not False:
        fail("activation plan schema must reject unknown root fields")
    version = schema.get("properties", {}).get("schemaVersion", {}).get("const")
    if version != SCHEMA_VERSION:
        fail("activation plan schema version authority drifted")


def validate_plan(plan: dict[str, Any]) -> None:
    require_exact_keys(
        plan,
        {
            "schemaVersion",
            "ticket",
            "enabled",
            "activationBlocked",
            "provider",
            "runner",
            "preconditions",
            "budgets",
            "workflowModes",
            "failureDrills",
            "requiredEvidence",
            "fixturePolicy",
        },
        "activation plan",
    )
    if plan["schemaVersion"] != SCHEMA_VERSION:
        fail("activation plan schema version drifted")
    if plan["ticket"] != "DEN-847":
        fail("activation plan lost its Linear owner")
    if not isinstance(plan["enabled"], bool) or not isinstance(
        plan["activationBlocked"], bool
    ):
        fail("enabled and activationBlocked must be booleans")

    provider = plan["provider"]
    if not isinstance(provider, dict):
        fail("provider must be an object")
    require_exact_keys(provider, {"count", "providerId", "credentialReference"}, "provider")
    if provider["count"] != 1 or isinstance(provider["count"], bool):
        fail("activation canary must select exactly one provider")

    runner = plan["runner"]
    if not isinstance(runner, dict):
        fail("runner must be an object")
    require_exact_keys(
        runner,
        {
            "activeReplicas",
            "rollbackReplicas",
            "imageDigest",
            "scaleToZeroOnAnyFailure",
        },
        "runner",
    )
    if runner["activeReplicas"] != 1 or isinstance(runner["activeReplicas"], bool):
        fail("activation canary must use exactly one runner replica")
    if runner["rollbackReplicas"] != 0 or isinstance(runner["rollbackReplicas"], bool):
        fail("activation canary rollback target must be zero replicas")
    if runner["scaleToZeroOnAnyFailure"] is not True:
        fail("activation canary must scale to zero on every failure")

    preconditions = plan["preconditions"]
    if not isinstance(preconditions, dict):
        fail("preconditions must be an object")
    require_exact_keys(preconditions, PRECONDITIONS, "preconditions")
    if any(not isinstance(value, bool) for value in preconditions.values()):
        fail("every precondition must be an explicit boolean")

    budgets = plan["budgets"]
    if not isinstance(budgets, dict):
        fail("budgets must be an object")
    require_exact_keys(budgets, set(BUDGET_BOUNDS), "budgets")
    for name, (minimum, maximum) in BUDGET_BOUNDS.items():
        value = budgets[name]
        if not isinstance(value, int) or isinstance(value, bool):
            fail(f"{name} must be an integer")
        if not minimum <= value <= maximum:
            fail(f"{name} must be between {minimum} and {maximum}")

    require_exact_set(plan["workflowModes"], WORKFLOW_MODES, "workflowModes")
    require_exact_set(plan["failureDrills"], FAILURE_DRILLS, "failureDrills")
    require_exact_set(plan["requiredEvidence"], REQUIRED_EVIDENCE, "requiredEvidence")

    fixture = plan["fixturePolicy"]
    if not isinstance(fixture, dict):
        fail("fixturePolicy must be an object")
    require_exact_keys(
        fixture,
        {
            "fixtureIdPrefix",
            "allowRepositoryWrites",
            "allowPromptOrOutputInEvidence",
            "allowCredentialValues",
        },
        "fixturePolicy",
    )
    if fixture["fixtureIdPrefix"] != "canary:den-847:":
        fail("canary fixture namespace drifted")
    for field in (
        "allowRepositoryWrites",
        "allowPromptOrOutputInEvidence",
        "allowCredentialValues",
    ):
        if fixture[field] is not False:
            fail(f"fixturePolicy.{field} must remain false")

    all_ready = all(preconditions.values())
    provider_id = provider["providerId"]
    credential_reference = provider["credentialReference"]
    image_digest = runner["imageDigest"]

    if not all_ready:
        if plan["enabled"] is not False or plan["activationBlocked"] is not True:
            fail("activation must stay blocked while any prerequisite is false")
        if any(value is not None for value in (provider_id, credential_reference, image_digest)):
            fail("blocked plans must not bind a provider, credential reference, or image digest")
    else:
        if plan["enabled"] is plan["activationBlocked"]:
            fail("an all-ready plan must be either enabled or explicitly blocked, never both/neither")
        if provider_id is None or PROVIDER_ID.fullmatch(provider_id) is None:
            fail("ready plans require one canonical providerId")
        if (
            credential_reference is None
            or CREDENTIAL_REFERENCE.fullmatch(credential_reference) is None
        ):
            fail("ready plans require an ExternalSecret reference, never a value")
        if image_digest is None or IMAGE_DIGEST.fullmatch(image_digest) is None:
            fail("ready plans require an exact sha256 image digest")

    for value in walk_strings(plan):
        if SENSITIVE_VALUE.search(value):
            fail("secret-like material entered the activation plan")
        schemes = URL_SCHEME.findall(value)
        if schemes and not value.startswith("external-secret://"):
            fail(f"network URL entered activation plan: {value!r}")


def main() -> None:
    schema = json.loads(SCHEMA_PATH.read_text())
    plan = json.loads(PLAN_PATH.read_text())
    if not isinstance(schema, dict) or not isinstance(plan, dict):
        fail("schema and activation plan roots must be objects")
    validate_schema(schema)
    validate_plan(plan)

    blocked = sorted(
        ticket for ticket, ready in plan["preconditions"].items() if not ready
    )
    state = "enabled" if plan["enabled"] else "disabled"
    print(
        f"validated {SCHEMA_VERSION}: {state}, one provider, one runner, "
        f"rollback=0, blocked_by={','.join(blocked) or 'none'}"
    )


if __name__ == "__main__":
    main()
