import argparse
import json
import os
import tempfile
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11 (e.g. Ubuntu 22.04)
    tomllib = None

SERVER_NAME = "focusstage"
LINUX_BINARY = "/usr/local/bin/focusstage"


def server_spec(distro, native):
    if native:
        return LINUX_BINARY, ["mcp"]
    if distro:
        return "wsl.exe", ["-d", distro, "-e", LINUX_BINARY, "mcp"]
    return None, None


def atomic_write_text(path, content, original, label):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        newline="",
        dir=path.parent,
        prefix=".focusstage-mcp-",
        delete=False,
    ) as output:
        pending = Path(output.name)
        try:
            output.write(content)
            output.close()
            current = path.read_bytes() if path.exists() else None
            if current != original:
                raise ValueError(f"{label} changed while updating it; retry the command")
            os.replace(pending, path)
        finally:
            pending.unlink(missing_ok=True)


def update_claude_config(path, command, args):
    try:
        original = path.read_bytes()
    except FileNotFoundError:
        if command is None:
            return
        original = None

    config = json.loads(original.decode("utf-8-sig")) if original is not None else {}
    if not isinstance(config, dict):
        raise ValueError("Claude Code config must be a JSON object")

    if "mcpServers" not in config:
        if command is None:
            return
        config["mcpServers"] = {}
    servers = config["mcpServers"]
    if not isinstance(servers, dict):
        raise ValueError("Claude Code mcpServers must be a JSON object")

    if command is None:
        if SERVER_NAME not in servers:
            return
        del servers[SERVER_NAME]
    else:
        entry = {
            "type": "stdio",
            "command": command,
            "args": list(args),
        }
        if servers.get(SERVER_NAME) == entry:
            return
        servers[SERVER_NAME] = entry

    rendered = json.dumps(config, indent=2, ensure_ascii=False, allow_nan=False) + "\n"
    atomic_write_text(path, rendered, original, f"Claude Code config ({path})")


def _scan_toml_string_state(line, state):
    """Update multiline TOML string state ('\"\"\"', \"'''\", or None) across one line."""
    i = 0
    n = len(line)
    in_basic = False
    in_literal = False

    while i < n:
        ch = line[i]
        if state == '"""':
            if ch == "\\":
                i += 2
                continue
            if line.startswith('"""', i):
                # Consume up to 2 extra quotes belonging to string content before closing """
                j = i + 3
                while j < n and line[j] == '"' and j - i < 5:
                    j += 1
                state = None
                i = j
                continue
            i += 1
        elif state == "'''":
            if line.startswith("'''", i):
                j = i + 3
                while j < n and line[j] == "'" and j - i < 5:
                    j += 1
                state = None
                i = j
                continue
            i += 1
        elif in_basic:
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                in_basic = False
            i += 1
        elif in_literal:
            if ch == "'":
                in_literal = False
            i += 1
        else:
            if ch == "#":
                break
            if line.startswith('"""', i):
                state = '"""'
                i += 3
            elif line.startswith("'''", i):
                state = "'''"
                i += 3
            elif ch == '"':
                in_basic = True
                i += 1
            elif ch == "'":
                in_literal = True
                i += 1
            else:
                i += 1
    return state


def _parse_toml_header(line):
    """If line is a TOML table header, return (is_array, [segments]); else None."""
    s = line.lstrip(" \t")
    if not s.startswith("["):
        return None
    is_array = s.startswith("[[")
    idx = 2 if is_array else 1
    n = len(s)
    segments = []

    while idx < n:
        while idx < n and s[idx] in " \t":
            idx += 1
        if idx >= n:
            return None

        ch = s[idx]
        if ch == '"':
            idx += 1
            buf = []
            while idx < n:
                c = s[idx]
                if c == "\\":
                    if idx + 1 >= n:
                        return None
                    nxt = s[idx + 1]
                    escapes = {"n": "\n", "r": "\r", "t": "\t", "b": "\b", "f": "\f", '"': '"', "\\": "\\"}
                    buf.append(escapes.get(nxt, nxt))
                    idx += 2
                elif c == '"':
                    idx += 1
                    break
                else:
                    buf.append(c)
                    idx += 1
            else:
                return None
            segments.append("".join(buf))
        elif ch == "'":
            idx += 1
            start = idx
            end = s.find("'", idx)
            if end == -1:
                return None
            segments.append(s[start:end])
            idx = end + 1
        else:
            start = idx
            while idx < n and (s[idx].isalnum() or s[idx] in "_-"):
                idx += 1
            if idx == start:
                return None
            segments.append(s[start:idx])

        while idx < n and s[idx] in " \t":
            idx += 1
        if idx < n and s[idx] == ".":
            idx += 1
            continue
        closing = "]]" if is_array else "]"
        if s.startswith(closing, idx):
            rest = s[idx + len(closing) :].strip(" \t\r\n")
            if rest and not rest.startswith("#"):
                return None
            return (is_array, segments) if segments else None
        return None
    return None


