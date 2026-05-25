export const AiPodPlugin = async ({ project }) => {
  const url = (process.env.AI_POD_SERVER_URL || "").replace(/\/+$/, "");
  const apiKey = process.env.AI_POD_API_KEY || "";
  const projectId = process.env.AI_POD_PROJECT_ID || "";
  const sessionId = process.env.AI_POD_SESSION_ID || "";

  const post = async (path, body) => {
    if (!url || !apiKey || !projectId) return;
    try {
      await fetch(`${url}${path}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Api-Key": apiKey,
        },
        body: JSON.stringify(body),
      });
    } catch {
      // Best-effort; ignore failures.
    }
  };

  const reportStatus = (status, line) =>
    sessionId
      ? post("/agent_status", {
          project_id: projectId,
          session_id: sessionId,
          status,
          status_line: line,
        })
      : Promise.resolve();

  const title = project?.name || "OpenCode";

  return {
    "session.idle": async () => {
      await post("/notify_user", {
        project_id: projectId,
        message: `${title}: Task completed`,
      });
      await reportStatus("Idle", "Task completed");
    },
    "chat.params": async () => {
      // Fires when OpenCode is about to send a request to the model — the
      // user has just submitted a prompt and work is starting.
      await reportStatus("Running", "Working...");
    },
  };
};
