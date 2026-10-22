import { loadFont as loadInter } from "@remotion/google-fonts/Inter";
import { loadFont as loadMono } from "@remotion/google-fonts/JetBrainsMono";

export const { fontFamily: sans } = loadInter("normal", {
  weights: ["400", "500", "600", "700", "800"],
  subsets: ["latin"],
});
export const { fontFamily: mono } = loadMono("normal", {
  weights: ["400", "500"],
  subsets: ["latin"],
});

// Parley Dark (parley-gpui/src/theme/parley.json)
export const c = {
  bg: "#0B0E14",
  raised: "#121722",
  muted: "#161C27",
  sidebar: "#0E121A",
  border: "#1F2633",
  fg: "#E6EAF2",
  fgMuted: "#8A94A6",
  primary: "#1A78E8",
  link: "#6FB0FF",
  green: "#2FBF71",
  red: "#F04F5A",
  yellow: "#F5A524",
  magenta: "#B57BF2",
  cyan: "#38BDF8",
};

export const people = {
  alex: { name: "Alex Morgan", initials: "AM", color: c.link },
  priya: { name: "Priya Shah", initials: "PS", color: c.green },
  marcus: { name: "Marcus Chen", initials: "MC", color: c.cyan },
  elena: { name: "Elena Novak", initials: "EN", color: c.magenta },
  sam: { name: "Sam Okafor", initials: "SO", color: c.yellow },
};

export type Person = keyof typeof people;

// Deterministic pseudo-noise for meters and waveforms.
export const noise = (seed: number, t: number) =>
  0.5 +
  0.25 * Math.sin(t * 0.31 + seed * 1.7) +
  0.15 * Math.sin(t * 0.73 + seed * 4.1) +
  0.1 * Math.sin(t * 1.9 + seed * 9.3);
