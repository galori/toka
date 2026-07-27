import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import reactRefresh from "eslint-plugin-react-refresh";
import tseslint from "typescript-eslint";

const browserGlobals = {
  ...globals.browser,
  console: "readonly",
  document: "readonly",
  HTMLMediaElement: "readonly",
  KeyboardEvent: "readonly",
  localStorage: "readonly",
  MouseEvent: "readonly",
  setInterval: "readonly",
  clearInterval: "readonly",
  setTimeout: "readonly",
  clearTimeout: "readonly",
  window: "readonly",
};

const nodeGlobals = {
  ...globals.node,
  Buffer: "readonly",
  console: "readonly",
  NodeJS: "readonly",
  process: "readonly",
  setTimeout: "readonly",
  clearTimeout: "readonly",
};

export default tseslint.config(
  {
    ignores: [
      "dist/",
      "design/",
      "logs/",
      "node_modules/",
      "src-tauri/gen/",
      "src-tauri/target/",
      ".worktrees/",
    ],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ["src/**/*.{ts,tsx}", "vite.config.ts"],
    languageOptions: {
      globals: browserGlobals,
    },
    plugins: {
      "react-hooks": reactHooks,
      "react-refresh": reactRefresh,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      "react-refresh/only-export-components": "off",
      "react-hooks/immutability": "off",
      "react-hooks/set-state-in-effect": "off",
      "react-hooks/exhaustive-deps": "off",
      "react-hooks/preserve-manual-memoization": "off",
      "react-hooks/preserve-caught-error": "off",
      "preserve-caught-error": "off",
      "@typescript-eslint/preserve-caught-error": "off",
      "@typescript-eslint/no-unused-vars": [
        "error",
        { argsIgnorePattern: "^(args|_)" },
      ],
    },
  },
  {
    files: ["test/**/*.ts", "wdio*.ts"],
    languageOptions: {
      globals: {
        ...browserGlobals,
        ...nodeGlobals,
        browser: "readonly",
        describe: "readonly",
        expect: "readonly",
        it: "readonly",
      },
    },
    rules: {
      "preserve-caught-error": "off",
      "@typescript-eslint/only-throw-error": "off",
    },
  },
  {
    files: ["scripts/**/*.{js,mjs}", "bin/**/*.mjs"],
    languageOptions: {
      globals: nodeGlobals,
    },
    rules: {
      "no-useless-assignment": "off",
      "no-useless-escape": "off",
    },
  },
);
