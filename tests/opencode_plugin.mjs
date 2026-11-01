import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

// Load the shipped .js as ESM without adding a package.json to the templates.
const source = await readFile(new URL("../templates/opencode-plugin.js", import.meta.url), "utf8");
const { default: plugin } = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);

test("V2 plugin definition, idle notifications, errors, and cleanup", async () => {
  assert.equal(plugin.id, "ai-pod");
  assert.equal(typeof plugin.setup, "function");
  const previousEnv = { ...process.env };
  const previousFetch = globalThis.fetch;
  const requests = [];
  let subscription;
  let cleanup;
  try {
    Object.assign(process.env, {
      AI_POD_SERVER_URL: "http://ai-pod.test///",
      AI_POD_API_KEY: "test-key",
      AI_POD_PROJECT_ID: "test-project",
    });
    globalThis.fetch = async (url, options) => {
      requests.push({ url, options });
      if (requests.length === 1) throw new Error("network unavailable");
      return { ok: true };
    };
    let finished;
    const done = new Promise((resolve) => { finished = resolve; });
    cleanup = plugin.setup({
      location: { project: { canonical: "/work/example" } },
      event: {
        async *subscribe(options) {
          subscription = options;
          yield { type: "session.created" };
          yield { type: "session.idle" };
          yield { type: "session.idle" };
          finished();
        },
      },
    });
    await done;
    assert.deepEqual(subscription.types, ["session.idle"]);
    assert.equal(requests.length, 2);
    assert.equal(requests[1].url, "http://ai-pod.test/notify_user");
    assert.equal(requests[1].options.method, "POST");
    assert.equal(requests[1].options.headers["X-Api-Key"], "test-key");
    assert.deepEqual(JSON.parse(requests[1].options.body), {
      project_id: "test-project",
      message: "example: Task completed",
    });
    assert.equal(subscription.signal.aborted, false);
    cleanup();
    assert.equal(subscription.signal.aborted, true);
    assert.equal(requests[1].options.signal.aborted, true);

    for (const key of ["AI_POD_SERVER_URL", "AI_POD_API_KEY", "AI_POD_PROJECT_ID"]) {
      const saved = process.env[key];
      delete process.env[key];
      assert.equal(plugin.setup({}), undefined);
      process.env[key] = saved;
    }
  } finally {
    cleanup?.();
    globalThis.fetch = previousFetch;
    process.env = previousEnv;
  }
});
