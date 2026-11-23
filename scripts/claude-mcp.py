import argparse
import json
import os
import tempfile
from pathlib import Path


def update_config(path, distro):
    try:
        original = path.read_bytes()
    except FileNotFoundError:
        if distro is None:
            return
        original = None

    config = json.loads(original.decode("utf-8-sig")) if original is not None else {}
    if not isinstance(config, dict):
        raise ValueError("Claude Code config must be a JSON object")

    if "mcpServers" not in config:
        if distro is None:
            return
        config["mcpServers"] = {}
    servers = config["mcpServers"]
    if not isinstance(servers, dict):
        raise ValueError("Claude Code mcpServers must be a JSON object")

    if distro is None:
        if "focusstage" not in servers:
            return
        del servers["focusstage"]
    else:
        entry = {
            "type": "stdio",
            "command": "wsl.exe",
            "args": ["-d", distro, "-e", "/usr/local/bin/focusstage", "mcp"],
        }
        if servers.get("focusstage") == entry:
            return
        servers["focusstage"] = entry

    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, prefix=".claude-mcp-", delete=False
    ) as output:
        pending = Path(output.name)
        try:
            json.dump(config, output, indent=2, ensure_ascii=False, allow_nan=False)
            output.write("\n")
            output.close()
            current = path.read_bytes() if path.exists() else None
            if current != original:
                raise ValueError("Claude Code config changed while updating it; retry the command")
            os.replace(pending, path)
        finally:
            pending.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("path", type=Path)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--distro")
    mode.add_argument("--remove", action="store_true")
    args = parser.parse_args()
    try:
        update_config(args.path, args.distro)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Cannot update Claude Code MCP config: {error}\n")


if __name__ == "__main__":
    main()
