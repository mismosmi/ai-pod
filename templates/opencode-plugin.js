export default {
  id: "ai-pod",
  setup(ctx) {
    const url = (process.env.AI_POD_SERVER_URL || "").replace(/\/+$/, "");
    const apiKey = process.env.AI_POD_API_KEY || "";
    const projectId = process.env.AI_POD_PROJECT_ID || "";
    if (!url || !apiKey || !projectId) return;

    const controller = new AbortController();
    const title = ctx.location.project.canonical?.split("/").filter(Boolean).pop() || "OpenCode";
    void (async () => {
      for await (const event of ctx.event.subscribe({
        types: ["session.idle"],
        signal: controller.signal,
      })) {
        if (event.type !== "session.idle") continue;
        try {
          await fetch(`${url}/notify_user`, {
            method: "POST",
            signal: controller.signal,
            headers: {
              "Content-Type": "application/json",
              "X-Api-Key": apiKey,
            },
            body: JSON.stringify({
              project_id: projectId,
              message: `${title}: Task completed`,
            }),
          });
        } catch {
          // Best-effort notification; ignore failures.
        }
      }
    })().catch(() => {
      // Stream shutdown (including plugin unload) must not reject unhandled.
    });
    return () => controller.abort();
  },
};
