import { AbsoluteFill, useCurrentFrame, useVideoConfig } from "remotion";
import { c, sans } from "../theme";

/** Dark canvas with a slowly drifting blue glow and a faint dot grid. */
export const Backdrop: React.FC<{ hue?: string; children?: React.ReactNode }> = ({
  hue = c.primary,
  children,
}) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const t = frame / fps;
  const x = 50 + Math.sin(t * 0.5) * 18;
  const y = 70 + Math.cos(t * 0.4) * 12;
  return (
    <AbsoluteFill style={{ backgroundColor: c.bg, overflow: "hidden", fontFamily: sans }}>
      <AbsoluteFill
        style={{
          background: `radial-gradient(1100px 700px at ${x}% ${y}%, ${hue}55, transparent 70%)`,
        }}
      />
      <AbsoluteFill
        style={{
          backgroundImage: `radial-gradient(${c.border} 1.5px, transparent 1.5px)`,
          backgroundSize: "36px 36px",
          opacity: 0.55,
          maskImage: "radial-gradient(ellipse at center, black 30%, transparent 80%)",
        }}
      />
      {children}
    </AbsoluteFill>
  );
};
