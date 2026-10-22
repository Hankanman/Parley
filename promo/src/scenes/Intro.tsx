import { AbsoluteFill, Easing, interpolate, Sequence, useCurrentFrame, useVideoConfig } from "remotion";
import { Backdrop } from "../components/Backdrop";
import { Headline } from "../components/Headline";
import { Logo } from "../components/Ui";
import { c, sans } from "../theme";

/** Fast word swaps: Every meeting. Recorded. Transcribed. Summarised. */
export const Hook: React.FC = () => {
  const { fps } = useVideoConfig();
  const beats = ["Every meeting.", "Recorded.", "Transcribed.", "Summarised."];
  const beat = Math.round(0.5 * fps);
  return (
    <Backdrop>
      {beats.map((word, i) => (
        <Sequence key={word} name={word} from={i * beat} durationInFrames={beat}>
          <Swap text={word} />
        </Sequence>
      ))}
      <Sequence name="Payoff" from={beats.length * beat}>
        <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
          <Headline text={"Without leaving\n*your machine.*"} size={150} align="center" stagger={4} />
        </AbsoluteFill>
      </Sequence>
    </Backdrop>
  );
};

const Swap: React.FC<{ text: string }> = ({ text }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <AbsoluteFill style={{ justifyContent: "center", alignItems: "center" }}>
      <div
        style={{
          fontFamily: sans,
          fontWeight: 800,
          fontSize: 190,
          letterSpacing: "-0.045em",
          color: c.fg,
          scale: interpolate(frame, [0, 0.25 * fps], [1.25, 1], {
            extrapolateRight: "clamp",
            easing: Easing.bezier(0.16, 1, 0.3, 1),
            output: "perceptual-scale",
          }),
          opacity: interpolate(frame, [0, 3], [0, 1], { extrapolateRight: "clamp" }),
          filter: `blur(${interpolate(frame, [0, 5], [10, 0], { extrapolateRight: "clamp" })}px)`,
        }}
      >
        {text}
      </div>
    </AbsoluteFill>
  );
};

/** The icon pops in with bouncing bars, then the wordmark slides out of it. */
export const LogoReveal: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const pop = interpolate(frame, [0, 0.5 * fps], [0, 1], {
    extrapolateRight: "clamp",
    easing: Easing.spring({ damping: 12, stiffness: 140 }),
  });
  const slide = interpolate(frame, [0.35 * fps, 0.9 * fps], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
    easing: Easing.bezier(0.16, 1, 0.3, 1),
  });
  return (
    <Backdrop>
      <AbsoluteFill style={{ justifyContent: "center", alignItems: "center", flexDirection: "row", gap: 48 * slide }}>
        <div style={{ scale: String(pop), rotate: `${(1 - pop) * -25}deg` }}>
          <Logo size={220} />
        </div>
        <div style={{ overflow: "hidden", width: 560 * slide }}>
          <div
            style={{
              fontFamily: sans,
              fontWeight: 800,
              fontSize: 200,
              letterSpacing: "-0.045em",
              color: c.fg,
              translate: `${(1 - slide) * -300}px 0px`,
              opacity: slide,
            }}
          >
            Parley
          </div>
        </div>
      </AbsoluteFill>
    </Backdrop>
  );
};
