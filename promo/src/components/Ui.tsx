import { Easing, interpolate, useCurrentFrame, useVideoConfig } from "remotion";
import { c, people, Person, sans } from "../theme";

/** A Parley-styled panel that swings in with a 3D tilt and keeps drifting. */
export const Panel: React.FC<{
  width: number;
  height: number;
  tilt?: number;
  delay?: number;
  children: React.ReactNode;
  title?: string;
}> = ({ width, height, tilt = -10, delay = 0, children, title }) => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames } = useVideoConfig();
  const enter = interpolate(frame, [delay, delay + 0.9 * fps], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: Easing.bezier(0.16, 1, 0.3, 1),
  });
  const drift = interpolate(frame, [0, durationInFrames], [0, 1]);
  return (
    <div style={{ perspective: 2200 }}>
      <div
        style={{
          width,
          height,
          borderRadius: 22,
          background: c.bg,
          border: `1px solid ${c.border}`,
          boxShadow: `0 60px 120px rgba(0,0,0,0.6), 0 0 0 1px rgba(255,255,255,0.04), 0 0 80px ${c.primary}22`,
          overflow: "hidden",
          fontFamily: sans,
          color: c.fg,
          opacity: enter,
          transform: `translateY(${(1 - enter) * 120}px) rotateY(${tilt * (1 - enter) * 2.2 + tilt * (0.5 - drift) * 0.5}deg) rotateX(${(1 - enter) * 12 + 3}deg) scale(${0.9 + enter * 0.1})`,
          transformOrigin: "center",
          display: "flex",
          flexDirection: "column",
        }}
      >
        {title !== undefined ? (
          <div
            style={{
              height: 64,
              flexShrink: 0,
              display: "flex",
              alignItems: "center",
              gap: 12,
              padding: "0 26px",
              borderBottom: `1px solid ${c.border}`,
              background: c.sidebar,
            }}
          >
            {[c.red, c.yellow, c.green].map((col) => (
              <div key={col} style={{ width: 14, height: 14, borderRadius: 7, background: col, opacity: 0.75 }} />
            ))}
            <div style={{ marginLeft: 16, fontSize: 24, color: c.fgMuted, fontWeight: 500 }}>{title}</div>
          </div>
        ) : null}
        <div style={{ flex: 1, position: "relative", overflow: "hidden" }}>{children}</div>
      </div>
    </div>
  );
};

export const Avatar: React.FC<{ who: Person; size?: number }> = ({ who, size = 48 }) => {
  const p = people[who];
  return (
    <div
      style={{
        width: size,
        height: size,
        borderRadius: size,
        background: `${p.color}26`,
        color: p.color,
        display: "grid",
        placeItems: "center",
        fontSize: size * 0.38,
        fontWeight: 600,
        fontFamily: sans,
        flexShrink: 0,
      }}
    >
      {p.initials}
    </div>
  );
};

/** Mouse pointer gliding between points; `clickAt` frames make it press. */
export const Cursor: React.FC<{
  path: { frame: number; x: number; y: number }[];
  clickAt?: number[];
}> = ({ path, clickAt = [] }) => {
  const frame = useCurrentFrame();
  const frames = path.map((p) => p.frame);
  const opts = {
    extrapolateLeft: "clamp" as const,
    extrapolateRight: "clamp" as const,
    easing: Easing.bezier(0.45, 0, 0.2, 1),
  };
  const x = interpolate(frame, frames, path.map((p) => p.x), opts);
  const y = interpolate(frame, frames, path.map((p) => p.y), opts);
  const press = clickAt.reduce(
    (s, f) => Math.min(s, interpolate(frame, [f - 3, f, f + 5], [1, 0.8, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" })),
    1,
  );
  const ring = clickAt
    .map((f) => interpolate(frame, [f, f + 14], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" }))
    .find((v) => v > 0 && v < 1);
  return (
    <div style={{ position: "absolute", left: x, top: y, pointerEvents: "none", zIndex: 50 }}>
      {ring !== undefined ? (
        <div
          style={{
            position: "absolute",
            left: -36 * ring - 4,
            top: -36 * ring - 4,
            width: 72 * ring + 8,
            height: 72 * ring + 8,
            borderRadius: "50%",
            border: `3px solid ${c.link}`,
            opacity: 1 - ring,
          }}
        />
      ) : null}
      <svg width="46" height="46" viewBox="0 0 24 24" style={{ scale: String(press), transformOrigin: "0 0", filter: "drop-shadow(0 4px 8px rgba(0,0,0,.6))" }}>
        <path d="M4 2l15 10.5-6.6 1.3 3.9 7.2-2.9 1.5-3.8-7.2L4 20z" fill="#fff" stroke="#0B0E14" strokeWidth="1.4" strokeLinejoin="round" />
      </svg>
    </div>
  );
};

/** The Parley app icon, with its five bars bouncing like a level meter. */
export const Logo: React.FC<{ size: number; live?: boolean }> = ({ size, live = true }) => {
  const frame = useCurrentFrame();
  const base = [0.42, 0.7, 1, 0.66, 0.48];
  return (
    <div
      style={{
        width: size,
        height: size,
        borderRadius: size * 0.23,
        background: c.primary,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        gap: size * 0.055,
        boxShadow: `0 ${size * 0.15}px ${size * 0.4}px ${c.primary}66`,
      }}
    >
      {base.map((h, i) => {
        const wobble = live ? 0.75 + 0.25 * Math.sin(frame * 0.35 + i * 1.3) : 1;
        return (
          <div
            key={i}
            style={{
              width: size * 0.085,
              height: size * 0.6 * h * wobble,
              borderRadius: size,
              background: "#fff",
            }}
          />
        );
      })}
    </div>
  );
};

export const fadeUp = (frame: number, start: number, fps: number) => ({
  opacity: interpolate(frame, [start, start + 0.35 * fps], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" }),
  translate: interpolate(frame, [start, start + 0.5 * fps], ["0px 28px", "0px 0px"], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: Easing.bezier(0.16, 1, 0.3, 1),
  }),
});

export const useFps = () => useVideoConfig().fps;
