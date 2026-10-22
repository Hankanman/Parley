import { Easing, interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { c, sans } from "../theme";

/**
 * Kinetic headline: each word springs up out of a blur, staggered.
 * Wrap words in *asterisks* to colour them with the accent.
 */
export const Headline: React.FC<{
  text: string;
  size?: number;
  delay?: number;
  stagger?: number;
  align?: "left" | "center";
  color?: string;
  weight?: number;
}> = ({ text, size = 104, delay = 0, stagger = 3, align = "left", color = c.fg, weight = 800 }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const lines = text.split("\n");
  let index = 0;
  let inAccent = false; // *accent* may span several words and lines
  return (
    <div
      style={{
        fontFamily: sans,
        fontWeight: weight,
        fontSize: size,
        lineHeight: 1.04,
        letterSpacing: "-0.035em",
        color,
        textAlign: align,
      }}
    >
      {lines.map((line, li) => (
        <div key={li} style={{ display: "flex", flexWrap: "wrap", gap: "0 0.26em", justifyContent: align === "center" ? "center" : "flex-start" }}>
          {line.split(" ").map((raw, wi) => {
            if (raw.startsWith("*")) inAccent = true;
            const accent = inAccent;
            if (raw.endsWith("*")) inAccent = false;
            const word = raw.replace(/\*/g, "");
            const start = delay + index++ * stagger;
            return (
              <span
                key={wi}
                style={{
                  display: "inline-block",
                  color: accent ? c.link : undefined,
                  opacity: interpolate(frame, [start, start + 0.3 * fps], [0, 1], {
                    extrapolateLeft: "clamp",
                    extrapolateRight: "clamp",
                  }),
                  translate: interpolate(frame, [start, start + 0.55 * fps], ["0px 0.5em", "0px 0em"], {
                    extrapolateLeft: "clamp",
                    extrapolateRight: "clamp",
                    easing: Easing.bezier(0.16, 1, 0.3, 1),
                  }),
                  filter: `blur(${interpolate(frame, [start, start + 0.35 * fps], [14, 0], {
                    extrapolateLeft: "clamp",
                    extrapolateRight: "clamp",
                  })}px)`,
                }}
              >
                {word}
              </span>
            );
          })}
        </div>
      ))}
    </div>
  );
};

export const Sub: React.FC<{ text: string; delay?: number; size?: number; align?: "left" | "center" }> = ({
  text,
  delay = 10,
  size = 40,
  align = "left",
}) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <div
      style={{
        fontFamily: sans,
        fontSize: size,
        lineHeight: 1.35,
        color: c.fgMuted,
        textAlign: align,
        marginTop: 28,
        opacity: interpolate(frame, [delay, delay + 0.5 * fps], [0, 1], {
          extrapolateLeft: "clamp",
          extrapolateRight: "clamp",
        }),
        translate: interpolate(frame, [delay, delay + 0.6 * fps], ["0px 24px", "0px 0px"], {
          extrapolateLeft: "clamp",
          extrapolateRight: "clamp",
          easing: Easing.bezier(0.16, 1, 0.3, 1),
        }),
      }}
    >
      {text}
    </div>
  );
};
