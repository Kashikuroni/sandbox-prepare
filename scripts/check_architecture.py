"""Check actual Cargo workspace dependencies against the approved boundaries."""
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ALLOWED = {
    "sandbox_core": set(),
    "sandbox_platform": {"sandbox_core"},
    "sandbox_telemetry": {"sandbox_core", "sandbox_platform"},
    "sandbox_ui": {"sandbox_core"},
    "sandbox_settings": {"sandbox_core", "sandbox_platform", "sandbox_telemetry"},
    "sandbox_backups": {"sandbox_core", "sandbox_platform", "sandbox_telemetry"},
    "sandbox_projects": {"sandbox_core", "sandbox_platform", "sandbox_telemetry"},
    "sandbox_sandboxes": {
        "sandbox_core", "sandbox_platform", "sandbox_telemetry",
        "sandbox_backups", "sandbox_projects",
    },
    "sandbox_runtime": {
        "sandbox_core", "sandbox_platform", "sandbox_telemetry", "sandbox_settings",
        "sandbox_backups", "sandbox_projects", "sandbox_sandboxes",
    },
    "sandbox_workspace": {
        "sandbox_core", "sandbox_ui", "sandbox_settings", "sandbox_platform",
        "sandbox_telemetry", "sandbox_backups", "sandbox_projects",
        "sandbox_sandboxes", "sandbox_runtime",
    },
    "sandbox-prepare": {
        "sandbox_workspace", "sandbox_runtime", "sandbox_sandboxes",
    },
}


def check():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--offline", "--format-version", "1", "--no-deps"],
        cwd=ROOT, text=True,
    ))
    members = {p["name"]: p for p in metadata["packages"]
               if p["id"] in metadata["workspace_members"]}
    errors = []
    for name, package in members.items():
        if name not in ALLOWED:
            errors.append(f"Unspecified workspace boundary: {name}")
            continue
        for dependency in package["dependencies"]:
            target = dependency["name"]
            if target in members and target not in ALLOWED[name]:
                errors.append(f"Forbidden dependency: {name} -> {target}")
            if name in {"sandbox_core", "sandbox_platform", "sandbox_telemetry"} and target == "gpui":
                errors.append(f"UI dependency in foundational package: {name}")
            if target.startswith("aws-") and name != "sandbox_backups":
                errors.append(f"S3 SDK escaped the backups boundary: {name} -> {target}")
    if errors:
        raise SystemExit("\n".join(errors))
    print("Cargo architecture boundaries verified")


if __name__ == "__main__":
    check()
