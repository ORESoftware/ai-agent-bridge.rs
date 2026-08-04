from __future__ import annotations

import copy
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = ROOT / "scripts" / "audit_alex_main_agent_manifests.py"
SPEC = importlib.util.spec_from_file_location("manifest_audit", MODULE_PATH)
assert SPEC and SPEC.loader
manifest_audit = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = manifest_audit
SPEC.loader.exec_module(manifest_audit)


class ManifestAuditTest(unittest.TestCase):
    def setUp(self) -> None:
        self.registry = json.loads((ROOT / "config" / "alex-main-agent.channels.json").read_text())
        self.lock = json.loads((ROOT / "config" / "alex-main-agent.manifests.lock.json").read_text())
        self.manifests = {}
        for entry in self.lock["entries"]:
            slack = {
                "workspace_id": self.lock["workspace_id"],
                "app_id": self.lock["app_id"],
                "channel_id": entry["slack"]["channel_id"],
                "channel_name": entry["slack"]["channel_name"],
            }
            if "rejected_channel_ids" in entry["slack"]:
                slack["rejected_channel_ids"] = entry["slack"]["rejected_channel_ids"]
            linear = {"team": self.lock["linear_team"], **entry["linear"]}
            self.manifests[entry["source"]["repository"]] = {
                "version": 1,
                "slack": slack,
                "linear": linear,
                "github": entry["github"],
                "routing": copy.deepcopy(manifest_audit.EXPECTED_ROUTING),
            }

    def _write(self, directory: Path, name: str, value: object) -> Path:
        path = directory / name
        path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
        return path

    def _pr(self, entry: manifest_audit.LockEntry) -> dict[str, object]:
        source = entry.source
        return {
            "number": source["pull_request"],
            "state": "open",
            "merged_at": None,
            "base": {"ref": source["base_ref"]},
            "head": {
                "ref": source["head_ref"],
                "sha": source["head_sha"],
                "repo": {"full_name": source["repository"]},
            },
        }

    def _fetcher(self, mutate_pr=None, mutate_manifest=None):
        def fetch(entry: manifest_audit.LockEntry):
            pr = self._pr(entry)
            manifest = copy.deepcopy(self.manifests[entry.source["repository"]])
            if mutate_pr:
                mutate_pr(entry, pr)
            if mutate_manifest:
                mutate_manifest(entry, manifest)
            return pr, manifest

        return fetch

    def _audit(self, registry=None, lock=None, fetcher=None):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            registry_path = self._write(directory, "registry.json", registry if registry is not None else self.registry)
            lock_value = copy.deepcopy(lock if lock is not None else self.lock)
            lock_value["central_registry"]["canonical_sha256"] = manifest_audit.canonical_json_sha256(
                registry if registry is not None else self.registry
            )
            lock_path = self._write(directory, "lock.json", lock_value)
            return manifest_audit.audit(registry_path, lock_path, fetcher)

    def test_remote_audit_verifies_all_thirteen_exact_heads(self):
        report = self._audit(fetcher=self._fetcher())
        self.assertEqual(report["status"], "pass")
        self.assertEqual(report["mode"], "remote")
        self.assertEqual(report["central_registry"]["bindings"], 13)
        self.assertEqual({item["status"] for item in report["manifests"]}, {"verified"})

    def test_pr_head_move_fails_closed(self):
        def mutate(entry, pr):
            if entry.slack["channel_name"] == "cliptown":
                pr["head"]["sha"] = "0" * 40

        with self.assertRaisesRegex(manifest_audit.AuditError, "contract_mismatch"):
            self._audit(fetcher=self._fetcher(mutate_pr=mutate))

    def test_repository_escape_fails_closed(self):
        def mutate(entry, pr):
            if entry.slack["channel_name"] == "benefactor-cc":
                pr["head"]["repo"]["full_name"] = "attacker/redirected"

        with self.assertRaisesRegex(manifest_audit.AuditError, "repository_escape"):
            self._audit(fetcher=self._fetcher(mutate_pr=mutate))

    def test_removed_idempotency_guardrail_fails_closed(self):
        def mutate(entry, manifest):
            if entry.slack["channel_name"] == "shared-auth":
                del manifest["routing"]["idempotency_source"]

        with self.assertRaisesRegex(manifest_audit.AuditError, "missing_field"):
            self._audit(fetcher=self._fetcher(mutate_manifest=mutate))

    def test_unknown_manifest_field_fails_closed(self):
        def mutate(entry, manifest):
            if entry.slack["channel_name"] == "opto-sync":
                manifest["routing"]["implicit_admin"] = True

        with self.assertRaisesRegex(manifest_audit.AuditError, "unknown_field"):
            self._audit(fetcher=self._fetcher(mutate_manifest=mutate))

    def test_changed_manifest_digest_fails_closed(self):
        def mutate(entry, manifest):
            if entry.slack["channel_name"] == "athlet-o":
                manifest["linear"]["delivery_issue"] = "DEN-9999"

        with self.assertRaisesRegex(manifest_audit.AuditError, "contract_mismatch"):
            self._audit(fetcher=self._fetcher(mutate_manifest=mutate))

    def test_duplicate_channel_in_lock_fails_closed(self):
        lock = copy.deepcopy(self.lock)
        lock["entries"][1]["slack"]["channel_id"] = lock["entries"][0]["slack"]["channel_id"]
        with self.assertRaisesRegex(manifest_audit.AuditError, "duplicate_channel"):
            self._audit(lock=lock)

    def test_misspelled_daedalus_channel_cannot_become_canonical(self):
        registry = copy.deepcopy(self.registry)
        daedalus = next(item for item in registry["bindings"] if item["channel_id"] == "C0BKP3DDDPZ")
        daedalus["channel_id"] = "C0BMB9GSSKY"
        with self.assertRaisesRegex(manifest_audit.AuditError, "rejected_channel_mapped"):
            self._audit(registry=registry)

    def test_temporary_targets_require_explicit_markers(self):
        lock = copy.deepcopy(self.lock)
        voxletra = next(item for item in lock["entries"] if item["slack"]["channel_name"] == "voxletra")
        del voxletra["github"]["temporary_execution_target"]
        with self.assertRaisesRegex(manifest_audit.AuditError, "Voxletra temporary target marker"):
            self._audit(lock=lock)

    def test_duplicate_json_keys_are_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "duplicate.json"
            path.write_text('{"schema_version":1,"schema_version":1}\n', encoding="utf-8")
            with self.assertRaisesRegex(manifest_audit.AuditError, "duplicate_json_key"):
                manifest_audit.load_json(path, 1024, "duplicate fixture")

    def test_report_is_metadata_only(self):
        report = self._audit(fetcher=self._fetcher())
        rendered = json.dumps(report, sort_keys=True)
        forbidden = ["token", "secret", "prompt", "message_body", "channel_history", "Authorization"]
        for marker in forbidden:
            self.assertNotIn(marker, rendered)


if __name__ == "__main__":
    unittest.main()
