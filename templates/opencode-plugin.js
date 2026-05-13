export const AiPodPlugin = async ({ project }) => {
  const url = (process.env.AI_POD_SERVER_URL || "").replace(/\/+$/, "");
  const apiKey = process.env.AI_POD_API_KEY || "";
  const projectId = process.env.AI_POD_PROJECT_ID || "";
  return {
    "session.idle": async () => {
      if (!url || !apiKey || !projectId) return;
      const title = project?.name || "OpenCode";
      try {
        await fetch(`${url}/notify_user`, {
          method: "POST",
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
    },
  };
};
