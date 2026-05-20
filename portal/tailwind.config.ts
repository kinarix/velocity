import type { Config } from "tailwindcss";

export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      fontFamily: {
        mono: [
          "ui-monospace",
          "JetBrains Mono",
          "SFMono-Regular",
          "Menlo",
          "Consolas",
          "monospace",
        ],
      },
      colors: {
        // Dark theme + amber accent, per phases.md
        ink: {
          950: "#0a0a0b",
          900: "#0f1012",
          800: "#15171b",
          700: "#1d2026",
          600: "#262a32",
          500: "#3a3f48",
          400: "#5a606b",
          300: "#8a8f99",
          200: "#b8bcc4",
          100: "#dfe2e7",
        },
        amber: {
          500: "#f59e0b",
          400: "#fbbf24",
          300: "#fcd34d",
        },
      },
    },
  },
  plugins: [],
} satisfies Config;
