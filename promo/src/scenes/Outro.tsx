import { AbsoluteFill, Easing, Img, interpolate, Sequence, staticFile, useCurrentFrame, useVideoConfig } from "remotion";
import { Backdrop } from "../components/Backdrop";
import { Headline, Sub } from "../components/Headline";
import { Logo } from "../components/Ui";
import { c, sans } from "../theme";

/** The real screenshots fanned out in 3D. */
export const RealApp: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames } = useVideoConfig();
  const shots = [
    { src: "actions.png", x: -560, rot: 22, delay: 6 },
    { src: "speakers.png", x: 560, rot: -22, delay: 10 },
    { src: "meeting.png", x: 0, rot: 0, delay: 0 },
  ];
  const push = interpolate(frame, [0, durationInFrames], [0, 1]);
  return (
    <Backdrop>
      <AbsoluteFill style={{ alignItems: "center", paddingTop: 90 }}>
        <Headline text={"The *real* app. Native Rust."} size={96} align="center" />
        <Sub text="Built with GPUI, the framework behind the Zed editor. No browser inside." align="center" delay={12} />
      </AbsoluteFill>
      <AbsoluteFill style={{ perspective: 2400, alignItems: "center", justifyContent: "flex-end" }}>
        {shots.map((s) => {
          const enter = interpolate(frame, [s.delay, s.delay + 0.8 * fps], [0, 1], {
            extrapolateLeft: "clamp",
            extrapolateRight: "clamp",
            easing: Easing.bezier(0.16, 1, 0.3, 1),
          });
          return (
            <Img
              key={s.src}
              src={staticFile(s.src)}
              style={{
                position: "absolute",
                bottom: -40,
                width: 1000,
                borderRadius: 18,
                border: "1px solid rgba(255,255,255,0.1)",
                boxShadow: "0 50px 100px rgba(0,0,0,0.7)",
                opacity: enter,
                transform: `translateX(${s.x * enter}px) translateY(${(1 - enter) * 400 - push * 40}px) rotateY(${s.rot * enter}deg) rotateX(14deg) scale(${s.x === 0 ? 1 : 0.86})`,
              }}
            />
          );
        })}
      </AbsoluteFill>
    </Backdrop>
  );
};

/** No cloud. No account. No telemetry. */
export const Privacy: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const lines = ["No cloud.", "No account.", "No telemetry."];
  const beat = Math.round(0.42 * fps);
  return (
    <Backdrop hue={c.green}>
      <AbsoluteFill style={{ justifyContent: "center", paddingLeft: 200, gap: 6 }}>
        {lines.map((line, i) => {
          const start = i * beat;
          const strike = interpolate(frame, [start + 8, start + 16], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" });
          return (
            <div
              key={line}
              style={{
                fontFamily: sans,
                fontWeight: 800,
                fontSize: 150,
                letterSpacing: "-0.045em",
                color: c.fg,
                position: "relative",
                alignSelf: "flex-start",
                opacity: interpolate(frame, [start, start + 4], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp" }),
                translate: `${interpolate(frame, [start, start + 10], [-80, 0], {
                  extrapolateLeft: "clamp",
                  extrapolateRight: "clamp",
                  easing: Easing.bezier(0.16, 1, 0.3, 1),
                })}px 0px`,
              }}
            >
              {line}
              <div style={{ position: "absolute", left: -10, top: "54%", height: 14, borderRadius: 7, width: `calc(${strike * 100}% + 20px)`, background: c.red, opacity: 0.9 }} />
            </div>
          );
        })}
        <Sequence from={lines.length * beat + 6} layout="none">
          <div style={{ marginTop: 40 }}>
            <Headline text={"Just *your machine.*"} size={110} stagger={4} />
          </div>
        </Sequence>
      </AbsoluteFill>
    </Backdrop>
  );
};

export const Outro: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const pop = interpolate(frame, [0, 0.5 * fps], [0, 1], {
    extrapolateRight: "clamp",
    easing: Easing.spring({ damping: 12, stiffness: 140 }),
  });
  const pill = interpolate(frame, [0.8 * fps, 1.3 * fps], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: Easing.bezier(0.16, 1, 0.3, 1),
  });
  return (
    <Backdrop>
      <AbsoluteFill style={{ justifyContent: "center", alignItems: "center", gap: 36 }}>
        <div style={{ scale: String(pop) }}>
          <Logo size={170} />
        </div>
        <Headline text="Parley" size={150} align="center" delay={6} />
        <Sub text="Private AI meeting notes. Free and open source, for Linux." align="center" delay={14} size={42} />
        <div
          style={{
            marginTop: 20,
            padding: "22px 48px",
            borderRadius: 999,
            background: c.primary,
            color: "#fff",
            fontFamily: sans,
            fontWeight: 700,
            fontSize: 52,
            letterSpacing: "-0.01em",
            boxShadow: `0 20px 60px ${c.primary}77`,
            opacity: pill,
            scale: String(0.85 + 0.15 * pill),
          }}
        >
          meetparley.app
        </div>
      </AbsoluteFill>
    </Backdrop>
  );
};
