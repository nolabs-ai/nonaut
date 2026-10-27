use serde_json::{Value, json};

pub(crate) fn specs() -> Value {
    json!([
        {
            "type": "function", "name": "read_file",
            "description": "Attempt to read a UTF-8 text file, optionally limiting the inclusive line range. Pass the requested path to nono and let nono decide whether access is allowed; do not predict policy.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string","description":"File path to attempt. Relative paths start at the workspace; nono enforces access."},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false}
        },
        {
            "type": "function", "name": "list_directory",
            "description": "Attempt to list a directory. Pass the requested path to nono even when it appears outside the workspace; nono alone decides whether access is allowed.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string","description":"Directory path to attempt; use . for the workspace root. Nono enforces access."},"depth":{"type":"integer","minimum":1,"maximum":8}},"required":["path"],"additionalProperties":false}
        },
        {
            "type": "function", "name": "write_file",
            "description": "Create or replace one UTF-8 text file and return a compact unified diff. Always call this tool for direct file creation or full-content replacement, including paths that appear outside the workspace. Do not predict policy; nono alone decides whether access is allowed.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string","description":"File path to attempt. Relative paths start at the workspace; Nonaut normalizes it and nono enforces access."},"content":{"type":"string","description":"Complete replacement file contents."}},"required":["path","content"],"additionalProperties":false}
        },
        {
            "type": "function", "name": "search_files",
            "description": "Attempt to search text files with ripgrep. The query is a regular expression. Pass the requested path to nono and let nono decide whether access is allowed.",
            "inputSchema": {"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string","description":"Search root to attempt; defaults to . and nono enforces access."},"glob":{"type":"string","description":"Optional ripgrep glob, such as *.rs."}},"required":["query"],"additionalProperties":false}
        },
        {
            "type": "function", "name": "edit_file",
            "description": "Edit a UTF-8 text file by replacing exact text and return a compact unified diff. Call this after at most one read when the user requests a focused patch or edit. Nono alone decides whether access is allowed.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string","description":"File path to attempt. Relative paths start at the workspace; nono enforces access."},"old_text":{"type":"string","description":"Exact existing text to replace."},"new_text":{"type":"string","description":"Replacement text."},"replace_all":{"type":"boolean","description":"Replace every occurrence instead of requiring exactly one; defaults to false."}},"required":["path","old_text","new_text"],"additionalProperties":false}
        },
        {
            "type": "function", "name": "delete_file",
            "description": "Delete one file and return a compact unified diff. Always call this tool for an explicitly requested deletion, even when the path appears outside the workspace. Nono alone decides whether access is allowed.",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string","description":"File path to attempt. Relative paths start at the workspace; nono enforces access."}},"required":["path"],"additionalProperties":false}
        },
        {
            "type": "function", "name": "run_command",
            "description": "Run a program under the policy selected by its explicit working directory. Always make an explicitly requested attempt and report the actual result instead of predicting a denial. The fresh nono sandbox grants only the matching workspace, its network policy, and no inherited secrets.",
            "inputSchema": {"type":"object","properties":{"program":{"type":"string","description":"Executable name, such as cargo, git, or sh."},"args":{"type":"array","items":{"type":"string"}},"cwd":{"type":"string","description":"Working directory used to select the operation's workspace policy. Relative paths start at the session directory."},"timeout_seconds":{"type":"integer","minimum":1,"description":"Optional; capped by the user's tool timeout (default 120 seconds)."}},"required":["program","args","cwd"],"additionalProperties":false}
        }
    ])
}