def _is_focusstage_header(parsed):
    if parsed is None:
        return False
    is_array, segments = parsed
    return (not is_array) and len(segments) >= 2 and segments[0] == "mcp_servers" and segments[1] == SERVER_NAME


def _render_codex_block(command, args, newline):
    args_literal = ", ".join(json.dumps(arg, ensure_ascii=False) for arg in args)
    return [
        f"[mcp_servers.{SERVER_NAME}]{newline}",
        f"command = {json.dumps(command, ensure_ascii=False)}{newline}",
        f"args = [{args_literal}]{newline}",
    ]


def _find_focusstage_sections(lines):
    """Return list of (start, body_end, section_end, is_exact_root) for all [mcp_servers.focusstage*] tables."""
    headers = []
    state = None
    for idx, line in enumerate(lines):
        if state is None:
            parsed = _parse_toml_header(line)
            if parsed is not None:
                headers.append((idx, parsed))
        state = _scan_toml_string_state(line, state)

    sections = []
    for h_pos, (start, parsed) in enumerate(headers):
        if not _is_focusstage_header(parsed):
            continue
        section_end = headers[h_pos + 1][0] if h_pos + 1 < len(headers) else len(lines)
        # Keep trailing blank lines and comments that belong to the next section.
        body_end = start + 1
        sub_state = None
        for idx in range(start + 1, section_end):
            raw = lines[idx]
            stripped = raw.strip()
            if sub_state is not None or (stripped and not stripped.startswith("#")):
                body_end = idx + 1
            sub_state = _scan_toml_string_state(raw, sub_state)
        is_exact_root = len(parsed[1]) == 2
        sections.append((start, body_end, section_end, is_exact_root))
    return sections


def update_codex_config(path, command, args):
    try:
        original = path.read_bytes()
    except FileNotFoundError:
        if command is None:
            return
        original = None

    text = original.decode("utf-8-sig") if original is not None else ""
    parsed_doc = None
    if tomllib is not None and text.strip():
        try:
            parsed_doc = tomllib.loads(text)
        except tomllib.TOMLDecodeError as error:
            raise ValueError(f"Codex config ({path}) is not valid TOML: {error}") from error

    newline = "\r\n" if "\r\n" in text else "\n"
    lines = text.splitlines(keepends=True)
    sections = _find_focusstage_sections(lines)

    if command is None:
        if not sections:
            if parsed_doc and SERVER_NAME in (parsed_doc.get("mcp_servers") or {}):
                raise ValueError(
                    f"Codex config ({path}) defines mcp_servers.{SERVER_NAME} inline; remove it manually"
                )
            return
        new_lines = list(lines)
        for start, body_end, _, _ in reversed(sections):
            del new_lines[start:body_end]
            # Collapse duplicate blank lines created by removing the block.
            while (
                start < len(new_lines)
                and not new_lines[start].strip()
                and (start == 0 or not new_lines[start - 1].strip())
            ):
                del new_lines[start]
            while start > 0 and start == len(new_lines) and not new_lines[start - 1].strip():
                del new_lines[start - 1]
                start -= 1
        new_text = "".join(new_lines)
    else:
        desired_block = _render_codex_block(command, args, newline)
        if len(sections) == 1 and sections[0][3]:
            start, body_end, _, _ = sections[0]
            if parsed_doc is not None:
                existing = (parsed_doc.get("mcp_servers") or {}).get(SERVER_NAME)
                if existing == {"command": command, "args": list(args)}:
                    return
            elif lines[start:body_end] == desired_block:
                return

        if not sections and parsed_doc is not None and SERVER_NAME in (parsed_doc.get("mcp_servers") or {}):
            existing = parsed_doc["mcp_servers"][SERVER_NAME]
            if existing == {"command": command, "args": list(args)}:
                return
            raise ValueError(
                f"Codex config ({path}) defines mcp_servers.{SERVER_NAME} inline; update it manually"
            )

        new_lines = list(lines)
        if sections:
            first_start, first_body_end, _, _ = sections[0]
            # Remove any extra/subtable focusstage blocks first from back to front.
            for start, body_end, _, _ in reversed(sections[1:]):
                del new_lines[start:body_end]
                while (
                    start < len(new_lines)
                    and not new_lines[start].strip()
                    and (start == 0 or not new_lines[start - 1].strip())
                ):
                    del new_lines[start]
            new_lines[first_start:first_body_end] = desired_block
        else:
            if new_lines and not new_lines[-1].endswith(("\n", "\r")):
                new_lines[-1] = new_lines[-1] + newline
            if new_lines and new_lines[-1].strip():
                new_lines.append(newline)
            new_lines.extend(desired_block)
        new_text = "".join(new_lines)

    if new_text == text and original is not None:
        return

    if tomllib is not None and new_text.strip():
        try:
            tomllib.loads(new_text)
        except tomllib.TOMLDecodeError as error:
            raise ValueError(f"Refusing to write invalid TOML to {path}: {error}") from error

    atomic_write_text(path, new_text, original, f"Codex config ({path})")


