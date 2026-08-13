// Minimal stdio MCP server forwarding workspace requests to the authenticated
// Rust broker. It intentionally has no filesystem or subprocess permissions.

const brokerUrl = Deno.env.get("FAST_RLM_AGENT_BROKER_URL")!;
const brokerToken = Deno.env.get("FAST_RLM_AGENT_BROKER_TOKEN")!;
const encoder = new TextEncoder();

const tools = [
  {
    name: "read_file",
    description: "Read a UTF-8 file inside the harness workspace. Read-only.",
    inputSchema: {
      type: "object",
      properties: { path: { type: "string" } },
      required: ["path"],
    },
  },
  {
    name: "write_file",
    description: "Create or overwrite a workspace file after user approval.",
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
    description: "Replace exact text in a workspace file after user approval.",
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
    name: "run_command",
    description: "Run a shell command in the workspace after user approval.",
    inputSchema: {
      type: "object",
      properties: { command: { type: "string" } },
      required: ["command"],
    },
  },
];

async function callTool(name: string, args: Record<string, unknown>) {
  const response = await fetch(brokerUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: brokerToken, tool: name, ...args }),
  });
  const result = await response.json();
  return {
    content: [{ type: "text", text: result.text }],
    structuredContent: result,
    isError: !result.ok,
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
    if (line) await respond(JSON.parse(line));
  }
}
