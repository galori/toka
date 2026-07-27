export type BuildInfo = {
  version: string;
  builtAt: string;
  gitSha: string;
};

export const buildInfo: BuildInfo = {
  version: import.meta.env.VITE_APP_VERSION || "0.1.0",
  builtAt: import.meta.env.VITE_BUILD_TIME || "development build",
  gitSha: import.meta.env.VITE_GIT_SHA || "development",
};
