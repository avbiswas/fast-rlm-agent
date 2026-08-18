// Minimal stdio MCP server forwarding workspace requests to the authenticated
// Rust broker. It intentionally has no filesystem or subprocess permissions.

const brokerUrl = Deno.env.get("FAST_RLM_AGENT_BROKER_URL")!;
const brokerToken = Deno.env.get("FAST_RLM_AGENT_BROKER_TOKEN")!;
const encoder = new TextEncoder();

const tools = [
  {
    name: "read_file",
    description: "Read a UTF-8 file from the real project workspace on the host. Read-only. This is the ONLY way to see workspace files: your Python REPL runs in a separate sandbox with its own empty filesystem.",
    inputSchema: {
      type: "object",
      properties: { path: { type: "string" } },
      required: ["path"],
    },
  },
  {
    name: "write_file",
    description: "Create or overwrite a file in the real project workspace on the host, after user approval. Writing with Python open() instead only writes to the REPL sandbox and does NOT touch the project.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string" },
        content: { type: "string" },
      },
      required: ["path", "content"],
    },
  },
  {
    name: "edit_file",
    description: "Replace exact text in a real workspace file after user approval. If old_string is not found or is ambiguous, the tool returns an explanatory message you can act on — it does not raise.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string" },
        old_string: { type: "string" },
        new_string: { type: "string" },
        replace_all: { type: "boolean", default: false },
      },
      required: ["path", "old_string", "new_string"],
    },
  },
  {
    name: "bash",
    description: "Run a general Bash command in the real project workspace after user approval. Commands default to a 120-second timeout. A non-zero exit is returned normally as text ending in \"(exit N)\" — it is NOT an exception, so you can inspect it and continue in the same REPL cell.",
    inputSchema: {
      type: "object",
      properties: {
        command: { type: "string" },
        timeout_seconds: { type: "integer", minimum: 1, maximum: 1800 },
      },
      required: ["command"],
    },
  },
  {
    name: "skill",
    description: "List workspace AGENTS.md/CLAUDE.md instructions and SKILL.md files, or read one listed document by path. Call without path to discover available documents.",
    inputSchema: {
      type: "object",
      properties: { path: { type: "string" } },
    },
  },
];

async function callTool(name: string, args: Record<string, unknown>) {
  let result: { ok?: boolean; is_error?: boolean; text?: string };
  try {
    const response = await fetch(brokerUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ token: brokerToken, tool: name, ...args }),
    });
    result = await response.json();
  } catch (error) {
    // Never let a broker failure escape: an exception here would unwind the
    // stdio loop and kill this server, so every later tool call in the run
    // would fail with "Connection closed" instead of just this one.
    const detail = error instanceof Error ? error.message : String(error);
    result = { ok: false, is_error: true, text: `workspace tool '${name}' failed: ${detail}` };
  }
  // Deliberately no `structuredContent`: FastRLM's mcp_call returns it in
  // preference to the text, so exposing {ok, is_error, text} would make
  // read_file hand back a dict instead of the file's contents. Agents then
  // slice or splitlines() it and get the dict's repr — observed at both the
  // root and sub-agent level. `ok` is already legible in the text (bash ends
  // in "(exit N)", failures start with "ERROR:"), and isError below is a
  // separate top-level MCP field, so nothing is lost by omitting it.
  return {
    content: [{ type: "text", text: result.text }],
    // Only transport/protocol failures raise in the agent's REPL. A tool that
    // ran and returned a message the model can act on (non-zero exit, missing
    // file, unmatched old_string) comes back as a normal result, so a single
    // recoverable failure no longer discards the rest of the REPL cell.
    isError: result.is_error === true,
  };
}

async function respond(message: any) {
  if (message.id === undefined) return;
  let result: unknown;
  if (message.method === "initialize") {
    result = {
      protocolVersion: message.params?.protocolVersion ?? "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: "fast-rlm-agent-workspace", version: "0.1.0" },
    };
  } else if (message.method === "tools/list") {
    result = { tools };
  } else if (message.method === "tools/call") {
    result = await callTool(message.params.name, message.params.arguments ?? {});
  } else if (message.method === "ping") {
    result = {};
  } else {
    const error = { jsonrpc: "2.0", id: message.id, error: { code: -32601, message: "Method not found" } };
    await Deno.stdout.write(encoder.encode(JSON.stringify(error) + "\n"));
    return;
  }
  await Deno.stdout.write(encoder.encode(JSON.stringify({ jsonrpc: "2.0", id: message.id, result }) + "\n"));
}

let pending = "";
for await (const chunk of Deno.stdin.readable.pipeThrough(new TextDecoderStream())) {
  pending += chunk;
  let newline;
  while ((newline = pending.indexOf("\n")) >= 0) {
    const line = pending.slice(0, newline).trim();
    pending = pending.slice(newline + 1);
    if (!line) continue;
    // Same reasoning as callTool: a malformed line or a failed respond() must
    // not tear down the server and take every remaining tool call with it.
    try {
      await respond(JSON.parse(line));
    } catch (error) {
      console.error(`workspace mcp: dropped a bad message: ${error}`);
    }
  }
}
