/**
 * Run against an existing daemon, without any coding agent or model key:
 *   agl start
 *   node examples/local-tools.ts
 * Node 22.18+ can execute this TypeScript directly.
 * For a hub, replace AglClient.local() with a connected client's forMachine(id).
 */
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AglClient } from "../clients/typescript/agl.ts";

const client = await AglClient.local();
const dir = await mkdtemp(join(tmpdir(), "agl-tools-example-"));
try {
  const tools = await client.tools();
  console.log("Available tools:", tools.map((t) => t.name).join(", "));
  // Your cloud model receives these schemas and returns {name, arguments}.
  const path = join(dir, "hello.txt");
  await client.callTool("write", { path, content: "Hello from the cloud!\n" });
  await client.callTool("edit", { path, old_text: "Hello", new_text: "你好" });
  console.log(await client.callTool("read", { path }));

  // Keep this ID and these arguments when retrying after a lost response.
  const { execution_id } = await client.startExec({
    execution_id: crypto.randomUUID(),
    argv: ["/bin/sh", "-c", "cat hello.txt; sleep 0.1; printf done"],
    cwd: dir,
  });
  let stdout_offset = 0, stderr_offset = 0;
  for (;;) {
    const result = await client.readExec(execution_id, { stdout_offset, stderr_offset, wait_ms: 1000 });
    process.stdout.write(result.stdout);
    process.stderr.write(result.stderr);
    stdout_offset = result.stdout_offset;
    stderr_offset = result.stderr_offset;
    if (result.status === "completed" && !result.has_more) {
      if (result.exit_code !== 0 || result.error) throw new Error(JSON.stringify(result));
      console.log("\nCommand finished.");
      break;
    }
  }
} finally {
  client.close();
  await rm(dir, { recursive: true, force: true });
}
