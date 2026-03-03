# Task: Expose terminal surface working directories via CLI/socket protocol

## Context

cmux already tracks working directories for terminal surfaces internally:
- `TerminalPanel.directory` (`Sources/Panels/TerminalPanel.swift`, line 22) — `@Published var directory: String`
- `Workspace.panelDirectories` (`Sources/Workspace.swift`) — `[UUID: String]` dictionary mapping panel UUIDs to directory paths
- Updated live via `updateDirectory()` in TerminalPanel
- Already used for sidebar git/branch metadata

This data is NOT currently exposed through the socket protocol. An external tool (cmux-controller) needs to match cmux workspaces to git worktrees by directory path, and currently has to guess based on workspace name string matching.

## What to change

Add a `directory` field to the JSON response for terminal surfaces in these V2 socket methods in `Sources/TerminalController.swift`:

1. **`v2PaneSurfaces()`** (around line 4194) — the `pane.surfaces` method. Each surface object in the response should include `"directory": "/path/to/cwd"` when the surface type is `terminal` and a directory is known.

2. **`v2SurfaceList()`** (around line 3102) — the `surface.list` method. Same addition.

3. **`v2Identify()`** (around line 1750) — the `system.identify` method. Include `"directory"` in both the `focused` and `caller` objects when the surface is a terminal.

The directory for a given surface can be looked up via the workspace's `panelDirectories` dictionary using the panel/surface UUID.

## Expected response format

Current `pane.surfaces` response:
```json
{
  "surfaces": [
    {
      "id": "<uuid>",
      "ref": "surface:1",
      "index": 0,
      "title": "Claude Code",
      "type": "terminal",
      "selected": true
    }
  ]
}
```

After change:
```json
{
  "surfaces": [
    {
      "id": "<uuid>",
      "ref": "surface:1",
      "index": 0,
      "title": "Claude Code",
      "type": "terminal",
      "selected": true,
      "directory": "/Users/robert/dev/scratch"
    }
  ]
}
```

Only include `directory` when:
- Surface type is `terminal` (not `browser`)
- A directory is known (omit the field rather than sending null/empty)

## CLI impact

No CLI changes needed — `cmux list-pane-surfaces --json` and `cmux identify --json` will automatically pick up the new fields since they just forward the socket response.

## Testing

```bash
# Should now include directory field for terminal surfaces
cmux list-pane-surfaces --json
cmux identify --json
cmux list-pane-surfaces --workspace workspace:2 --json
```
