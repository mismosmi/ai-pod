export const AiPodPlugin = async ({ project, $ }) => {
  return {
    "session.idle": async () => {
      try {
        const title = project?.name || "OpenCode";
        await $`/usr/local/bin/host-tools notify-user "${title}: Task completed"`;
      } catch {
        // Best-effort notification; ignore failures.
      }
    },
  };
};