def update_all(windows_home, wsl_home, distro, remove):
    if remove:
        win_cmd, win_args = None, None
        wsl_cmd, wsl_args = None, None
    else:
        win_cmd, win_args = server_spec(distro=distro, native=False)
        wsl_cmd, wsl_args = server_spec(distro=None, native=True)

    update_claude_config(windows_home / ".claude.json", win_cmd, win_args)
    update_codex_config(windows_home / ".codex" / "config.toml", win_cmd, win_args)
    update_claude_config(wsl_home / ".claude.json", wsl_cmd, wsl_args)
    update_codex_config(wsl_home / ".codex" / "config.toml", wsl_cmd, wsl_args)


def main():
    parser = argparse.ArgumentParser(
        description="Register or remove the FocusStage MCP server for Claude Code and Codex."
    )
    parser.add_argument(
        "path",
        nargs="?",
        type=Path,
        help="Target config file path (single-file mode; requires --client).",
    )
    parser.add_argument(
        "--client",
        choices=("claude", "codex"),
        help="Target client format for single-file mode.",
    )
    parser.add_argument(
        "--windows-home",
        type=Path,
        help="WSL path to the Windows user profile directory (updates both Windows and WSL global configs).",
    )
    parser.add_argument(
        "--wsl-home",
        type=Path,
        default=None,
        help="Override WSL user home directory in --windows-home mode (defaults to current $HOME).",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--distro", help="Register Windows wsl.exe bridge targeting this WSL distro.")
    mode.add_argument("--native", action="store_true", help="Register native Linux /usr/local/bin/focusstage command.")
    mode.add_argument("--remove", action="store_true", help="Remove the FocusStage MCP entry.")

    args = parser.parse_args()
    try:
        if args.windows_home is not None:
            if args.path is not None or args.client is not None:
                parser.error("--windows-home cannot be combined with path or --client")
            if args.native:
                parser.error("--windows-home requires --distro or --remove")
            wsl_home = args.wsl_home if args.wsl_home is not None else Path.home()
            update_all(args.windows_home, wsl_home, args.distro, args.remove)
        else:
            if args.path is None or args.client is None:
                parser.error("single-file mode requires both --client and path")
            command, cmd_args = server_spec(args.distro, args.native)
            if args.client == "claude":
                update_claude_config(args.path, command, cmd_args)
            else:
                update_codex_config(args.path, command, cmd_args)
    except (OSError, ValueError) as error:
        parser.exit(1, f"Cannot update MCP config: {error}\n")


if __name__ == "__main__":
    main()
