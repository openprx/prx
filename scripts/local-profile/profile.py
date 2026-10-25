#!/usr/bin/env python3
"""Create an isolated, versioned local-model/Office acceptance profile."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import urllib.request

HERE = Path(__file__).resolve().parent
SKILLS_REVISION = "53048666b05b4799081517d00e09e0a2dd688678"
MODEL = "qwen38-27b-splash"
PROVIDER = "custom:http://127.0.0.1:18086/v1"
PYTHON_VERSION = "3.14.6"
NODE_VERSION = "v24.15.0"
LIBREOFFICE_VERSION = "26.8.1.1"
POPPLER_VERSION = "26.10.0"


def run(*args, **kwargs):
    return subprocess.check_output([str(x) for x in args], text=True, **kwargs).strip()


def sha(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def quoted(value):
    # JSON basic strings are also TOML basic strings for these paths and IDs.
    return json.dumps(str(value))


def environment(root, node):
    return {
        "HOME": str(root / "home"),
        "PATH": os.pathsep.join([str(root / "bin"), str(root / "python/bin"),
                                 str(Path(node).parent), "/usr/bin", "/bin", "/usr/sbin", "/sbin"]),
        "NODE_PATH": str(root / "node/node_modules"),
        "TMPDIR": str(root / "config/workspace/tmp"),
        "OPENPRX_CONFIG_DIR": str(root / "config"),
        "TERM": os.environ.get("TERM", "xterm-256color"),
        "LANG": "en_US.UTF-8",
        "PRX_TUI": "1",
    }


def create(args):
    root = args.profile.expanduser().resolve()
    # Never overwrite a live profile, configuration, or installation.
    if root.exists():
        raise ValueError(f"Profile already exists: {root}; select a fresh directory")
    if run(sys.executable, "--version") != f"Python {PYTHON_VERSION}":
        raise ValueError(f"Use Python {PYTHON_VERSION} to reproduce this profile")
    node = Path(shutil.which("node") or "").resolve()
    npm = shutil.which("npm")
    if not npm or run(node, "--version") != NODE_VERSION:
        raise ValueError(f"Use Node {NODE_VERSION} with npm on PATH")
    soffice = args.soffice.expanduser().resolve()
    lo_version = run(soffice, "--version")
    if not lo_version.startswith(f"LibreOffice {LIBREOFFICE_VERSION} "):
        raise ValueError(f"Use LibreOffice {LIBREOFFICE_VERSION}; observed {lo_version}")
    poppler = args.poppler_bin.expanduser().resolve()
    for tool in ["pdftoppm", "pdftotext"]:
        version = run(poppler / tool, "-v", stderr=subprocess.STDOUT)
        if not version.startswith(f"{tool} version {POPPLER_VERSION}"):
            raise ValueError(f"Use Poppler {POPPLER_VERSION}; observed {version.splitlines()[0]}")
    source = args.skills_source.expanduser().resolve()
    if run("git", "-C", source, "rev-parse", f"{SKILLS_REVISION}^{{commit}}") != SKILLS_REVISION:
        raise ValueError("Pinned Office skill revision is unavailable")
    archive = subprocess.check_output([
        "git", "-C", str(source), "archive", SKILLS_REVISION,
        "skills/docx", "skills/pptx", "skills/pdf", "skills/xlsx",
    ])
    root.mkdir(parents=True, mode=0o700)
    workspace = root / "config/workspace"
    for directory in [root / "home", root / "bin", workspace / "tmp", root / "node"]:
        directory.mkdir(parents=True, mode=0o700)
    with tarfile.open(fileobj=io.BytesIO(archive)) as bundle:
        bundle.extractall(workspace, filter="data")
    (root / "bin/soffice").symlink_to(soffice)
    for tool in ["pdftoppm", "pdftotext"]:
        (root / "bin" / tool).symlink_to(poppler / tool)
    for name in ["package.json", "package-lock.json"]:
        shutil.copyfile(HERE / name, root / "node" / name)
    subprocess.run([npm, "ci", "--ignore-scripts", "--no-audit", "--no-fund"],
                   cwd=root / "node", check=True)
    for name, lock in [("python", "requirements.lock"), ("powerpoint", "powerpoint.lock")]:
        subprocess.run([sys.executable, "-m", "venv", str(root / name)], check=True)
        subprocess.run([str(root / name / "bin/python"), "-m", "pip", "install",
                        "--require-hashes", "-r", str(HERE / lock)], check=True)
    config = f'''default_provider = {quoted(PROVIDER)}
default_model = {quoted(MODEL)}
default_temperature = 0.0

[causal_tree]
schema_version = 2
enabled = true

[causal_tree.backend]
kind = "builtin"
remote_enabled = false

[tool_tiering]
always_include = ["shell", "skills_manage"]

# This local inference service has no provider API charge.
[cost.prices.{quoted(PROVIDER + "/" + MODEL)}]
input = 0.0
output = 0.0
cache_read = 0.0
cache_write = 0.0

[[providers.models]]
provider = {quoted(PROVIDER)}
model = {quoted(MODEL)}
operations = ["chat"]
native_tools = true

[runtime]
shell_env_allowlist = ["NODE_PATH"]
'''
    (root / "config/config.toml").write_text(config)
    # Optional operator configuration, not implicitly enabled by the workspace.
    (root / "powerpoint-mcp.toml").write_text(f'''
[mcp.servers.powerpoint]
command = {quoted(root / "powerpoint/bin/ppt_mcp_server")}
args = []
working_dir = {quoted(workspace)}
''')
    receipt = {
        "skills_revision": SKILLS_REVISION,
        "skill_archive_sha256": hashlib.sha256(archive).hexdigest(),
        "python": run(sys.executable, "--version"),
        "node": str(node), "node_version": run(node, "--version"),
        "npm_version": run(npm, "--version"),
        "soffice": str(soffice), "libreoffice_version": lo_version,
        "soffice_sha256": sha(soffice),
        "poppler_version": POPPLER_VERSION,
        "poppler_sha256": {name: sha(poppler / name) for name in ["pdftoppm", "pdftotext"]},
        "provider": PROVIDER, "model": MODEL, "decision_backend": "builtin",
        "inputs": {name: sha(HERE / name) for name in
                   ["requirements.lock", "powerpoint.lock", "package-lock.json", "profile.py"]},
        "acceptance": "Setup only; real model, Office and Jev acceptance must be recorded separately.",
    }
    (root / "profile.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(check(root, probe_model=False), indent=2))


def check(root, probe_model):
    receipt = json.loads((root / "profile.json").read_text())
    env = environment(root, receipt["node"])
    result = {"profile": str(root), "decision_backend": receipt["decision_backend"]}
    for name in ["python", "powerpoint"]:
        result[name + "_dependencies"] = run(root / name / "bin/python", "-m", "pip", "check")
    result["office_imports"] = run(root / "python/bin/python", "-c",
        "import docx, pptx, openpyxl, reportlab, pypdf, pymupdf, lxml, defusedxml, markitdown, pdf2image, pdfplumber; print('ok')", env=env)
    result["node_packages"] = json.loads(run(receipt["node"], "-e",
        "console.log(JSON.stringify(['docx','pptxgenjs'].map(x=>require.resolve(x))))", env=env))
    result["powerpoint_mcp"] = run(root / "powerpoint/bin/python", "-c",
        "from mcp.server.fastmcp import FastMCP; import ppt_mcp_server; print('ok')", env=env)
    result["libreoffice"] = run(root / "bin/soffice", "--version", env=env)
    result["poppler"] = run(root / "bin/pdftoppm", "-v", env=env, stderr=subprocess.STDOUT).splitlines()[0]
    if probe_model:
        request = urllib.request.Request(PROVIDER.removeprefix("custom:") + "/models")
        # Do not send localhost readiness requests through inherited proxies.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=10) as response:
            ids = [entry["id"] for entry in json.load(response)["data"]]
        if MODEL not in ids:
            raise ValueError(f"Expected served model {MODEL}; observed {ids}")
        result["served_model"] = MODEL
    result["scope"] = "Prerequisites only; not an execution or real Jev acceptance result."
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    setup = sub.add_parser("create")
    setup.add_argument("profile", type=Path)
    setup.add_argument("--skills-source", type=Path, required=True)
    setup.add_argument("--soffice", type=Path, required=True)
    setup.add_argument("--poppler-bin", type=Path, required=True)
    verify = sub.add_parser("check")
    verify.add_argument("profile", type=Path)
    verify.add_argument("--probe-model", action="store_true")
    launch = sub.add_parser("launch")
    launch.add_argument("profile", type=Path)
    launch.add_argument("--prx", type=Path, required=True)
    args = parser.parse_args()
    root = args.profile.expanduser().resolve()
    if args.command == "create":
        create(args)
    elif args.command == "check":
        print(json.dumps(check(root, args.probe_model), indent=2))
    else:
        receipt = json.loads((root / "profile.json").read_text())
        binary = args.prx.expanduser().resolve()
        os.chdir(root / "config/workspace")
        os.execve(binary, [str(binary), "chat"], environment(root, receipt["node"]))


if __name__ == "__main__":
    main()
